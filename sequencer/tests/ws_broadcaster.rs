// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256, Signature};
use alloy_sol_types::Eip712Domain;
use futures_util::StreamExt;
use sequencer::api::{AppState, router};
use sequencer::inclusion_lane::{InclusionLaneInput, PendingUserOp, SequencerError};
use sequencer::l2_tx_broadcaster::{L2TxBroadcaster, L2TxBroadcasterConfig};
use sequencer::storage::{IndexedDirectInput, Storage};
use sequencer::user_op::{SignedUserOp, UserOp};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Deserialize)]
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
async fn ws_subscribe_streams_ordered_txs_from_offset_zero() {
    let db_path = temp_db_path("ws-subscribe-zero");
    seed_ordered_txs(&db_path);

    let Some((addr, shutdown_tx, server_task)) = start_test_server(&db_path).await else {
        return;
    };
    let url = format!("ws://{addr}/ws/subscribe?from_offset=0");
    let (mut ws, _) = connect_async(url).await.expect("connect websocket");

    let first = recv_tx_message(&mut ws).await;
    let second = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_tx.send(()).expect("request shutdown");
    server_task.await.expect("join server task");

    match first {
        WsTxMessage::UserOp {
            offset,
            sender,
            fee,
            data,
        } => {
            assert_eq!(offset, 0);
            assert_eq!(fee, 1);
            assert_eq!(decode_hex_prefixed(data.as_str()), vec![0x42]);
            assert_eq!(
                decode_hex_prefixed(sender.as_str()),
                vec![0x11; 20],
                "sender should match persisted user-op sender"
            );
        }
        value => panic!("expected user_op at offset 0, got {value:?}"),
    }

    match second {
        WsTxMessage::DirectInput { offset, payload } => {
            assert_eq!(offset, 1);
            assert_eq!(decode_hex_prefixed(payload.as_str()), vec![0xaa]);
        }
        value => panic!("expected direct_input at offset 1, got {value:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_resumes_from_given_offset() {
    let db_path = temp_db_path("ws-subscribe-resume");
    seed_ordered_txs(&db_path);

    let Some((addr, shutdown_tx, server_task)) = start_test_server(&db_path).await else {
        return;
    };
    let url = format!("ws://{addr}/ws/subscribe?from_offset=1");
    let (mut ws, _) = connect_async(url).await.expect("connect websocket");

    let first = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_tx.send(()).expect("request shutdown");
    server_task.await.expect("join server task");

    match first {
        WsTxMessage::DirectInput { offset, payload } => {
            assert_eq!(offset, 1);
            assert_eq!(decode_hex_prefixed(payload.as_str()), vec![0xaa]);
        }
        value => panic!("expected direct_input at offset 1, got {value:?}"),
    }
}

fn seed_ordered_txs(db_path: &str) {
    let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
    let mut head = storage.load_open_state().expect("load open state");

    let (respond_to, _recv) = oneshot::channel::<Result<(), SequencerError>>();
    let pending = PendingUserOp {
        signed: SignedUserOp {
            sender: Address::from_slice(&[0x11; 20]),
            signature: Signature::test_signature(),
            user_op: UserOp {
                nonce: 0,
                max_fee: 3,
                data: vec![0x42].into(),
            },
        },
        tx_hash: B256::from([0x77; 32]),
        respond_to,
        received_at: SystemTime::now(),
    };

    storage
        .append_user_ops_chunk(&mut head, &[pending])
        .expect("append user-op chunk");
    storage
        .append_safe_direct_inputs(&[IndexedDirectInput {
            index: 0,
            payload: vec![0xaa],
        }])
        .expect("append direct input");
    storage
        .close_frame_only(&mut head, 1)
        .expect("close frame with one drained direct input");
}

async fn start_test_server(
    db_path: &str,
) -> Option<(
    std::net::SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
)> {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(value) => value,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "skipping ws integration test: cannot bind test listener in this environment"
            );
            return None;
        }
        Err(err) => panic!("bind test listener: {err}"),
    };
    let addr = listener.local_addr().expect("read listener addr");

    let (tx_sender, _rx) = mpsc::channel::<InclusionLaneInput>(1);
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
        tx_sender,
        domain: Eip712Domain {
            name: None,
            version: None,
            chain_id: None,
            verifying_contract: None,
            salt: None,
        },
        queue_timeout: Duration::from_millis(50),
        broadcaster,
    });
    let app = router(state, 128 * 1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });
    let task = tokio::spawn(async move {
        server.await.expect("run test server");
    });

    Some((addr, shutdown_tx, task))
}

async fn recv_tx_message(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> WsTxMessage {
    let received = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("wait for websocket message")
        .expect("websocket stream ended")
        .expect("receive websocket frame");

    let text = match received {
        Message::Text(value) => value,
        other => panic!("expected text frame, got {other:?}"),
    };

    serde_json::from_str(text.as_str()).expect("parse websocket tx message")
}

fn decode_hex_prefixed(value: &str) -> Vec<u8> {
    assert!(value.starts_with("0x"), "hex field must be 0x-prefixed");
    alloy_primitives::hex::decode(value).expect("decode hex")
}

fn temp_db_path(name: &str) -> String {
    let mut path = std::env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("sequencer-ws-broadcaster-{name}-{unique}.sqlite"));
    path_to_string(path)
}

fn path_to_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}
