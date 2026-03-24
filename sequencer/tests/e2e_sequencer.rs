// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::io::ErrorKind;
use std::time::Duration;

use alloy_primitives::{Address, Signature, U256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use app_core::application::{
    MAX_METHOD_PAYLOAD_BYTES, Method, WalletApp, WalletConfig, Withdrawal,
};
use futures_util::StreamExt;
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::hazmat::PrehashSigner;
use sequencer::api::{self, ApiConfig};
use sequencer::inclusion_lane::{
    InclusionLane, InclusionLaneConfig, InclusionLaneError, PendingUserOp,
};
use sequencer::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use sequencer::shutdown::ShutdownSignal;
use sequencer::storage::{SafeInputRange, Storage, StoredSafeInput};
use sequencer_core::api::{TxRequest, TxResponse, WsTxMessage};
use sequencer_core::l2_tx::SequencedL2Tx;
use sequencer_core::user_op::UserOp;
use sequencer_rust_client::SequencerClient;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_submit_tx_ack_and_broadcast() {
    let db = temp_db("full-e2e");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint.clone(), Duration::from_secs(2))
        .expect("build sequencer client");
    let ws_url = client.ws_subscribe_url(0);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(ws_url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    let method = Method::Withdrawal(Withdrawal {
        amount: U256::from(0_u64),
    });
    let user_op = UserOp {
        nonce: 0,
        max_fee: 0,
        data: ssz::Encode::as_ssz_bytes(&method).into(),
    };
    let signature_hex = sign_user_op_hex(&domain, &user_op, &signing_key);

    let request_body = TxRequest {
        message: user_op,
        signature: signature_hex,
        sender: sender.to_string(),
    };

    let (status, response_body) = client
        .submit_tx_with_status(&request_body)
        .await
        .expect("submit tx");
    assert_eq!(
        status, 200,
        "submit tx should succeed: body={response_body}"
    );

    let response: TxResponse =
        serde_json::from_str(response_body.as_str()).expect("parse response");
    assert!(response.ok);
    assert_eq!(response.nonce, 0);
    assert_eq!(response.sender, sender.to_string());

    let first_message = recv_ws_message(&mut ws).await;
    match first_message {
        WsTxMessage::UserOp {
            offset,
            sender: ws_sender,
            fee,
            data,
        } => {
            assert_eq!(offset, 0);
            assert_eq!(ws_sender, sender.to_string());
            assert_eq!(fee, 0);
            assert_eq!(
                decode_hex_prefixed(data.as_str()),
                ssz::Encode::as_ssz_bytes(&method)
            );
        }
        value => panic!("expected user_op at offset 0, got {value:?}"),
    }

    drop(ws);
    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_signature_with_wrong_hex_length() {
    let db = temp_db("bad-signature-hex-len");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    let mut request = make_valid_request(&domain);
    request.signature = "0xdeadbeef".to_string();
    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");

    assert_eq!(
        status, 400,
        "unexpected status for bad signature len: {body}"
    );
    assert!(
        body.contains("signature must be"),
        "expected signature length message, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_sender_with_wrong_hex_length() {
    let db = temp_db("bad-sender-hex-len");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    let mut request = make_valid_request(&domain);
    request.sender = "0x1234".to_string();
    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");

    assert_eq!(status, 400, "unexpected status for bad sender len: {body}");
    assert!(
        body.contains("sender must be"),
        "expected sender length message, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_oversized_json_body_before_parsing() {
    let db = temp_db("oversized-json");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) = start_full_server_with_max_body(db.path.as_str(), domain, 256).await else {
        return;
    };

    let oversized_json = format!(
        r#"{{"message":{{"nonce":0,"max_fee":0,"data":"0x"}},"signature":"0x{}","sender":null,"pad":"{}"}}"#,
        "00".repeat(65),
        " ".repeat(4096)
    );
    let (status, body) = post_raw_json(runtime.addr, oversized_json.as_str()).await;

    assert_eq!(
        status, 413,
        "unexpected status for oversized JSON body: {body}"
    );
    assert!(
        body.contains("PAYLOAD_TOO_LARGE"),
        "expected payload-too-large error code, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_malformed_json_as_bad_request() {
    let db = temp_db("malformed-json");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) = start_full_server_with_max_body(db.path.as_str(), domain, 128 * 1024).await
    else {
        return;
    };

    let malformed_json =
        r#"{"message":{"nonce":0,"max_fee":0,"data":"0x"},"signature":"0x00","sender":"0x1234""#;
    let (status, body) = post_raw_json(runtime.addr, malformed_json).await;

    assert_eq!(
        status, 400,
        "unexpected status for malformed JSON body: {body}"
    );
    assert!(
        body.contains("BAD_REQUEST"),
        "expected bad-request error code, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_returns_429_when_queue_is_full() {
    let db = temp_db("queue-full-overload");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) =
        start_api_only_server(db.path.as_str(), domain.clone(), 128 * 1024, 1).await
    else {
        return;
    };

    let request = make_valid_request(&domain);
    let request_json = serde_json::to_string(&request).expect("serialize valid request");
    let first = tokio::spawn({
        let body = request_json.clone();
        let addr = runtime.addr;
        async move { post_raw_json(addr, body.as_str()).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (status, body) = post_raw_json(runtime.addr, request_json.as_str()).await;
    assert_eq!(status, 429, "expected 429 for queue-full overload: {body}");
    assert!(
        body.contains("queue full"),
        "expected queue full message in overload response: {body}"
    );
    assert!(
        body.contains("OVERLOADED"),
        "expected OVERLOADED code for queue-full overload: {body}"
    );

    first.abort();
    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_user_op_payloads_above_application_limit() {
    let db = temp_db("user-op-payload-too-large");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    let user_op = UserOp {
        nonce: 0,
        max_fee: 0,
        data: vec![0_u8; MAX_METHOD_PAYLOAD_BYTES + 1].into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(&domain, &user_op, &signing_key),
        sender: sender.to_string(),
        message: user_op,
    };

    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit oversized tx");

    assert_eq!(
        status, 400,
        "unexpected status for oversized payload: {body}"
    );
    assert!(
        body.contains("user op payload too large"),
        "expected payload-size validation message, got: {body}"
    );
    assert!(
        body.contains(&MAX_METHOD_PAYLOAD_BYTES.to_string()),
        "expected max payload size in error body, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_replays_same_ordered_l2_tx_stream_from_db() {
    let db = temp_db("restart-replay-golden");
    let domain = test_domain();
    seed_safe_direct_input(db.path.as_str(), 10, vec![0xaa]);

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint.clone(), Duration::from_secs(2))
        .expect("build sequencer client");
    let ws_url = client.ws_subscribe_url(0);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(ws_url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    let first_live = recv_ws_message(&mut ws).await;

    let request = make_valid_request(&domain);
    let (status, response_body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");
    assert_eq!(
        status, 200,
        "submit tx should succeed before restart: body={response_body}"
    );

    let second_live = recv_ws_message(&mut ws).await;
    drop(ws);

    let expected = load_all_ordered_l2_txs(db.path.as_str());
    assert_eq!(
        expected.len(),
        2,
        "expected one direct input and one user op"
    );
    assert_ws_message_matches_tx(first_live, &expected[0], 0);
    assert_ws_message_matches_tx(second_live, &expected[1], 1);

    shutdown_runtime(runtime).await;

    let Some(restarted) = start_full_server(db.path.as_str(), domain).await else {
        return;
    };

    let restarted_endpoint = format!("http://{}", restarted.addr);
    let restarted_client =
        SequencerClient::new_with_timeout(restarted_endpoint, Duration::from_secs(2))
            .expect("build sequencer client after restart");
    let restarted_ws_url = restarted_client.ws_subscribe_url(0);
    let (mut restarted_ws, _) =
        tokio::time::timeout(Duration::from_secs(5), connect_async(restarted_ws_url))
            .await
            .expect("timeout connecting websocket after restart")
            .expect("connect websocket after restart");

    for (offset, expected_tx) in expected.iter().enumerate() {
        let replayed = recv_ws_message(&mut restarted_ws).await;
        assert_ws_message_matches_tx(replayed, expected_tx, offset as u64);
    }
    drop(restarted_ws);

    shutdown_runtime(restarted).await;
}

struct FullServerRuntime {
    addr: std::net::SocketAddr,
    shutdown: ShutdownSignal,
    server_task: Option<api::ApiServerTask>,
    lane_handle:
        Option<tokio::task::JoinHandle<Result<(), sequencer::inclusion_lane::InclusionLaneError>>>,
    _parked_rx: Option<mpsc::Receiver<PendingUserOp>>,
}

impl Drop for FullServerRuntime {
    fn drop(&mut self) {
        self.shutdown.request_shutdown();
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
        if let Some(task) = self.lane_handle.take() {
            task.abort();
        }
    }
}

async fn start_full_server(db_path: &str, domain: Eip712Domain) -> Option<FullServerRuntime> {
    start_full_server_with_max_body(db_path, domain, 128 * 1024).await
}

async fn start_full_server_with_max_body(
    db_path: &str,
    domain: Eip712Domain,
    max_body_bytes: usize,
) -> Option<FullServerRuntime> {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(value) => value,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "skipping e2e integration test: cannot bind test listener in this environment"
            );
            return None;
        }
        Err(err) => panic!("bind test listener: {err}"),
    };
    let addr = listener.local_addr().expect("read listener addr");

    let storage = Storage::open(db_path, "NORMAL").expect("open storage");
    let shutdown = ShutdownSignal::default();

    let (tx, lane_handle) = InclusionLane::start(
        128,
        shutdown.clone(),
        WalletApp::new(WalletConfig::default()),
        storage,
        InclusionLaneConfig {
            batch_submitter_address: Address::from([0xff; 20]),
            max_user_ops_per_chunk: 32,
            safe_input_buffer_capacity: 32,
            max_batch_open: Duration::from_secs(60 * 60),
            idle_poll_interval: Duration::from_millis(2),
        },
    );

    let tx_feed = L2TxFeed::new(
        db_path.to_string(),
        shutdown.clone(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            batch_submitter_address: None,
        },
    );

    let server_task = api::start_on_listener(
        listener,
        tx,
        domain,
        MAX_METHOD_PAYLOAD_BYTES,
        shutdown.clone(),
        tx_feed,
        ApiConfig {
            max_body_bytes,
            ..ApiConfig::default()
        },
    );

    Some(FullServerRuntime {
        addr,
        shutdown,
        server_task: Some(server_task),
        lane_handle: Some(lane_handle),
        _parked_rx: None,
    })
}

async fn start_api_only_server(
    db_path: &str,
    domain: Eip712Domain,
    max_body_bytes: usize,
    queue_capacity: usize,
) -> Option<FullServerRuntime> {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(value) => value,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping api overload test: cannot bind test listener in this environment");
            return None;
        }
        Err(err) => panic!("bind test listener: {err}"),
    };
    let addr = listener.local_addr().expect("read listener addr");

    let _storage = Storage::open(db_path, "NORMAL").expect("open storage");
    let (tx, rx) = mpsc::channel::<PendingUserOp>(queue_capacity);
    let shutdown = ShutdownSignal::default();
    let tx_feed = L2TxFeed::new(
        db_path.to_string(),
        shutdown.clone(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            batch_submitter_address: None,
        },
    );
    let server_task = api::start_on_listener(
        listener,
        tx,
        domain,
        MAX_METHOD_PAYLOAD_BYTES,
        shutdown.clone(),
        tx_feed,
        ApiConfig {
            max_body_bytes,
            ..ApiConfig::default()
        },
    );

    Some(FullServerRuntime {
        addr,
        shutdown,
        server_task: Some(server_task),
        lane_handle: None,
        _parked_rx: Some(rx),
    })
}

async fn shutdown_runtime(mut runtime: FullServerRuntime) {
    runtime.shutdown.request_shutdown();
    if let Some(task) = runtime.lane_handle.take() {
        let lane_result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("wait for inclusion lane")
            .expect("join inclusion lane task");
        let ok =
            lane_result.is_ok() || matches!(lane_result, Err(InclusionLaneError::ChannelClosed));
        assert!(
            ok,
            "expected clean shutdown (Ok or ChannelClosed), got {lane_result:?}"
        );
    }
    if let Some(task) = runtime.server_task.take() {
        let server_result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("wait for server task")
            .expect("join server task");
        assert!(
            server_result.is_ok(),
            "expected clean server shutdown, got {server_result:?}"
        );
    }
}

fn bootstrap_open_frame_fee_zero(db_path: &str) {
    let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
    // gas_price defaults to 0 → recommended_fee = 0.
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    assert_eq!(head.frame_fee, 0);
}

fn make_valid_request(domain: &Eip712Domain) -> TxRequest {
    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    let method = Method::Withdrawal(Withdrawal {
        amount: U256::from(0_u64),
    });
    let user_op = UserOp {
        nonce: 0,
        max_fee: 0,
        data: ssz::Encode::as_ssz_bytes(&method).into(),
    };
    let signature_hex = sign_user_op_hex(domain, &user_op, &signing_key);
    TxRequest {
        message: user_op,
        signature: signature_hex,
        sender: sender.to_string(),
    }
}

fn seed_safe_direct_input(db_path: &str, safe_block: u64, payload: Vec<u8>) {
    let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
    storage
        .append_safe_inputs(
            safe_block,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload,
                block_number: safe_block,
            }],
        )
        .expect("append safe direct input");
}

fn load_all_ordered_l2_txs(db_path: &str) -> Vec<SequencedL2Tx> {
    let mut storage = Storage::open_read_only(db_path).expect("open read-only storage");
    let total = storage
        .ordered_l2_tx_count()
        .expect("query ordered l2 tx count");
    storage
        .load_ordered_l2_txs_page_from(0, total as usize)
        .expect("load ordered l2 txs")
}

fn assert_ws_message_matches_tx(
    actual: WsTxMessage,
    expected: &SequencedL2Tx,
    expected_offset: u64,
) {
    match (actual, expected) {
        (
            WsTxMessage::UserOp {
                offset,
                sender,
                fee,
                data,
            },
            SequencedL2Tx::UserOp(expected),
        ) => {
            assert_eq!(offset, expected_offset);
            assert_eq!(
                decode_hex_prefixed(sender.as_str()),
                expected.sender.as_slice()
            );
            assert_eq!(fee, expected.fee);
            assert_eq!(decode_hex_prefixed(data.as_str()), expected.data.as_slice());
        }
        (
            WsTxMessage::DirectInput {
                offset,
                sender,
                block_number,
                payload,
            },
            SequencedL2Tx::Direct(expected),
        ) => {
            assert_eq!(offset, expected_offset);
            assert_eq!(
                decode_hex_prefixed(sender.as_str()),
                expected.sender.as_slice()
            );
            assert_eq!(block_number, expected.block_number);
            assert_eq!(
                decode_hex_prefixed(payload.as_str()),
                expected.payload.as_slice()
            );
        }
        (actual, expected) => {
            panic!(
                "expected websocket message to match persisted tx, got {actual:?} vs {expected:?}"
            );
        }
    }
}

async fn post_raw_json(addr: std::net::SocketAddr, body: &str) -> (u16, String) {
    let host_port = addr.to_string();
    let mut stream = tokio::net::TcpStream::connect(host_port.as_str())
        .await
        .expect("connect test http socket");
    let request = format!(
        "POST /tx HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write raw request");
    stream.flush().await.expect("flush raw request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read raw response");
    parse_http_response(response.as_slice())
}

fn parse_http_response(raw: &[u8]) -> (u16, String) {
    let text = String::from_utf8(raw.to_vec()).expect("response is valid utf8");
    let mut sections = text.splitn(2, "\r\n\r\n");
    let headers = sections.next().unwrap_or_default();
    let body = sections.next().unwrap_or_default().to_string();
    let status = headers
        .lines()
        .next()
        .expect("status line exists")
        .split_whitespace()
        .nth(1)
        .expect("status code exists")
        .parse::<u16>()
        .expect("status code parses");
    (status, body)
}

fn sign_user_op_hex(domain: &Eip712Domain, user_op: &UserOp, signing_key: &SigningKey) -> String {
    let hash = user_op.eip712_signing_hash(domain);
    let k256_sig = signing_key
        .sign_prehash(hash.as_slice())
        .expect("sign user op hash");

    let sender = address_from_signing_key(signing_key);
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|candidate| {
            candidate
                .recover_address_from_prehash(&hash)
                .ok()
                .map(|value| value == sender)
                .unwrap_or(false)
        })
        .expect("recoverable parity for signature");

    alloy_primitives::hex::encode_prefixed(signature.as_bytes())
}

fn address_from_signing_key(signing_key: &SigningKey) -> Address {
    let verifying = signing_key.verifying_key().to_encoded_point(false);
    Address::from_raw_public_key(&verifying.as_bytes()[1..])
}

async fn recv_ws_message(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> WsTxMessage {
    let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("wait for websocket frame")
        .expect("websocket stream ended")
        .expect("receive websocket frame");
    match frame {
        Message::Text(value) => serde_json::from_str(value.as_str()).expect("parse ws payload"),
        other => panic!("expected text ws frame, got {other:?}"),
    }
}

fn decode_hex_prefixed(value: &str) -> Vec<u8> {
    assert!(value.starts_with("0x"), "hex field must be 0x-prefixed");
    alloy_primitives::hex::decode(value).expect("decode hex")
}

fn test_domain() -> Eip712Domain {
    Eip712Domain {
        name: Some("CartesiAppSequencer".to_string().into()),
        version: Some("1".to_string().into()),
        chain_id: Some(U256::from(1_u64)),
        verifying_contract: Some(Address::from_slice(&[0_u8; 20])),
        salt: None,
    }
}

struct TestDb {
    _dir: TempDir,
    path: String,
}

fn temp_db(name: &str) -> TestDb {
    let dir = tempfile::Builder::new()
        .prefix(format!("sequencer-full-e2e-{name}-").as_str())
        .tempdir()
        .expect("create temporary test directory");
    let path = dir.path().join("sequencer.sqlite");
    TestDb {
        _dir: dir,
        path: path.to_string_lossy().into_owned(),
    }
}
