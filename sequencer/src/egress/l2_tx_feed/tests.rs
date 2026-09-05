// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, B256, Signature};
use tokio::sync::oneshot;

use super::{BroadcastTxMessage, L2TxFeed, L2TxFeedConfig, SubscribeError, SubscriptionError};
use crate::ingress::inclusion_lane::{PendingUserOp, SequencerError};
use crate::runtime::process_lock::{ProcessLock, ProcessLockError};
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::test_helpers::temp_db;
use crate::storage::{FrontierMode, IngestedSafeInput, SafeInputRange, Storage, StoredSafeInput};
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
use sequencer_core::user_op::UserOp;

#[test]
fn broadcast_user_op_serializes_with_hex_data() {
    let msg = BroadcastTxMessage::from_user_op(
        7,
        ValidUserOp {
            sender: Address::from_slice(&[0x11; 20]),
            fee: 3,
            data: vec![0xaa, 0xbb],
        },
        11,
        1_234,
        5,
    );
    let json = serde_json::to_string(&msg).expect("serialize");
    assert!(json.contains("\"kind\":\"user_op\""));
    assert!(json.contains("\"offset\":7"));
    assert!(json.contains("\"nonce\":11"));
    assert!(json.contains("\"fee\":3"));
    assert!(json.contains("\"data\":\"0xaabb\""));
    assert!(json.contains("\"safe_block\":1234"));
    assert!(json.contains("\"batch_nonce\":5"));
}

