// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, Signature};
use alloy_sol_types::Eip712Domain;
use futures_util::{SinkExt, StreamExt};
use sequencer::api::{AppState, router};
use sequencer::inclusion_lane::{InclusionLaneInput, PendingUserOp, SequencerError};
use sequencer::l2_tx::SequencedL2Tx;
use sequencer::l2_tx_broadcaster::{L2TxBroadcaster, L2TxBroadcasterConfig};
use sequencer::storage::{IndexedDirectInput, Storage};
use sequencer::user_op::{SignedUserOp, UserOp};
use serde::Deserialize;
use tempfile::TempDir;
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
    let db = temp_db("ws-subscribe-zero");
    seed_ordered_txs(db.path.as_str());
    let expected = load_ordered_l2_txs_page(db.path.as_str(), 0, 2);
    assert_eq!(expected.len(), 2, "seeded replay must contain two txs");

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };
    let url = format!("ws://{}/ws/subscribe?from_offset=0", runtime.addr);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    let first = recv_tx_message(&mut ws).await;
    let second = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_runtime(runtime).await;

    assert_ws_message_matches_tx(first, &expected[0], 0);
    assert_ws_message_matches_tx(second, &expected[1], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_resumes_from_given_offset() {
    let db = temp_db("ws-subscribe-resume");
    seed_ordered_txs(db.path.as_str());
    let expected = load_ordered_l2_txs_page(db.path.as_str(), 1, 1);
    assert_eq!(
        expected.len(),
        1,
        "resume snapshot must contain one event at offset 1"
    );

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };
    let url = format!("ws://{}/ws/subscribe?from_offset=1", runtime.addr);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    let first = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_runtime(runtime).await;

    assert_ws_message_matches_tx(first, &expected[0], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_receives_live_events_after_subscribing() {
    let db = temp_db("ws-subscribe-live");
    seed_ordered_txs(db.path.as_str());
    let base_offset = ordered_l2_tx_count(db.path.as_str());

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };

    // Subscribe at the current DB head to exercise live-only delivery.
    let url = format!(
        "ws://{}/ws/subscribe?from_offset={base_offset}",
        runtime.addr
    );
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    append_drained_direct_input(db.path.as_str(), 1, vec![0xbb]);
    let expected = load_ordered_l2_txs_page(db.path.as_str(), base_offset, 1);
    assert_eq!(
        expected.len(),
        1,
        "appending one drained direct input must add one sequenced tx"
    );
    let live = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_runtime(runtime).await;

    assert_ws_message_matches_tx(live, &expected[0], base_offset);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_fanout_delivers_live_event_to_multiple_subscribers() {
    let db = temp_db("ws-subscribe-fanout");
    seed_ordered_txs(db.path.as_str());
    let base_offset = ordered_l2_tx_count(db.path.as_str());

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };

    let url = format!(
        "ws://{}/ws/subscribe?from_offset={base_offset}",
        runtime.addr
    );
    let (mut ws_a, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url.as_str()))
        .await
        .expect("timeout connecting websocket A")
        .expect("connect websocket A");
    let (mut ws_b, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket B")
        .expect("connect websocket B");

    append_drained_direct_input(db.path.as_str(), 1, vec![0xcd]);
    let expected = load_ordered_l2_txs_page(db.path.as_str(), base_offset, 1);
    assert_eq!(
        expected.len(),
        1,
        "appending one drained direct input must add one sequenced tx"
    );

    let event_a = recv_tx_message(&mut ws_a).await;
    let event_b = recv_tx_message(&mut ws_b).await;
    drop(ws_a);
    drop(ws_b);

    shutdown_runtime(runtime).await;

    assert_ws_message_matches_tx(event_a, &expected[0], base_offset);
    assert_ws_message_matches_tx(event_b, &expected[0], base_offset);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_replies_with_pong_on_ping() {
    let db = temp_db("ws-subscribe-ping-pong");
    seed_ordered_txs(db.path.as_str());
    // Use a far-future offset so this test validates ping/pong without
    // interleaving replay/live tx frames.
    let from_offset = u64::MAX;

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };

    let url = format!(
        "ws://{}/ws/subscribe?from_offset={from_offset}",
        runtime.addr
    );
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    ws.send(Message::Ping(vec![0x01, 0x02].into()))
        .await
        .expect("send ping frame");

    let frame = recv_raw_message(&mut ws).await;
    drop(ws);

    shutdown_runtime(runtime).await;

    match frame {
        Message::Pong(payload) => assert_eq!(payload.as_ref(), [0x01, 0x02]),
        value => panic!("expected pong frame, got {value:?}"),
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
        .close_frame_only(&mut head, 0, 1)
        .expect("close frame with one drained direct input");
}

fn append_drained_direct_input(db_path: &str, index: u64, payload: Vec<u8>) {
    let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
    let mut head = storage.load_open_state().expect("load open state");
    storage
        .append_safe_direct_inputs(&[IndexedDirectInput { index, payload }])
        .expect("append direct input");
    storage
        .close_frame_only(&mut head, index, 1)
        .expect("close frame with one drained direct input");
}

struct WsServerRuntime {
    addr: std::net::SocketAddr,
    broadcaster: L2TxBroadcaster,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for WsServerRuntime {
    fn drop(&mut self) {
        self.broadcaster.request_shutdown();
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

async fn start_test_server(db_path: &str) -> Option<WsServerRuntime> {
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
            metrics_enabled: false,
            metrics_log_interval: Duration::from_secs(5),
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
        queue_capacity: 1,
        overload_queue_depth_threshold: 1,
        overload_max_inflight_submissions: 16,
        inflight_submissions: Arc::new(AtomicUsize::new(0)),
        queue_timeout: Duration::from_millis(50),
        broadcaster: broadcaster.clone(),
    });
    let app = router(state, 128 * 1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });
    let task = tokio::spawn(async move {
        server.await.expect("run test server");
    });

    Some(WsServerRuntime {
        addr,
        broadcaster,
        shutdown_tx: Some(shutdown_tx),
        server_task: Some(task),
    })
}

async fn shutdown_runtime(mut runtime: WsServerRuntime) {
    runtime.broadcaster.request_shutdown();
    if let Some(tx) = runtime.shutdown_tx.take() {
        let _ = tx.send(());
    }
    if let Some(task) = runtime.server_task.take() {
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("wait for server task")
            .expect("join server task");
    }
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

async fn recv_raw_message(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Message {
    tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("wait for websocket message")
        .expect("websocket stream ended")
        .expect("receive websocket frame")
}

fn decode_hex_prefixed(value: &str) -> Vec<u8> {
    assert!(value.starts_with("0x"), "hex field must be 0x-prefixed");
    alloy_primitives::hex::decode(value).expect("decode hex")
}

fn ordered_l2_tx_count(db_path: &str) -> u64 {
    let mut storage = Storage::open_read_only(db_path).expect("open read-only storage");
    storage
        .ordered_l2_tx_count()
        .expect("query ordered l2 count")
}

fn load_ordered_l2_txs_page(db_path: &str, from_offset: u64, limit: usize) -> Vec<SequencedL2Tx> {
    let mut storage = Storage::open_read_only(db_path).expect("open read-only storage");
    storage
        .load_ordered_l2_txs_page_from(from_offset, limit)
        .expect("load ordered l2 tx page")
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
        (WsTxMessage::DirectInput { offset, payload }, SequencedL2Tx::Direct(expected)) => {
            assert_eq!(offset, expected_offset);
            assert_eq!(
                decode_hex_prefixed(payload.as_str()),
                expected.payload.as_slice()
            );
        }
        (actual, expected) => {
            panic!("ws message type mismatch; actual={actual:?}, expected={expected:?}")
        }
    }
}

struct TestDb {
    _dir: TempDir,
    path: String,
}

fn temp_db(name: &str) -> TestDb {
    let dir = tempfile::Builder::new()
        .prefix(format!("sequencer-ws-broadcaster-{name}-").as_str())
        .tempdir()
        .expect("create temporary test directory");
    let path = dir.path().join("sequencer.sqlite");
    TestDb {
        _dir: dir,
        path: path.to_string_lossy().into_owned(),
    }
}
