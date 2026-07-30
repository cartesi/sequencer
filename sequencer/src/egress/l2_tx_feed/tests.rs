// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, Signature};
use tokio::sync::oneshot;

use super::{BroadcastTxMessage, L2TxFeed, L2TxFeedConfig, SubscribeError};
use crate::ingress::inclusion_lane::{PendingUserOp, SequencerError};
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::test_helpers::temp_db;
use crate::storage::{DeploymentIdentity, SafeInputRange, Storage, StoredSafeInput, WriteHead};
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
        1234,
        5,
        None,
        Some(11),
        None,
        None,
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
    let msg = BroadcastTxMessage::from_offset_and_tx(
        9,
        SequencedL2Tx::Direct(DirectInput {
            sender: Address::ZERO,
            block_number: 42,
            payload: vec![0xcc, 0xdd],
        }),
        1234,
        5,
        Some(3),
        None,
        Some(1700000000),
        Some(alloy_primitives::B256::from_slice(&[0xab; 32])),
    );
    let json = serde_json::to_string(&msg).expect("serialize");
    assert!(json.contains("\"kind\":\"direct_input\""));
    assert!(json.contains("\"offset\":9"));
    assert!(json.contains("\"sender\":\"0x0000000000000000000000000000000000000000\""));
    assert!(json.contains("\"block_number\":42"));
    assert!(json.contains("\"payload\":\"0xccdd\""));
    assert!(json.contains("\"input_index\":3"));
    assert!(json.contains("\"block_timestamp\":1700000000"));
    assert!(json.contains(&format!("\"transaction_hash\":\"0x{}\"", "ab".repeat(32))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_from_rejects_catchup_window() {
    let db = temp_db("catchup-window");
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
async fn subscribe_from_accepts_exact_catchup_window() {
    let db = temp_db("catchup-window-exact");
    seed_ordered_txs(db.path.as_str());
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());

    let subscription = feed.subscribe_from(0, 2);

    assert!(
        subscription.is_ok(),
        "exactly 2 replayable events should be allowed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_from_executed_input_count_skips_applied_prefix() {
    let db = temp_db("executed-input-count-boundary");
    seed_two_user_op_batches(db.path.as_str());
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());

    let mut subscription = feed
        .subscribe_from_executed_input_count(1, 1)
        .expect("subscribe after one executed input");
    let event = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait for batch 1 event")
        .expect("batch 1 event");

    assert!(matches!(
        event,
        BroadcastTxMessage::UserOp {
            offset: 2,
            batch_nonce: 1,
            ..
        }
    ));

    let no_second = tokio::time::timeout(Duration::from_millis(50), subscription.recv()).await;
    assert!(
        no_second.is_err(),
        "the first executed input must not be replayed"
    );
    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executed_input_count_ignores_empty_batches() {
    let db = temp_db("empty-batch-boundary");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    append_test_user_op(&mut storage, &mut head, 0x10);
    storage
        .close_frame_and_batch(&mut head, 0)
        .expect("close batch 0");
    storage
        .close_frame_and_batch(&mut head, 0)
        .expect("close empty batch 1");
    append_test_user_op(&mut storage, &mut head, 0x12);
    drop(storage);
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());

    let mut subscription = feed
        .subscribe_from_executed_input_count(1, 1)
        .expect("subscribe after one executed input");
    let event = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait for batch 2 event")
        .expect("batch 2 event");

    assert!(matches!(
        event,
        BroadcastTxMessage::UserOp {
            offset: 2,
            batch_nonce: 2,
            ..
        }
    ));
    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executed_input_count_excludes_batch_submission_rows() {
    let db = temp_db("executed-input-count-excludes-batches");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let batch_submitter = Address::repeat_byte(0x99);
    storage
        .load_or_insert_deployment_identity(test_identity(batch_submitter))
        .expect("pin identity");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage
        .append_safe_inputs(
            10,
            &[
                StoredSafeInput {
                    sender: batch_submitter,
                    payload: vec![0xBA],
                    block_number: 10,
                    ..Default::default()
                },
                StoredSafeInput {
                    sender: Address::repeat_byte(0x22),
                    payload: vec![0xDA],
                    block_number: 10,
                    ..Default::default()
                },
            ],
            batch_submitter,
            &crate::storage::test_helpers::default_protocol_timing(),
        )
        .expect("seed batch and direct");
    storage
        .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
        .expect("sequence safe inputs");
    storage
        .close_frame_and_batch(&mut head, 10)
        .expect("close first batch");
    append_test_user_op(&mut storage, &mut head, 0x12);
    drop(storage);

    let feed = L2TxFeed::new(
        db.path.clone(),
        ShutdownSignal::default(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            batch_submitter_address: Some(batch_submitter),
        },
    );
    let mut subscription = feed
        .subscribe_from_executed_input_count(2, u64::MAX)
        .expect("subscribe after direct and user op");
    let no_event = tokio::time::timeout(Duration::from_millis(50), subscription.recv()).await;
    assert!(
        no_event.is_err(),
        "the batch-submission row must not consume an executed-input count"
    );
    subscription.finish().await.expect("finish subscription");
}

#[test]
fn executed_input_count_rejects_count_before_recovery_anchor() {
    let db = temp_db("executed-input-count-before-anchor");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .append_safe_inputs(
            0,
            &[StoredSafeInput {
                sender: Address::repeat_byte(0x22),
                payload: vec![0xAA],
                block_number: 0,
                ..Default::default()
            }],
            Address::repeat_byte(0x99),
            &crate::storage::test_helpers::default_protocol_timing(),
        )
        .expect("seed recovery direct");
    storage.open_recovery_tip(0, 7).expect("open recovery root");
    drop(storage);
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());

    let result = feed.subscribe_from_executed_input_count(6, u64::MAX);

    assert!(matches!(
        result,
        Err(SubscribeError::ExecutedInputCountBeforeAnchor {
            requested_executed_input_count: 6,
            minimum_executed_input_count: 7,
        })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_anchor_skips_already_applied_padding_directs() {
    let db = temp_db("executed-input-count-recovery-anchor");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let batch_submitter = Address::repeat_byte(0x99);
    storage
        .load_or_insert_deployment_identity(test_identity(batch_submitter))
        .expect("pin identity");
    storage
        .append_safe_inputs(
            0,
            &[
                StoredSafeInput {
                    sender: batch_submitter,
                    payload: vec![0xBA],
                    block_number: 0,
                    ..Default::default()
                },
                StoredSafeInput {
                    sender: Address::repeat_byte(0x22),
                    payload: vec![0xAA],
                    block_number: 0,
                    ..Default::default()
                },
            ],
            batch_submitter,
            &crate::storage::test_helpers::default_protocol_timing(),
        )
        .expect("seed recovery padding");
    storage.open_recovery_tip(0, 7).expect("open recovery root");
    let mut head = storage.open_state().expect("load").expect("root tip");
    let feed = L2TxFeed::new(
        db.path.clone(),
        ShutdownSignal::default(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            batch_submitter_address: Some(batch_submitter),
        },
    );
    let mut subscription = feed
        .subscribe_from_executed_input_count(7, u64::MAX)
        .expect("subscribe at recovered state");

    let no_padding = tokio::time::timeout(Duration::from_millis(50), subscription.recv()).await;
    assert!(
        no_padding.is_err(),
        "recovery padding is already reflected in the recovered state"
    );

    append_test_user_op(&mut storage, &mut head, 0x12);
    let event = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait for first post-recovery event")
        .expect("post-recovery event");
    assert!(matches!(
        event,
        BroadcastTxMessage::UserOp { offset: 3, .. }
    ));
    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_anchor_resolves_minimum_on_valid_root_after_cascade() {
    let db = temp_db("executed-input-count-recovery-anchor-cascade");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let batch_submitter = Address::repeat_byte(0x99);
    storage
        .load_or_insert_deployment_identity(test_identity(batch_submitter))
        .expect("pin identity");
    storage
        .append_safe_inputs(
            0,
            &[StoredSafeInput {
                sender: Address::repeat_byte(0x22),
                payload: vec![0xAA],
                block_number: 0,
                ..Default::default()
            }],
            batch_submitter,
            &crate::storage::test_helpers::default_protocol_timing(),
        )
        .expect("seed recovery padding");
    storage.open_recovery_tip(0, 7).expect("open recovery root");
    storage
        .append_safe_inputs(
            1200,
            &[],
            batch_submitter,
            &crate::storage::test_helpers::default_protocol_timing(),
        )
        .expect("advance safe head");

    let invalidated = storage
        .recover_aging_tip(1200)
        .expect("cascade recovered root");
    assert_eq!(invalidated, vec![0]);
    let mut recovered_head = storage
        .open_state()
        .expect("load")
        .expect("replacement recovery tip");
    append_test_user_op(&mut storage, &mut recovered_head, 0x12);
    drop(storage);

    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());
    let mut subscription = feed
        .subscribe_from_executed_input_count(7, u64::MAX)
        .expect("subscribe at recovered state");
    let event = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait for first post-recovery event")
        .expect("post-recovery event");

    assert!(matches!(
        event,
        BroadcastTxMessage::UserOp { offset: 3, .. }
    ));
    let no_padding = tokio::time::timeout(Duration::from_millis(50), subscription.recv()).await;
    assert!(
        no_padding.is_err(),
        "replacement recovery padding is already reflected in the recovered state"
    );
    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executed_input_count_reuses_boundary_after_tip_recovery() {
    let db = temp_db("executed-input-count-tip-recovery");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut head = storage
        .initialize_open_state(10, SafeInputRange::empty_at(0))
        .expect("initialize");
    append_test_user_op(&mut storage, &mut head, 0x10);
    storage
        .close_frame_and_batch(&mut head, 10)
        .expect("close valid batch 0");
    append_test_user_op(&mut storage, &mut head, 0x11);
    storage
        .append_safe_inputs(
            1210,
            &[],
            Address::repeat_byte(0x99),
            &crate::storage::test_helpers::default_protocol_timing(),
        )
        .expect("advance safe head");

    let invalidated = storage.recover_aging_tip(1200).expect("recover aging tip");
    assert_eq!(invalidated, vec![1]);
    let mut recovered_head = storage
        .open_state()
        .expect("load recovered state")
        .expect("recovery tip");
    append_test_user_op(&mut storage, &mut recovered_head, 0x12);
    drop(storage);

    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());
    let mut subscription = feed
        .subscribe_from_executed_input_count(1, 1)
        .expect("subscribe after the valid ancestor input");
    let event = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait for recovery-branch event")
        .expect("recovery-branch event");

    assert!(matches!(
        event,
        BroadcastTxMessage::UserOp {
            offset: 3,
            batch_nonce: 1,
            ..
        }
    ));
    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_executed_input_count_skips_intermediate_live_inputs() {
    let db = temp_db("future-executed-input-count");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());
    let mut subscription = feed
        .subscribe_from_executed_input_count(2, u64::MAX)
        .expect("subscribe from future executed input count");

    append_test_user_op(&mut storage, &mut head, 0x10);
    storage
        .close_frame_and_batch(&mut head, 0)
        .expect("close batch 0");
    append_test_user_op(&mut storage, &mut head, 0x11);
    storage
        .close_frame_and_batch(&mut head, 0)
        .expect("close batch 1");

    let no_early_event = tokio::time::timeout(Duration::from_millis(50), subscription.recv()).await;
    assert!(
        no_early_event.is_err(),
        "the first two application inputs must stay filtered"
    );

    append_test_user_op(&mut storage, &mut head, 0x12);
    let event = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .expect("wait for batch 2 event")
        .expect("batch 2 event");
    assert!(matches!(
        event,
        BroadcastTxMessage::UserOp {
            offset: 3,
            batch_nonce: 2,
            ..
        }
    ));

    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_replays_existing_rows_in_order() {
    let db = temp_db("replay-existing");
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

    // DB offsets (SQLite rowid) start at 1.
    assert_eq!(first.offset(), 1);
    assert_eq!(second.offset(), 2);

    subscription.finish().await.expect("finish subscription");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_filters_batch_submitter_safe_inputs() {
    let db = temp_db("filters-batch-submitter-inputs");
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
                ..Default::default()
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
                ..Default::default()
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
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());
    assert!(
        feed.subscribe_from(0, 1).is_err(),
        "should reject: 2 valid events > max 1"
    );

    // Invalidate batch 0 — this creates a hole in the offset space.
    // Now only 1 valid event remains (from batch 1).
    let mut storage = Storage::open(db.path.as_str()).expect("reopen storage");
    storage.insert_invalid_batch(0).expect("invalidate batch 0");
    drop(storage);

    // After invalidation: only 1 valid event, so max_catchup_events=1 should succeed.
    let feed = test_feed(db.path.as_str(), ShutdownSignal::default());
    assert!(
        feed.subscribe_from(0, 1).is_ok(),
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
                    ..Default::default()
                },
                StoredSafeInput {
                    sender: user_address,
                    payload: vec![0xbb],
                    block_number: 10,
                    ..Default::default()
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

    // Without batch_submitter_address filtering: 2 events, max=1 should reject.
    let feed_no_filter = L2TxFeed::new(
        db.path.clone(),
        ShutdownSignal::default(),
        L2TxFeedConfig {
            batch_submitter_address: None,
            ..L2TxFeedConfig::default()
        },
    );
    assert!(
        feed_no_filter.subscribe_from(0, 1).is_err(),
        "without filter: 2 events > max 1"
    );

    // With batch_submitter_address filtering: only the user's event counts.
    let feed_filtered = L2TxFeed::new(
        db.path.clone(),
        ShutdownSignal::default(),
        L2TxFeedConfig {
            batch_submitter_address: Some(batch_submitter),
            ..L2TxFeedConfig::default()
        },
    );
    assert!(
        feed_filtered.subscribe_from(0, 1).is_ok(),
        "with filter: only 1 broadcastable event, should accept"
    );
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

fn test_identity(batch_submitter_address: Address) -> DeploymentIdentity {
    DeploymentIdentity {
        chain_id: 1,
        app_address: Address::repeat_byte(0x11),
        input_box_address: Address::repeat_byte(0x22),
        input_box_genesis_block: 0,
        batch_submitter_address,
    }
}

fn seed_ordered_txs(db_path: &str) {
    seed_ordered_txs_with_sender(db_path, Address::ZERO);
}

fn seed_ordered_txs_with_sender(db_path: &str, direct_sender: Address) {
    let mut storage = Storage::open(db_path).expect("open storage");
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
                ..Default::default()
            }],
            Address::ZERO,
            &sequencer_core::protocol::ProtocolTiming {
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

fn seed_two_user_op_batches(db_path: &str) {
    let mut storage = Storage::open(db_path).expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    append_test_user_op(&mut storage, &mut head, 0x20);
    storage
        .close_frame_and_batch(&mut head, 0)
        .expect("close batch 0");
    append_test_user_op(&mut storage, &mut head, 0x21);
}

fn append_test_user_op(storage: &mut Storage, head: &mut WriteHead, data: u8) {
    let (respond_to, _recv) = oneshot::channel::<Result<(), SequencerError>>();
    storage
        .append_user_ops_chunk(
            head,
            &[PendingUserOp {
                signed: sequencer_core::user_op::SignedUserOp {
                    sender: Address::from_slice(&[data; 20]),
                    signature: Signature::test_signature(),
                    user_op: UserOp {
                        nonce: 0,
                        max_fee: 3,
                        data: vec![data].into(),
                    },
                },
                respond_to,
                received_at: SystemTime::now(),
            }],
        )
        .expect("append test user op");
}
