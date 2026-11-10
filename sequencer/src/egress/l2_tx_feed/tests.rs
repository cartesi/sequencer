// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::Path;
use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, B256, Signature};
use tokio::sync::oneshot;

use super::{BroadcastTxMessage, L2TxFeed, L2TxFeedConfig, SubscribeError};
use crate::egress::test_era;
use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp, SequencerError};
use crate::runtime::process_lock::{ProcessLock, ProcessLockError};
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::{IngestedSafeInput, SafeInputRange, Storage};
use crate::test_support::{pin_test_deployment_identity, temp_db};
use sequencer_core::history::{HistoryClaim, HistoryPolicyError, L2TxCount, RecoveryGeneration};
use sequencer_core::l2_tx::{Direct, UserOpTx};
use sequencer_core::user_op::UserOp;

#[test]
fn broadcast_user_op_serializes_with_hex_data() {
    let msg = BroadcastTxMessage::from_user_op(
        7,
        &UserOpTx {
            sender: Address::from_slice(&[0x11; 20]),
            nonce: 11,
            fee: 3,
            block: 1_234,
            data: vec![0xaa, 0xbb],
        },
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
    let msg = BroadcastTxMessage::from_direct(
        9,
        &Direct {
            input_index: 3,
            sender: Address::ZERO,
            block: 42,
            timestamp: 1_700_000_000,
            payload: vec![0xcc, 0xdd],
        },
        5,
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
async fn subscription_refuses_ahead_and_stale_claims() {
    let db = temp_db("subscription-claims");
    seed_ordered_txs(&db.path);
    let feed = test_feed(&db.path, RuntimeScope::default());
    assert!(matches!(feed.subscribe_from(claim(&db.path, 3)).await,
        Err(SubscribeError::History(HistoryPolicyError::AheadOfHead { head })) if head.get() == 2));
    let mut stale = claim(&db.path, 0);
    stale.version.recovery_generation = RecoveryGeneration::new(1);
    assert!(matches!(
        feed.subscribe_from(stale).await,
        Err(SubscribeError::History(
            HistoryPolicyError::StaleGeneration { .. }
        ))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_accepts_more_than_fifty_thousand_inputs_without_a_total_cap() {
    let db = temp_db("subscription-deep-history");
    let mut storage = Storage::open(&db.path).unwrap();
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    let inputs: Vec<_> = (0..50_001)
        .map(|offset| {
            let (respond_to, _) = oneshot::channel();
            IncludedUserOp {
                pending: PendingUserOp {
                    signed: sequencer_core::user_op::SignedUserOp {
                        sender: Address::repeat_byte(0x11),
                        signature: Signature::test_signature(),
                        user_op: UserOp {
                            nonce: offset,
                            max_fee: u16::MAX,
                            data: vec![0x42].into(),
                        },
                    },
                    respond_to,
                    received_at: SystemTime::now(),
                },
                l2_tx_index: L2TxCount::new(u64::from(offset)),
            }
        })
        .collect();
    storage
        .append_included_user_ops(&mut head, &inputs)
        .unwrap();
    drop(storage);
    let feed = L2TxFeed::new(
        db.path.clone(),
        RuntimeScope::default(),
        L2TxFeedConfig {
            page_size: 2,
            ..Default::default()
        },
    );
    let mut subscription = feed.subscribe_from(claim(&db.path, 0)).await.unwrap();
    for expected in 0..3 {
        let event = tokio::time::timeout(Duration::from_secs(2), subscription.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.offset(), expected);
    }
    subscription.finish().await.unwrap();
}

/// Subscribe from `from` and read every row through `end`, one feed per call
/// so each page size starts a fresh cursor.
async fn read_feed(db_path: &str, from: u64, end: u64, page_size: usize) -> Vec<serde_json::Value> {
    let feed = L2TxFeed::new(
        db_path.to_string(),
        RuntimeScope::default(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size,
        },
    );
    let mut subscription = feed.subscribe_from(claim(db_path, from)).await.unwrap();
    let mut messages = Vec::new();
    for _ in from..end {
        let message = tokio::time::timeout(Duration::from_secs(2), subscription.recv())
            .await
            .expect("wait for a feed row")
            .expect("the feed is open");
        messages.push(serde_json::to_value(message).unwrap());
    }
    subscription.finish().await.unwrap();
    messages
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feed_serves_the_rebuilt_history_then_the_live_rows_across_the_seam() {
    let db = temp_db("feed-rebuilt-era");
    let mut storage = test_era::seed_rebuilt_era(&db.path, Path::new("/unused/baseline"));
    let mut expected: Vec<_> = (test_era::BASE..=test_era::REBUILT_END)
        .map(test_era::expected_feed_message)
        .collect();
    expected.push(test_era::append_live_user_op(&mut storage, 2));
    expected.push(test_era::append_live_user_op(&mut storage, 3));
    drop(storage);
    let head = test_era::BASE + expected.len() as u64;
    assert_eq!(head, 48);

    // From the base, the middle of the rebuilt range, its last row, its end
    // (the first live row) and past it, with page sizes that split the seam.
    for from in [41, 43, 44, 45, 46] {
        for page_size in [1, 2, 3, 64] {
            let messages = read_feed(&db.path, from, head, page_size).await;
            let skip = usize::try_from(from - test_era::BASE).unwrap();
            assert_eq!(
                messages,
                expected[skip..],
                "from {from}, page size {page_size}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feed_refuses_a_claim_below_the_rebuilt_eras_base() {
    let db = temp_db("feed-rebuilt-era-below-base");
    drop(test_era::seed_rebuilt_era(
        &db.path,
        Path::new("/unused/baseline"),
    ));
    let feed = test_feed(&db.path, RuntimeScope::default());
    for next in [0, test_era::BASE - 1] {
        assert!(matches!(
            feed.subscribe_from(claim(&db.path, next)).await,
            Err(SubscribeError::History(HistoryPolicyError::HistoryUnavailable { available_from }))
                if available_from.get() == test_era::BASE
        ));
    }
}

#[test]
fn cancelled_subscription_prepare_retains_process_lock_until_blocking_read_finishes() {
    let db = temp_db("cancelled-subscription-prepare-lock");
    seed_ordered_txs(db.path.as_str());
    let data_dir = db._dir.path().to_str().expect("utf8 data dir").to_string();
    let db_path = db.path.clone();
    let start = claim(&db_path, 0);
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
            feed.subscribe_from(start).await
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
            "detached subscription preparation must retain process ownership"
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
        .expect("detached subscription preparation should release ownership");
        drop(reacquired);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_replays_existing_rows_in_order() {
    let db = temp_db("replay-existing");
    seed_ordered_txs(db.path.as_str());
    let feed = test_feed(db.path.as_str(), RuntimeScope::default());

    let mut subscription = feed
        .subscribe_from(claim(&db.path, 0))
        .await
        .expect("subscribe");

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
            offset: 0,
            nonce: 7,
            safe_block: 123,
            batch_nonce: 1,
            ..
        }
    ));
    assert!(matches!(
        second,
        BroadcastTxMessage::DirectInput {
            offset: 1,
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
async fn shutdown_signal_closes_subscription() {
    let db = temp_db("shutdown-closes");
    seed_ordered_txs(db.path.as_str());
    let shutdown = RuntimeScope::default();
    let feed = test_feed(db.path.as_str(), shutdown.clone());

    let mut subscription = feed
        .subscribe_from(claim(&db.path, 2))
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
#[cfg(unix)]
async fn corrupt_feed_head_trips_terminal_storage_fault() {
    if !crate::runtime::shutdown::abort_test_child(
        "egress::l2_tx_feed::tests::corrupt_feed_head_trips_terminal_storage_fault",
    ) {
        return;
    }
    let db = temp_db("corrupt-feed-head");
    seed_ordered_txs(db.path.as_str());
    let conn = Storage::open_connection(db.path.as_str()).expect("raw connection");
    let start = claim(&db.path, 0);
    // An era id of the wrong length: the head read decodes it and fails with
    // a storage error, not a panic. The era is immutable and its CHECK bounds
    // the length, so both are lifted for the corruption.
    conn.execute_batch(
        "PRAGMA ignore_check_constraints = ON; \
         DROP TRIGGER trg_era_immutable; \
         UPDATE era SET era_id = X'0102';",
    )
    .expect("corrupt the era id");
    drop(conn);

    let shutdown = RuntimeScope::default();
    let feed = test_feed(db.path.as_str(), shutdown.clone());

    let _ = feed.subscribe_from(start).await;
    panic!("corrupt feed head returned instead of aborting");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn corrupt_feed_page_trips_terminal_storage_fault() {
    if !crate::runtime::shutdown::abort_test_child(
        "egress::l2_tx_feed::tests::corrupt_feed_page_trips_terminal_storage_fault",
    ) {
        return;
    }
    let db = temp_db("corrupt-feed-page");
    seed_ordered_txs(db.path.as_str());
    let conn = Storage::open_connection(db.path.as_str()).expect("raw connection");
    // A missing frame makes the page's LEFT JOIN read NULL frame columns (fee,
    // safe block), which the decoder refuses as a wrongly typed stored value.
    conn.execute_batch("PRAGMA foreign_keys = OFF; DELETE FROM frames;")
        .expect("drop the frames under stored rows");
    drop(conn);

    let shutdown = RuntimeScope::default();
    let feed = test_feed(db.path.as_str(), shutdown.clone());
    let mut subscription = feed
        .subscribe_from(claim(&db.path, 0))
        .await
        .expect("subscribe");

    let _ = subscription.recv().await;
    panic!("corrupt feed page returned instead of aborting");
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
        },
    )
}

fn seed_ordered_txs(db_path: &str) {
    let mut storage = Storage::open(db_path).expect("open storage");
    pin_test_deployment_identity(&mut storage, NO_OWN_BATCHES);
    let mut head = storage
        .initialize_open_state(123, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    storage
        .close_frame_and_batch(&mut head)
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
        .append_included_user_ops(
            &mut head,
            &[IncludedUserOp {
                pending,
                l2_tx_index: L2TxCount::ZERO,
            }],
        )
        .expect("append user-op chunk");
    storage
        .append_ingested_safe_inputs_with_timestamp(
            456,
            456,
            &[IngestedSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 456,
                block_timestamp: 1_700_000_000,
                transaction_hash: B256::repeat_byte(0xcd),
            }],
            &crate::storage::test_helpers::default_protocol_timing(),
        )
        .expect("append direct input");
    storage
        .close_frame_only(&mut head, 456, SafeInputRange::new(0, 1))
        .expect("close frame with one drained direct input");
}

fn claim(db_path: &str, next: u64) -> HistoryClaim {
    let mut storage = Storage::open_read_only(db_path).unwrap();
    HistoryClaim {
        version: storage.history_version().unwrap(),
        next_input: L2TxCount::new(next),
    }
}
