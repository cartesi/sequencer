// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, Signature};
use tempfile::TempDir;
use tokio::sync::oneshot;

use super::{BroadcastTxMessage, L2TxFeed, L2TxFeedConfig, SubscribeError};
use crate::inclusion_lane::{PendingUserOp, SequencerError};
use crate::shutdown::ShutdownSignal;
use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
use sequencer_core::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};
use sequencer_core::user_op::UserOp;

#[test]
fn broadcast_user_op_serializes_with_hex_data() {
    let msg = BroadcastTxMessage::from_offset_and_tx(
        7,
        SequencedL2Tx::UserOp(ValidUserOp {
            sender: Address::from_slice(&[0x11; 20]),
            fee: 3,
            data: vec![0xaa, 0xbb],
        }),
    );
    let json = serde_json::to_string(&msg).expect("serialize");
    assert!(json.contains("\"kind\":\"user_op\""));
    assert!(json.contains("\"offset\":7"));
    assert!(json.contains("\"fee\":3"));
    assert!(json.contains("\"data\":\"0xaabb\""));
}

#[test]
fn broadcast_direct_input_serializes_with_hex_payload() {
    let msg = BroadcastTxMessage::from_offset_and_tx(
        9,
        SequencedL2Tx::Direct(DirectInput {
            sender: Address::ZERO,
            block_number: 42,
            payload: vec![0xcc, 0xdd],
        }),
    );
    let json = serde_json::to_string(&msg).expect("serialize");
    assert!(json.contains("\"kind\":\"direct_input\""));
    assert!(json.contains("\"offset\":9"));
    assert!(json.contains("\"sender\":\"0x0000000000000000000000000000000000000000\""));
    assert!(json.contains("\"block_number\":42"));
    assert!(json.contains("\"payload\":\"0xccdd\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_from_rejects_catchup_window() {
    let db = test_db("catchup-window");
    seed_ordered_txs(db.path.as_str());
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());

    let result = feed.subscribe_from(0, 1);

    assert!(matches!(
        result,
        Err(SubscribeError::CatchUpWindowExceeded {
            requested_offset: 0,
            live_start_offset: 2,
            max_catchup_events: 1,
        })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_replays_existing_rows_in_order() {
    let db = test_db("replay-existing");
    seed_ordered_txs(db.path.as_str());
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());

    let mut subscription = feed.subscribe_from(0, u64::MAX).expect("subscribe");

    let first = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait first event")
        .expect("first event");
    let second = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait second event")
        .expect("second event");

    assert_eq!(first.offset(), 0);
    assert_eq!(second.offset(), 1);

    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_filters_batch_submitter_safe_inputs() {
    let db = test_db("filters-batch-submitter-inputs");
    let batch_submitter_address = Address::from([0xfe; 20]);
    seed_ordered_txs_with_sender(db.path.as_str(), batch_submitter_address);
    let feed = L2TxFeed::new(
        db.path.clone(),
        ShutdownSignal::default(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            batch_submitter_address: Some(batch_submitter_address),
        },
    );

    let mut subscription = feed.subscribe_from(0, u64::MAX).expect("subscribe");
    let first = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait first event")
        .expect("first event");

    assert!(matches!(
        first,
        BroadcastTxMessage::UserOp { offset: 0, .. }
    ));

    let no_second = tokio::time::timeout(Duration::from_millis(50), subscription.recv()).await;
    assert!(
        no_second.is_err(),
        "filtered batch-submitter input should not be broadcast"
    );

    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_signal_closes_subscription() {
    let db = test_db("shutdown-closes");
    seed_ordered_txs(db.path.as_str());
    let shutdown = ShutdownSignal::default();
    let feed = test_feed(db.path.as_str(), shutdown.clone());

    let mut subscription = feed.subscribe_from(u64::MAX, u64::MAX).expect("subscribe");

    shutdown.request_shutdown();

    assert!(
        tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("wait for subscription close")
            .is_none()
    );
    subscription.finish().await.expect("clean shutdown");
}

fn test_feed(db_path: &str, shutdown: ShutdownSignal) -> L2TxFeed {
    L2TxFeed::new(
        db_path.to_string(),
        shutdown,
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            batch_submitter_address: None,
        },
    )
}

fn test_db(label: &str) -> TestDb {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join(format!("{label}.db"));
    TestDb {
        _dir: dir,
        path: path.to_string_lossy().into_owned(),
    }
}

fn seed_ordered_txs(db_path: &str) {
    seed_ordered_txs_with_sender(db_path, Address::ZERO);
}

fn seed_ordered_txs_with_sender(db_path: &str, direct_sender: Address) {
    let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");

    let (respond_to, _recv) = oneshot::channel::<Result<(), SequencerError>>();
    let pending = PendingUserOp {
        signed: sequencer_core::user_op::SignedUserOp {
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
                sender: direct_sender,
                payload: vec![0xaa],
                block_number: 10,
            }],
        )
        .expect("append direct input");
    storage
        .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
        .expect("close frame with one drained direct input");
}

struct TestDb {
    _dir: TempDir,
    path: String,
}
