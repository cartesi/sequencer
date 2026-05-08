// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::io::ErrorKind;
use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, Signature};
use alloy_sol_types::Eip712Domain;
use app_core::application::MAX_METHOD_PAYLOAD_BYTES;
use futures_util::{SinkExt, StreamExt};
use sequencer::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use sequencer::http::{self, ApiConfig, WS_CATCHUP_WINDOW_EXCEEDED_REASON};
use sequencer::ingress::inclusion_lane::{PendingUserOp, SequencerError};
use sequencer::runtime::shutdown::ShutdownSignal;
use sequencer::storage::{SafeInputRange, Storage, StoredSafeInput};
use sequencer_core::api::WsTxMessage;
use sequencer_core::l2_tx::SequencedL2Tx;
use sequencer_core::user_op::{SignedUserOp, UserOp};
use sequencer_rust_client::SequencerClient;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

mod common;
use common::temp_db;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_streams_ordered_txs_from_offset_zero() {
    let db = temp_db("ws-subscribe-zero");
    seed_ordered_txs(db.path.as_str());
    let expected = load_ordered_l2_txs_page(db.path.as_str(), 0, 2);
    assert_eq!(expected.len(), 2, "seeded replay must contain two txs");

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };
    let url = ws_subscribe_url(runtime.addr, 0);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    let first = recv_tx_message(&mut ws).await;
    let second = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_runtime(runtime).await;

    // DB offsets (SQLite rowid) start at 1.
    assert_ws_message_matches_tx(first, &expected[0], 1);
    assert_ws_message_matches_tx(second, &expected[1], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_resumes_from_given_offset() {
    let db = temp_db("ws-subscribe-resume");
    seed_ordered_txs(db.path.as_str());
    // Resume from DB offset 1 — should get items with offset > 1.
    let expected = load_ordered_l2_txs_page(db.path.as_str(), 1, 1);
    assert_eq!(
        expected.len(),
        1,
        "resume snapshot must contain one event at offset 2"
    );

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };
    let url = ws_subscribe_url(runtime.addr, 1);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    let first = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_runtime(runtime).await;

    assert_ws_message_matches_tx(first, &expected[0], 2);
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
    let url = ws_subscribe_url(runtime.addr, base_offset);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    append_drained_direct_input(db.path.as_str(), vec![0xbb]);
    let expected = load_ordered_l2_txs_page(db.path.as_str(), base_offset, 1);
    assert_eq!(
        expected.len(),
        1,
        "appending one drained direct input must add one sequenced tx"
    );
    let live = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_runtime(runtime).await;

    assert_ws_message_matches_tx(live, &expected[0], base_offset + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_fanout_delivers_live_event_to_multiple_subscribers() {
    let db = temp_db("ws-subscribe-fanout");
    seed_ordered_txs(db.path.as_str());
    let base_offset = ordered_l2_tx_count(db.path.as_str());

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };

    let url = ws_subscribe_url(runtime.addr, base_offset);
    let (mut ws_a, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url.as_str()))
        .await
        .expect("timeout connecting websocket A")
        .expect("connect websocket A");
    let (mut ws_b, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket B")
        .expect("connect websocket B");

    append_drained_direct_input(db.path.as_str(), vec![0xcd]);
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

    assert_ws_message_matches_tx(event_a, &expected[0], base_offset + 1);
    assert_ws_message_matches_tx(event_b, &expected[0], base_offset + 1);
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

    let url = ws_subscribe_url(runtime.addr, from_offset);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_rejects_when_subscriber_limit_is_reached() {
    let db = temp_db("ws-subscriber-limit");
    seed_ordered_txs(db.path.as_str());
    let base_offset = ordered_l2_tx_count(db.path.as_str());

    let Some(runtime) = start_test_server_with_limits(db.path.as_str(), 1, 50_000).await else {
        return;
    };

    let url = ws_subscribe_url(runtime.addr, base_offset);
    let (ws_a, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url.as_str()))
        .await
        .expect("timeout connecting websocket A")
        .expect("connect websocket A");

    let second = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket B");
    match second {
        Ok((_ws, _resp)) => panic!("expected second subscriber to be rejected with 429"),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status().as_u16(), 429);
        }
        Err(other) => panic!("expected HTTP 429 handshake rejection, got {other:?}"),
    }

    drop(ws_a);
    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_closes_when_catchup_window_exceeds_limit() {
    let db = temp_db("ws-catchup-limit");
    seed_ordered_txs(db.path.as_str());

    let Some(runtime) = start_test_server_with_limits(db.path.as_str(), 64, 1).await else {
        return;
    };

    let url = ws_subscribe_url(runtime.addr, 0);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    let frame = recv_raw_message(&mut ws).await;
    match frame {
        Message::Close(Some(close_frame)) => {
            assert_eq!(
                close_frame.code,
                tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy
            );
            assert_eq!(close_frame.reason, WS_CATCHUP_WINDOW_EXCEEDED_REASON);
        }
        other => panic!("expected close frame for catch-up limit, got {other:?}"),
    }

    drop(ws);
    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_allows_catchup_exactly_at_limit() {
    let db = temp_db("ws-catchup-boundary");
    seed_ordered_txs(db.path.as_str());
    let expected = load_ordered_l2_txs_page(db.path.as_str(), 0, 2);
    assert_eq!(expected.len(), 2, "seeded replay must contain two txs");

    let Some(runtime) = start_test_server_with_limits(db.path.as_str(), 64, 2).await else {
        return;
    };

    let url = ws_subscribe_url(runtime.addr, 0);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    let first = recv_tx_message(&mut ws).await;
    let second = recv_tx_message(&mut ws).await;
    drop(ws);

    shutdown_runtime(runtime).await;

    assert_ws_message_matches_tx(first, &expected[0], 1);
    assert_ws_message_matches_tx(second, &expected[1], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_subscribe_closes_on_oversized_inbound_message() {
    let db = temp_db("ws-oversized-inbound");
    seed_ordered_txs(db.path.as_str());

    let Some(runtime) = start_test_server(db.path.as_str()).await else {
        return;
    };

    let url = ws_subscribe_url(runtime.addr, u64::MAX);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    ws.send(Message::Text("x".repeat(64 * 1024).into()))
        .await
        .expect("send oversized text frame");

    let received = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("wait for websocket close after oversized inbound message");

    match received {
        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {}
        other => {
            panic!("expected websocket to close after oversized inbound message, got {other:?}")
        }
    }

    drop(ws);
    shutdown_runtime(runtime).await;
}

fn seed_ordered_txs(db_path: &str) {
    let mut storage = Storage::open(db_path).expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");

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
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }],
            &sequencer_core::protocol::ProtocolConfig {
                batch_submitter: Address::ZERO,
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
        )
        .expect("append direct input");
    storage
        .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
        .expect("close frame with one drained direct input");
}

fn append_drained_direct_input(db_path: &str, payload: Vec<u8>) {
    let mut storage = Storage::open(db_path).expect("open storage");
    let mut head = storage
        .open_state()
        .expect("load open state")
        .expect("open state should exist");
    let safe_block = storage
        .current_safe_block()
        .expect("read current safe block")
        .expect("safe head should have been observed")
        .saturating_add(1);
    let next_direct_index = storage
        .safe_input_end_exclusive()
        .expect("read next direct input index");
    storage
        .append_safe_inputs(
            safe_block,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload,
                block_number: safe_block,
            }],
            &sequencer_core::protocol::ProtocolConfig {
                batch_submitter: Address::ZERO,
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
        )
        .expect("append direct input");
    storage
        .close_frame_only(
            &mut head,
            safe_block,
            SafeInputRange::new(next_direct_index, next_direct_index.saturating_add(1)),
        )
        .expect("close frame with one drained direct input");
}

