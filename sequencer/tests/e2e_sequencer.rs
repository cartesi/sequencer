// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, Signature, U256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use app_core::application::{WalletApp, WalletConfig};
use futures_util::StreamExt;
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::hazmat::PrehashSigner;
use sequencer::api::{AppState, router};
use sequencer::inclusion_lane::{
    InclusionLane, InclusionLaneConfig, InclusionLaneError, InclusionLaneInput,
};
use sequencer::l2_tx_broadcaster::{L2TxBroadcaster, L2TxBroadcasterConfig};
use sequencer::storage::Storage;
use sequencer_core::api::{TxRequest, TxResponse, WsTxMessage};
use sequencer_core::application::{Method, Withdrawal};
use sequencer_core::user_op::UserOp;
use sequencer_rust_client::SequencerClient;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc, oneshot};
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
async fn api_returns_429_when_tx_middleware_concurrency_is_exceeded() {
    let db = temp_db("tx-middleware-overload");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) =
        start_api_only_server(db.path.as_str(), domain.clone(), 128 * 1024, 8, 1).await
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
    assert_eq!(status, 429, "expected 429 for middleware overload: {body}");
    assert!(
        body.contains("OVERLOADED"),
        "expected OVERLOADED code for middleware overload: {body}"
    );

    first.abort();
    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_returns_429_when_queue_is_full() {
    let db = temp_db("queue-full-overload");
    let domain = test_domain();
    bootstrap_open_frame_fee_zero(db.path.as_str());

    let Some(runtime) =
        start_api_only_server(db.path.as_str(), domain.clone(), 128 * 1024, 1, 8).await
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

struct FullServerRuntime {
    addr: std::net::SocketAddr,
    broadcaster: L2TxBroadcaster,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_task: Option<tokio::task::JoinHandle<()>>,
    lane_stop: Option<sequencer::inclusion_lane::InclusionLaneStop>,
    lane_handle: Option<tokio::task::JoinHandle<InclusionLaneError>>,
    _parked_rx: Option<mpsc::Receiver<InclusionLaneInput>>,
}

impl Drop for FullServerRuntime {
    fn drop(&mut self) {
        self.broadcaster.request_shutdown();
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(stop) = self.lane_stop.take() {
            stop.request_shutdown();
        }
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
    let (tx, rx) = mpsc::channel::<InclusionLaneInput>(128);

    let inclusion_lane = InclusionLane::new(
        rx,
        WalletApp::new(WalletConfig),
        storage,
        InclusionLaneConfig {
            max_user_ops_per_chunk: 32,
            safe_direct_buffer_capacity: 32,
            max_batch_open: Duration::from_secs(60 * 60),
            max_batch_user_op_bytes: 1_048_576,
            idle_poll_interval: Duration::from_millis(2),
            metrics_enabled: false,
            metrics_log_interval: Duration::from_secs(5),
        },
    );
    let (lane_handle, lane_stop) = inclusion_lane.spawn();

    let broadcaster = L2TxBroadcaster::start(
        db_path.to_string(),
        L2TxBroadcasterConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            subscriber_buffer_capacity: 256,
            metrics_enabled: false,
            metrics_log_interval: Duration::from_secs(5),
        },
    )
    .expect("start broadcaster");

    let state = Arc::new(AppState {
        tx_sender: tx,
        domain,
        overload_max_inflight_submissions: 256,
        ws_subscriber_limit: Arc::new(Semaphore::new(64)),
        ws_max_catchup_events: 50_000,
        broadcaster: broadcaster.clone(),
    });
    let app = router(state, max_body_bytes);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });
    let server_task = tokio::spawn(async move {
        server.await.expect("run test server");
    });

    Some(FullServerRuntime {
        addr,
        broadcaster,
        shutdown_tx: Some(shutdown_tx),
        server_task: Some(server_task),
        lane_stop: Some(lane_stop),
        lane_handle: Some(lane_handle),
        _parked_rx: None,
    })
}

async fn start_api_only_server(
    db_path: &str,
    domain: Eip712Domain,
    max_body_bytes: usize,
    queue_capacity: usize,
    overload_max_inflight_submissions: usize,
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
    let (tx, rx) = mpsc::channel::<InclusionLaneInput>(queue_capacity);
    let broadcaster = L2TxBroadcaster::start(
        db_path.to_string(),
        L2TxBroadcasterConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            subscriber_buffer_capacity: 256,
            metrics_enabled: false,
            metrics_log_interval: Duration::from_secs(5),
        },
    )
    .expect("start broadcaster");
    let state = Arc::new(AppState {
        tx_sender: tx,
        domain,
        overload_max_inflight_submissions,
        ws_subscriber_limit: Arc::new(Semaphore::new(64)),
        ws_max_catchup_events: 50_000,
        broadcaster: broadcaster.clone(),
    });
    let app = router(state, max_body_bytes);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });
    let server_task = tokio::spawn(async move {
        server.await.expect("run test server");
    });

    Some(FullServerRuntime {
        addr,
        broadcaster,
        shutdown_tx: Some(shutdown_tx),
        server_task: Some(server_task),
        lane_stop: None,
        lane_handle: None,
        _parked_rx: Some(rx),
    })
}

async fn shutdown_runtime(mut runtime: FullServerRuntime) {
    runtime.broadcaster.request_shutdown();
    if let Some(stop) = runtime.lane_stop.take() {
        stop.request_shutdown();
    }
    if let Some(tx) = runtime.shutdown_tx.take() {
        let _ = tx.send(());
    }
    if let Some(task) = runtime.server_task.take() {
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("wait for server task")
            .expect("join server task");
    }
    if let Some(task) = runtime.lane_handle.take() {
        let lane_result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("wait for inclusion lane")
            .expect("join inclusion lane task");
        assert!(
            matches!(lane_result, InclusionLaneError::ShutdownRequested),
            "expected shutdown result, got {lane_result}"
        );
    }
}

fn bootstrap_open_frame_fee_zero(db_path: &str) {
    let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
    storage.set_recommended_fee(0).expect("set recommended fee");
    let mut head = storage.load_open_state().expect("load open state");
    storage
        .close_frame_and_batch(&mut head, 0, 0)
        .expect("rotate batch to fee=0");
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