#[test]
fn broadcast_direct_input_serializes_with_hex_payload() {
    let msg = BroadcastTxMessage::from_direct_input(
        9,
        DirectInput {
            sender: Address::ZERO,
            block_number: 42,
            payload: vec![0xcc, 0xdd],
        },
        3,
        5,
        1_700_000_000,
        B256::repeat_byte(0xab),
    );
    let json = serde_json::to_string(&msg).expect("serialize");
    assert!(json.contains("\"kind\":\"direct_input\""));
    assert!(json.contains("\"offset\":9"));
    assert!(json.contains("\"sender\":\"0x0000000000000000000000000000000000000000\""));
    assert!(json.contains("\"block_number\":42"));
    assert!(json.contains("\"payload\":\"0xccdd\""));
    assert!(json.contains("\"input_index\":3"));
    assert!(json.contains("\"batch_nonce\":5"));
    assert!(json.contains("\"block_timestamp\":1700000000"));
    assert!(json.contains(&format!("\"transaction_hash\":\"0x{}\"", "ab".repeat(32))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_from_rejects_catchup_window() {
    let db = temp_db("catchup-window");
    seed_ordered_txs(db.path.as_str());
    append_direct_input(db.path.as_str());
    let feed = test_feed(db.path.as_str(), RuntimeScope::default());

    let result = feed.subscribe_from(1, 1).await;

    assert!(matches!(
        result,
        Err(SubscribeError::CatchUpWindowExceeded {
            requested_offset: 1,
            live_start_offset: 3,
            max_catchup_events: 1,
        })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_from_accepts_exact_catchup_window() {
    let db = temp_db("catchup-window-exact");
    seed_ordered_txs(db.path.as_str());
    let feed = test_feed(db.path.as_str(), RuntimeScope::default());

    let subscription = feed.subscribe_from(0, 2).await;

    assert!(
        subscription.is_ok(),
        "exactly 2 replayable events should be allowed"
    );
}

#[test]
fn cancelled_catchup_prepare_retains_process_lock_until_blocking_read_finishes() {
    let db = temp_db("cancelled-catchup-prepare-lock");
    seed_ordered_txs(db.path.as_str());
    let data_dir = db._dir.path().to_str().expect("utf8 data dir").to_string();
    let db_path = db.path.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("build test runtime");

    runtime.block_on(async move {
        let process_lock = ProcessLock::acquire(&data_dir).expect("acquire process lock");
        let feed = test_feed(&db_path, RuntimeScope::new(process_lock));

        // Occupy the only blocking thread so subscription preparation is
        // deterministically queued, then cancel the async task awaiting it.
        let (blocker_started_tx, blocker_started_rx) = oneshot::channel();
        let (release_blocker_tx, release_blocker_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = blocker_started_tx.send(());
            release_blocker_rx.recv().expect("release blocking pool");
        });
        blocker_started_rx.await.expect("blocking pool occupied");

        let (subscribe_entered_tx, subscribe_entered_rx) = oneshot::channel();
        let subscribe = tokio::spawn(async move {
            let _ = subscribe_entered_tx.send(());
            feed.subscribe_from(0, u64::MAX).await
        });
        subscribe_entered_rx
            .await
            .expect("subscription preparation entered");
        subscribe.abort();
        let join = match subscribe.await {
            Ok(_) => panic!("subscription task should be cancelled"),
            Err(join) => join,
        };
        assert!(join.is_cancelled());

        assert!(
            matches!(
                ProcessLock::acquire(&data_dir),
                Err(ProcessLockError::Locked { .. })
            ),
            "detached catch-up preparation must retain process ownership"
        );

        release_blocker_tx.send(()).expect("release blocking pool");
        blocker.await.expect("join blocking-pool occupant");

        let reacquired = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match ProcessLock::acquire(&data_dir) {
                    Ok(lock) => break lock,
                    Err(ProcessLockError::Locked { .. }) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected lock acquisition failure: {error}"),
                }
            }
        })
        .await
        .expect("detached catch-up preparation should release ownership");
        drop(reacquired);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_replays_existing_rows_in_order() {
    let db = temp_db("replay-existing");
    seed_ordered_txs(db.path.as_str());
    let feed = test_feed(db.path.as_str(), RuntimeScope::default());

    let mut subscription = feed.subscribe_from(0, u64::MAX).await.expect("subscribe");

    let first = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait first event")
        .expect("first event");
    let second = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait second event")
        .expect("second event");

    assert!(matches!(
        first,
        BroadcastTxMessage::UserOp {
            offset: 1,
            nonce: 7,
            safe_block: 123,
            batch_nonce: 1,
            ..
        }
    ));
    assert!(matches!(
        second,
        BroadcastTxMessage::DirectInput {
            offset: 2,
            input_index: 0,
            batch_nonce: 1,
            block_timestamp: 1_700_000_000,
            transaction_hash,
            ..
        } if transaction_hash == B256::repeat_byte(0xcd).to_string()
    ));

    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_filters_batch_submitter_safe_inputs() {
    let db = temp_db("filters-batch-submitter-inputs");
    let batch_submitter_address = Address::from([0xfe; 20]);
    seed_ordered_txs_with_sender(db.path.as_str(), batch_submitter_address);
    let feed = L2TxFeed::new(
        db.path.clone(),
        RuntimeScope::default(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            ..L2TxFeedConfig::new(batch_submitter_address)
        },
    );

    let mut subscription = feed.subscribe_from(0, u64::MAX).await.expect("subscribe");
    let first = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait first event")
        .expect("first event");

    // DB offsets start at 1. The user op is the first sequenced tx (offset=1),
    // and the batch submitter's safe input (offset=2) is filtered out.
    assert!(matches!(
        first,
        BroadcastTxMessage::UserOp { offset: 1, .. }
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
    let db = temp_db("shutdown-closes");
    seed_ordered_txs(db.path.as_str());
    let shutdown = RuntimeScope::default();
    let feed = test_feed(db.path.as_str(), shutdown.clone());

    let mut subscription = feed
        .subscribe_from(u64::MAX, u64::MAX)
        .await
        .expect("subscribe");

    shutdown.request_shutdown();

    assert!(
        tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("wait for subscription close")
            .is_none()
    );
    subscription.finish().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_fault_discards_already_queued_subscription_events() {
    let db = temp_db("terminal-discards-queued-feed");
    seed_ordered_txs(db.path.as_str());
    let shutdown = RuntimeScope::default();
    let feed = test_feed(db.path.as_str(), shutdown.clone());
    let mut subscription = feed.subscribe_from(0, u64::MAX).await.expect("subscribe");
    tokio::time::sleep(Duration::from_millis(20)).await;

    shutdown.contain_storage_invariant_failure("test fault");

    assert!(
        subscription.recv().await.is_none(),
        "biased shutdown must outrank a replay event queued before terminal publication"
    );
    subscription
        .finish()
        .await
        .expect("clean terminal shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_feed_head_trips_terminal_storage_fault() {
    let db = temp_db("corrupt-feed-head");
    seed_ordered_txs(db.path.as_str());
    let conn = Storage::open_connection(db.path.as_str()).expect("raw connection");
    conn.execute("UPDATE sequenced_l2_txs SET offset = -offset", [])
        .expect("corrupt offsets");
    drop(conn);

    let shutdown = RuntimeScope::default();
    let feed = test_feed(db.path.as_str(), shutdown.clone());

    assert!(matches!(
        feed.subscribe_from(0, u64::MAX).await,
        Err(SubscribeError::StorageInvariantViolation)
    ));
    assert!(shutdown.is_storage_invariant_contained());
    assert!(shutdown.is_shutdown_requested());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_feed_page_trips_terminal_storage_fault() {
    let db = temp_db("corrupt-feed-page");
    seed_ordered_txs(db.path.as_str());
    let conn = Storage::open_connection(db.path.as_str()).expect("raw connection");
    conn.execute("UPDATE frames SET safe_block = 'not-an-integer'", [])
        .expect("corrupt safe-block storage type");
    drop(conn);

    let shutdown = RuntimeScope::default();
    let feed = test_feed(db.path.as_str(), shutdown.clone());
    let subscription = feed.subscribe_from(0, u64::MAX).await.expect("subscribe");

    tokio::time::timeout(Duration::from_secs(1), shutdown.wait_for_shutdown())
        .await
        .expect("terminal fault containment requests shutdown");
    assert!(shutdown.is_storage_invariant_contained());
    assert!(shutdown.is_shutdown_requested());
    assert!(matches!(
        subscription.finish().await,
        Err(SubscriptionError::StorageInvariantViolation)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catchup_window_not_inflated_by_invalidated_batch_holes() {
    // Regression test: after batch invalidation, offset holes in sequenced_l2_txs
    // must not inflate the catch-up event count. The check should count actual
    // valid events, not subtract rowids.
    let db = temp_db("catchup-holes");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");

    // Create two closed batches, each with one direct input.
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }],
            Address::ZERO,
            &sequencer_core::protocol::ProtocolTiming {
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
        )
        .expect("append direct 0");
    storage
        .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
        .expect("close frame");
    storage
        .close_frame_and_batch(&mut head, 10)
        .expect("close batch 0");

    storage
        .append_safe_inputs(
            20,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 20,
            }],
            Address::ZERO,
            &sequencer_core::protocol::ProtocolTiming {
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
        )
        .expect("append direct 1");
    storage
        .close_frame_only(&mut head, 20, SafeInputRange::new(1, 2))
        .expect("close frame");
    drop(storage);

    // Before invalidation: 2 valid events.
    // With max_catchup_events=1, subscribing from 0 should fail.
    let feed = test_feed(db.path.as_str(), RuntimeScope::default());
    assert!(
        feed.subscribe_from(0, 1).await.is_err(),
        "should reject: 2 valid events > max 1"
    );

    // Invalidate batch 0 — this creates a hole in the offset space.
    // Now only 1 valid event remains (from batch 1).
    let mut storage = Storage::open(db.path.as_str()).expect("reopen storage");
    storage.insert_invalid_batch(0).expect("invalidate batch 0");
    drop(storage);

    // After invalidation: only 1 valid event, so max_catchup_events=1 should succeed.
    let feed = test_feed(db.path.as_str(), RuntimeScope::default());
    assert!(
        feed.subscribe_from(0, 1).await.is_ok(),
        "should accept: only 1 valid event after invalidation, despite rowid hole"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catchup_window_excludes_batch_submitter_direct_inputs() {
    // Regression test: batch-submitter direct inputs are filtered before WS
    // delivery, so the catch-up window must not count them. Otherwise a
    // reconnecting client could be rejected even when the number of
    // replayable messages is within the limit.
    let db = temp_db("catchup-submitter-filter");
    let batch_submitter = Address::from([0xfe; 20]);
    let user_address = Address::from([0x01; 20]);

    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");

    // Two direct inputs: one from the batch submitter, one from a user.
    storage
        .append_safe_inputs(
            10,
            &[
                StoredSafeInput {
                    sender: batch_submitter,
                    payload: vec![0xaa],
                    block_number: 10,
                },
                StoredSafeInput {
                    sender: user_address,
                    payload: vec![0xbb],
                    block_number: 10,
                },
            ],
            Address::ZERO,
            &sequencer_core::protocol::ProtocolTiming {
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
        )
        .expect("append directs");
    storage
        .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
        .expect("close frame");
    drop(storage);

    // With a submitter address that matches no seeded sender: 2 events,
    // max=1 should reject.
    let feed_no_filter = L2TxFeed::new(
        db.path.clone(),
        RuntimeScope::default(),
        L2TxFeedConfig::new(NO_OWN_BATCHES),
    );
    assert!(
        feed_no_filter.subscribe_from(0, 1).await.is_err(),
        "without filter: 2 events > max 1"
    );

    // With batch_submitter_address filtering: only the user's event counts.
    let feed_filtered = L2TxFeed::new(
        db.path.clone(),
        RuntimeScope::default(),
        L2TxFeedConfig::new(batch_submitter),
    );
    assert!(
        feed_filtered.subscribe_from(0, 1).await.is_ok(),
        "with filter: only 1 broadcastable event, should accept"
    );
}

/// Sentinel submitter for fixtures that seed no own-batch rows. Must not
/// collide with any seeded sender (`seed_ordered_txs` uses `Address::ZERO`).
const NO_OWN_BATCHES: Address = Address::repeat_byte(0x7f);

fn test_feed(db_path: &str, shutdown: RuntimeScope) -> L2TxFeed {
    L2TxFeed::new(
        db_path.to_string(),
        shutdown,
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            ..L2TxFeedConfig::new(NO_OWN_BATCHES)
        },
    )
}

fn seed_ordered_txs(db_path: &str) {
    seed_ordered_txs_with_sender(db_path, Address::ZERO);
}

fn seed_ordered_txs_with_sender(db_path: &str, direct_sender: Address) {
    let mut storage = Storage::open(db_path).expect("open storage");
    let mut head = storage
        .initialize_open_state(123, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    storage
        .close_frame_and_batch(&mut head, 123)
        .expect("advance to batch nonce 1");

    let (respond_to, _recv) = oneshot::channel::<Result<(), SequencerError>>();
    let pending = PendingUserOp {
        signed: sequencer_core::user_op::SignedUserOp {
            sender: Address::from_slice(&[0x11; 20]),
            signature: Signature::test_signature(),
            user_op: UserOp {
                nonce: 7,
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
        .append_ingested_safe_inputs_with_timestamp(
            456,
            456,
            &[IngestedSafeInput {
                sender: direct_sender,
                payload: vec![0xaa],
                block_number: 456,
                block_timestamp: 1_700_000_000,
                transaction_hash: B256::repeat_byte(0xcd),
            }],
            Address::ZERO,
            &sequencer_core::protocol::ProtocolTiming {
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
            FrontierMode::Populate,
        )
        .expect("append direct input");
    storage
        .close_frame_only(&mut head, 456, SafeInputRange::new(0, 1))
        .expect("close frame with one drained direct input");
}

fn append_direct_input(db_path: &str) {
    let mut storage = Storage::open(db_path).expect("open storage");
    let mut head = storage
        .open_state()
        .expect("load open state")
        .expect("open state exists");
    storage
        .append_ingested_safe_inputs_with_timestamp(
            789,
            789,
            &[IngestedSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 789,
                block_timestamp: 1_700_000_001,
                transaction_hash: B256::repeat_byte(0xef),
            }],
            Address::ZERO,
            &sequencer_core::protocol::ProtocolTiming {
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
            FrontierMode::Populate,
        )
        .expect("append second direct input");
    storage
        .close_frame_only(&mut head, 789, SafeInputRange::new(1, 2))
        .expect("close frame with second direct input");
}