struct WsServerRuntime {
    addr: std::net::SocketAddr,
    shutdown: ShutdownSignal,
    server_task: Option<http::ApiServerTask>,
}

impl Drop for WsServerRuntime {
    fn drop(&mut self) {
        self.shutdown.request_shutdown();
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

async fn start_test_server(db_path: &str) -> Option<WsServerRuntime> {
    start_test_server_with_limits(db_path, 64, 50_000).await
}

async fn start_test_server_with_limits(
    db_path: &str,
    ws_max_subscribers: usize,
    ws_max_catchup_events: u64,
) -> Option<WsServerRuntime> {
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

    let (tx_sender, _rx) = mpsc::channel::<PendingUserOp>(1);
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
    let task = http::start_on_listener(
        listener,
        tx_sender,
        Eip712Domain {
            name: None,
            version: None,
            chain_id: None,
            verifying_contract: None,
            salt: None,
        },
        MAX_METHOD_PAYLOAD_BYTES,
        shutdown.clone(),
        tx_feed,
        ApiConfig {
            ws_max_subscribers,
            ws_max_catchup_events,
            ..ApiConfig::default()
        },
    );

    Some(WsServerRuntime {
        addr,
        shutdown,
        server_task: Some(task),
    })
}

async fn shutdown_runtime(mut runtime: WsServerRuntime) {
    runtime.shutdown.request_shutdown();
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

fn ws_subscribe_url(addr: std::net::SocketAddr, from_offset: u64) -> String {
    let endpoint = format!("http://{addr}");
    let client = SequencerClient::new(endpoint).expect("build sequencer client");
    client.ws_subscribe_url(from_offset)
}

fn ordered_l2_tx_count(db_path: &str) -> u64 {
    let mut storage = Storage::open_read_only(db_path).expect("open read-only storage");
    storage
        .ordered_l2_tx_head_offset()
        .expect("query ordered l2 head offset")
}

fn load_ordered_l2_txs_page(db_path: &str, from_offset: u64, limit: usize) -> Vec<SequencedL2Tx> {
    let mut storage = Storage::open_read_only(db_path).expect("open read-only storage");
    storage
        .ordered_l2_txs_page_from(from_offset, limit)
        .expect("load ordered l2 tx page")
        .into_iter()
        .map(|(_offset, tx)| tx)
        .collect()
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
            panic!("ws message type mismatch; actual={actual:?}, expected={expected:?}")
        }
    }
}
