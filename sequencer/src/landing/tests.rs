// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use super::*;
use crate::storage::test_helpers::{
    LandingFixture, SENDER_A, open_temp_storage, record_canonical_divergence,
};
use crate::storage::{Divergence, SafeInputRange};
use crate::test_support::TestDb;

/// The window the test identity pins (the wallet's).
const W: u64 = app_core::application::WALLET_SCHEDULER_LIMITS.max_wait_blocks;
const POLL: Duration = Duration::from_millis(20);

/// One open batch whose first frame is at safe block 10, under a safe head
/// at `safe_block` stamped with L1 timestamp `timestamp`.
fn open_batch_at_10(name: &str, safe_block: u64, timestamp: u64) -> TestDb {
    let (db, mut storage) = open_temp_storage(name);
    storage
        .initialize_open_state(10, SafeInputRange::empty_at(0))
        .expect("open batch 0");
    storage
        .append_safe_inputs_with_timestamp(safe_block, timestamp, &[], SENDER_A)
        .expect("safe head");
    db
}

fn start(
    db: &TestDb,
) -> (
    tokio::task::JoinHandle<Result<LandingWatchExit, LandingWatchError>>,
    LandingFeed,
    ShutdownSignal,
) {
    let (watch, feed) = LandingWatch::new(db.path.clone(), W, POLL, ProcessLock::test());
    let shutdown = ShutdownSignal::default();
    let handle = watch.start(shutdown.clone()).expect("start the watch");
    (handle, feed, shutdown)
}

async fn exit_of(
    handle: tokio::task::JoinHandle<Result<LandingWatchExit, LandingWatchError>>,
) -> LandingWatchExit {
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the watch exits within the timeout")
        .expect("join")
        .expect("the watch's result")
}

/// Inside the window the watch publishes the reading and keeps running until
/// shutdown.
#[tokio::test]
async fn publishes_the_reading_inside_the_window_and_stops_on_shutdown() {
    let db = open_batch_at_10("watch-inside-window", 100, 1_000);
    let (handle, mut feed, shutdown) = start(&db);
    assert_eq!(
        feed.borrow().lag,
        None,
        "before the first tick only the window is known"
    );
    assert_eq!(feed.borrow().max_wait_blocks, W);

    tokio::time::timeout(Duration::from_secs(5), feed.changed())
        .await
        .expect("the first tick publishes")
        .expect("the watch is alive");
    let reading = *feed.borrow();
    assert_eq!(
        reading,
        LandingReading {
            lag: Some(LandingLag {
                batch_nonce: 0,
                first_frame_safe_block: 10,
                safe_block: 100,
            }),
            max_wait_blocks: W,
            safe_block_timestamp: Some(1_000),
        }
    );
    assert_eq!(reading.landing_lag_blocks(), Some(91));
    assert_eq!(reading.safe_head_age_seconds(1_500_999), Some(500));
    assert_eq!(
        reading.safe_head_age_seconds(999_000),
        Some(0),
        "a safe block ahead of the clock saturates at 0"
    );

    // Several more ticks inside the window: still running.
    tokio::time::sleep(POLL * 5).await;
    assert!(!handle.is_finished());
    shutdown.request_shutdown();
    assert!(matches!(exit_of(handle).await, LandingWatchExit::Shutdown));
}

/// The watch stops the process on a void history that arises while it runs:
/// the safe head reaches the block from which no later block can accept the
/// oldest unaccepted batch.
#[tokio::test]
async fn stops_on_a_void_history_reached_while_running() {
    let db = open_batch_at_10("watch-void-while-running", 10 + W - 2, 1_000);
    let (handle, mut feed, _shutdown) = start(&db);
    tokio::time::timeout(Duration::from_secs(5), feed.changed())
        .await
        .expect("the first tick publishes")
        .expect("the watch is alive");
    assert_eq!(feed.borrow().landing_lag_blocks(), Some(W - 1));
    assert!(
        !handle.is_finished(),
        "one block short of void, it keeps running"
    );

    Storage::open(&db.path)
        .expect("open writer")
        .advance_safe_head(10 + W - 1);
    match exit_of(handle).await {
        LandingWatchExit::Terminal(TerminalFact::VoidHistory {
            lag,
            max_wait_blocks,
        }) => {
            assert_eq!(lag.batch_nonce, 0);
            assert_eq!(lag.first_frame_safe_block, 10);
            assert_eq!(lag.safe_block, 10 + W - 1);
            assert_eq!(max_wait_blocks, W);
        }
        other => panic!("expected a void history, got {other:?}"),
    }
}

#[tokio::test]
async fn stops_on_a_divergence() {
    let db = open_batch_at_10("watch-divergence", 100, 1_000);
    let (handle, mut feed, _shutdown) = start(&db);
    tokio::time::timeout(Duration::from_secs(5), feed.changed())
        .await
        .expect("the first tick publishes")
        .expect("the watch is alive");

    let mut storage = Storage::open(&db.path).expect("open writer");
    record_canonical_divergence(&mut storage, 7, 3);
    drop(storage);
    assert!(matches!(
        exit_of(handle).await,
        LandingWatchExit::Terminal(TerminalFact::Divergence(Divergence {
            nonce: 7,
            safe_input_index: 3,
        }))
    ));
}

/// A void history already recorded stops the watch on its first tick.
#[tokio::test]
async fn stops_on_its_first_tick_over_a_void_history() {
    let db = open_batch_at_10("watch-void-at-start", 10 + W, 1_000);
    let (handle, feed, _shutdown) = start(&db);
    assert!(matches!(
        exit_of(handle).await,
        LandingWatchExit::Terminal(TerminalFact::VoidHistory { .. })
    ));
    assert_eq!(
        feed.borrow().lag,
        None,
        "a terminal tick publishes no reading"
    );
}
