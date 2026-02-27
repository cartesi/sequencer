// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, Signature, U256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use futures_util::StreamExt;
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::hazmat::PrehashSigner;
use sequencer::api::{AppState, router};
use sequencer::application::{Deposit, Method, WalletApp, WalletConfig};
use sequencer::inclusion_lane::{
    InclusionLane, InclusionLaneConfig, InclusionLaneError, InclusionLaneInput,
};
use sequencer::l2_tx_broadcaster::{L2TxBroadcaster, L2TxBroadcasterConfig};
use sequencer::storage::Storage;
use sequencer::user_op::UserOp;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Deserialize)]
struct TxResponse {
    ok: bool,
    tx_hash: String,
    sender: String,
    nonce: u32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WsTxMessage {
    UserOp {
        offset: u64,
        sender: String,
        fee: u64,
        data: String,
    },
    DirectInput {
        offset: u64,
        payload: String,
    },
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_submit_tx_ack_and_broadcast() {
    let db_path = temp_db_path("full-e2e");
    let domain = test_domain();
    bootstrap_open_batch_fee_zero(&db_path);

    let Some(runtime) = start_full_server(&db_path, domain.clone()).await else {
        return;
    };

    let ws_url = format!("ws://{}/ws/subscribe?from_offset=0", runtime.addr);
    let (mut ws, _) = connect_async(ws_url).await.expect("connect websocket");

    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    let method = Method::Deposit(Deposit {
        amount: U256::from(5_u64),
        to: sender,
    });
    let user_op = UserOp {
        nonce: 0,
        max_fee: 0,
        data: ssz::Encode::as_ssz_bytes(&method).into(),
    };
    let signature_hex = sign_user_op_hex(&domain, &user_op, &signing_key);

    let request_body = serde_json::json!({
        "message": user_op,
        "signature": signature_hex,
        "sender": sender.to_string(),
    });

    let (status, response_body) = post_json(runtime.addr, "/tx", request_body.to_string()).await;
    assert_eq!(
        status, 200,
        "submit tx should succeed: body={response_body}"
    );

    let response: TxResponse =
        serde_json::from_str(response_body.as_str()).expect("parse response");
    assert!(response.ok);
    assert_eq!(response.nonce, 0);
    assert_eq!(response.sender, sender.to_string());
    assert!(
        response.tx_hash.starts_with("0x"),
        "response tx hash should be 0x-prefixed"
    );

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

struct FullServerRuntime {
    addr: std::net::SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
    server_task: tokio::task::JoinHandle<()>,
    lane_stop: sequencer::inclusion_lane::InclusionLaneStop,
    lane_handle: tokio::task::JoinHandle<InclusionLaneError>,
}

async fn start_full_server(db_path: &str, domain: Eip712Domain) -> Option<FullServerRuntime> {
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
        },
    );
    let (lane_handle, lane_stop) = inclusion_lane.spawn();

    let broadcaster = L2TxBroadcaster::start(
        db_path.to_string(),
        L2TxBroadcasterConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            subscriber_buffer_capacity: 256,
        },
    )
    .expect("start broadcaster");

    let state = Arc::new(AppState {
        tx_sender: tx,
        domain,
        queue_timeout: Duration::from_millis(100),
        broadcaster,
    });
    let app = router(state, 128 * 1024);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });
    let server_task = tokio::spawn(async move {
        server.await.expect("run test server");
    });

    Some(FullServerRuntime {
        addr,
        shutdown_tx,
        server_task,
        lane_stop,
        lane_handle,
    })
}

async fn shutdown_runtime(runtime: FullServerRuntime) {
    runtime
        .shutdown_tx
        .send(())
        .expect("request server shutdown");
    runtime.server_task.await.expect("join server task");
    runtime.lane_stop.request_shutdown();
    let lane_result = tokio::time::timeout(Duration::from_secs(2), runtime.lane_handle)
        .await
        .expect("wait for inclusion lane")
        .expect("join inclusion lane task");
    assert!(
        matches!(lane_result, InclusionLaneError::ShutdownRequested),
        "expected shutdown result, got {lane_result}"
    );
}

fn bootstrap_open_batch_fee_zero(db_path: &str) {
    let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
    storage.set_recommended_fee(0).expect("set recommended fee");
    let mut head = storage.load_open_state().expect("load open state");
    storage
        .close_frame_and_batch(&mut head, 0)
        .expect("rotate batch to fee=0");
    assert_eq!(head.batch_fee, 0);
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

async fn post_json(addr: std::net::SocketAddr, path: &str, body: String) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect http socket");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write http request");
    stream.flush().await.expect("flush http request");

    let mut response = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read_result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
            .await
            .expect("timed out while reading http response")
            .expect("read http response");
        if read_result == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read_result]);

        if let Some((header_end, content_length)) = response_content_len(response.as_slice())
            && response.len() >= header_end.saturating_add(content_length)
        {
            break;
        }
    }
    parse_http_response(response.as_slice())
}

fn parse_http_response(raw: &[u8]) -> (u16, String) {
    let text = String::from_utf8(raw.to_vec()).expect("http response utf8");
    let mut sections = text.splitn(2, "\r\n\r\n");
    let headers = sections.next().unwrap_or_default();
    let body = sections.next().unwrap_or_default().to_string();

    let mut header_lines = headers.lines();
    let status_line = header_lines.next().expect("http status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse::<u16>()
        .expect("parse status code");
    (status, body)
}

fn response_content_len(raw: &[u8]) -> Option<(usize, usize)> {
    let header_end = raw.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let headers = std::str::from_utf8(&raw[..header_end]).ok()?;
    let mut content_length = None;
    for line in headers.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
            break;
        }
    }
    content_length.map(|len| (header_end, len))
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

fn temp_db_path(name: &str) -> String {
    let mut path = std::env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("sequencer-full-e2e-{name}-{unique}.sqlite"));
    path_to_string(path)
}

fn path_to_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}
