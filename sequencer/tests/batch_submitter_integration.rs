// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Integration tests for the batch submitter: worker loop with real storage and mock poster.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use sequencer::l1::submitter::{BatchPoster, BatchPosterError, TxHash};
use sequencer::l1::submitter::{BatchSubmitter, BatchSubmitterConfig};
use sequencer::runtime::shutdown::ShutdownSignal;
use sequencer::storage::{SafeInputRange, Storage};
use sequencer_core::batch::Batch;
use sequencer_core::protocol::ProtocolConfig;

mod common;
use common::{TestDb, temp_db};

/// Minimal mock for integration tests.
///
/// Records submissions. Optionally delays each `submit_batches` call (to race
/// a concurrent writer against the submitter loop), and can fail a configurable
/// number of times before succeeding (to exercise the transient-error retry
/// path).
struct TestMock {
    submissions: Mutex<Vec<(u64, usize)>>,
    /// Per-call delay applied inside `submit_batches`.
    submit_delay: Mutex<Duration>,
    /// Remaining `submit_batches` calls that should return a Provider error
    /// before the real submission path runs.
    fail_next_n_submits: Mutex<u32>,
}

impl TestMock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            submissions: Mutex::new(Vec::new()),
            submit_delay: Mutex::new(Duration::ZERO),
            fail_next_n_submits: Mutex::new(0),
        })
    }

    fn submissions(&self) -> Vec<(u64, usize)> {
        self.submissions.lock().expect("lock").clone()
    }

    fn set_submit_delay(&self, delay: Duration) {
        *self.submit_delay.lock().expect("lock") = delay;
    }

    fn fail_next_n_submits(&self, n: u32) {
        *self.fail_next_n_submits.lock().expect("lock") = n;
    }
}

#[async_trait]
impl BatchPoster for TestMock {
    async fn submit_batches(
        &self,
        payloads: Vec<Vec<u8>>,
    ) -> Result<Vec<TxHash>, BatchPosterError> {
        // Transient-failure hook: consume one of the configured failures
        // before anything else, so the tick outcome maps to `Transient` and
        // the loop must sleep + retry.
        {
            let mut slot = self.fail_next_n_submits.lock().expect("lock");
            if *slot > 0 {
                *slot -= 1;
                return Err(BatchPosterError::Provider(
                    "injected transient failure".into(),
                ));
            }
        }

        let delay = *self.submit_delay.lock().expect("lock");
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        let mut tx_hashes = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let batch_index = ssz::Decode::from_ssz_bytes(payload.as_slice())
                .map(|b: Batch| b.nonce)
                .unwrap_or(0);
            self.submissions
                .lock()
                .expect("lock")
                .push((batch_index, payload.len()));
            tx_hashes.push(TxHash::ZERO);
        }
        Ok(tx_hashes)
    }

    async fn observed_submitted_batch_nonces(
        &self,
        _from_block: u64,
    ) -> Result<Vec<u64>, BatchPosterError> {
        Ok(self
            .submissions
            .lock()
            .expect("lock")
            .iter()
            .map(|(nonce, _)| *nonce)
            .collect())
    }
}

/// Mirrors what `run_preemptive_recovery` does in production: persist a real
/// safe-head observation so `submitter_frontier` has a row to read. Without
/// this, the submitter's first tick errors out on `current_safe_block_required`
/// and the loop exits before submitting anything.
fn seed_observed_safe_head(storage: &mut Storage) {
    let protocol = ProtocolConfig::try_new(
        alloy_primitives::Address::ZERO,
        sequencer_core::MAX_WAIT_BLOCKS,
        75,
        900,
        12,
    )
    .expect("default test protocol");
    storage
        .append_safe_inputs(0, &[], &protocol)
        .expect("seed observed safe head");
}

/// Seeds storage so batches 1 and 2 are closed and batch 3 is open.
fn seed_two_closed_batches(db_path: &str) {
    let mut storage = Storage::open(db_path).expect("open storage");
    seed_observed_safe_head(&mut storage);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    let next_safe = head.safe_block;
    storage
        .close_frame_and_batch(&mut head, next_safe)
        .expect("close batch 0");
    storage
        .close_frame_and_batch(&mut head, next_safe)
        .expect("close batch 1");
    storage
        .close_frame_and_batch(&mut head, next_safe)
        .expect("close batch 2");
}

/// Seeds storage so batch 0 is closed and batch 1 is the open Tip.
fn seed_one_closed_batch(db_path: &str) {
    let mut storage = Storage::open(db_path).expect("open storage");
    seed_observed_safe_head(&mut storage);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    let next_safe = head.safe_block;
    storage
        .close_frame_and_batch(&mut head, next_safe)
        .expect("close batch 0");
}

/// Close the current open Tip so it becomes eligible for submission.
fn close_current_tip(db_path: &str) {
    let mut storage = Storage::open(db_path).expect("open storage");
    let mut head = storage
        .open_state()
        .expect("load open state")
        .expect("open Tip exists");
    let next_safe = head.safe_block;
    storage
        .close_frame_and_batch(&mut head, next_safe)
        .expect("close current Tip");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitter_loop_submits_closed_batches_then_exits_on_shutdown() {
    let TestDb { _dir, path } = temp_db("loop-submits");
    seed_two_closed_batches(&path);

    let mock = TestMock::new();
    let shutdown = ShutdownSignal::default();
    let config = BatchSubmitterConfig {
        idle_poll_interval_ms: 5000,
    };
    let submitter = BatchSubmitter::new(path, mock.clone(), shutdown.clone(), config);
    let handle = submitter.start().expect("start batch submitter");

    // Allow at least one tick to run (worker may submit batch 1 and 2 in one tick).
    tokio::time::sleep(Duration::from_millis(200)).await;

    shutdown.request_shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    let submissions = mock.submissions();
    assert!(
        submissions.len() >= 3,
        "submitter should have submitted at least batch 0, 1, and 2, got {:?}",
        submissions
    );
    assert_eq!(submissions[0].0, 0, "first submission should be batch 0");
    assert_eq!(submissions[1].0, 1, "second submission should be batch 1");
    assert_eq!(submissions[2].0, 2, "third submission should be batch 2");
}

// ── Loop cadence invariants ───────────────────────────────────────────────
//
// These pin the behavior the two-worker refactor unlocked:
//   - Submitted → re-enter IMMEDIATELY (no sleep).
//   - Transient (Poster error) → log + sleep + retry (loop must NOT exit).
//
// Both are loop-level properties that aren't visible from `tick_once`.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitter_re_enters_immediately_after_productive_tick() {
    // Design of the test:
    //
    //   t=0ms   Submitter starts. Tick 1 loads batch 0, enters submit_batches
    //           which sleeps for `submit_delay` (400ms) before recording.
    //   t=100   A concurrent writer closes the Tip, making batch 1 eligible.
    //   t~400   submit_batches returns. Tick 1 outcome is Submitted(1).
    //           Loop must re-enter IMMEDIATELY (Submitted branch → `continue`).
    //   t~400   Tick 2 observes the new batch 1, submits it (another 400ms).
    //   t~800   submit_batches returns again, Submitted(1).
    //   t=1200  Test asserts: two submissions landed inside the window.
    //
    // If `Submitted → sleep idle_poll` ever regresses, tick 2 would wait 10s
    // and the second submission would not appear in the 1.2s budget.
    let TestDb { _dir, path } = temp_db("loop-immediate-retry");
    seed_one_closed_batch(&path);

    let mock = TestMock::new();
    mock.set_submit_delay(Duration::from_millis(400));
    let shutdown = ShutdownSignal::default();
    let config = BatchSubmitterConfig {
        // Ten seconds — anything above ~2s would be enough to fail if the
        // immediate-retry cadence regressed to always-sleep.
        idle_poll_interval_ms: 10_000,
    };
    let submitter = BatchSubmitter::new(path.clone(), mock.clone(), shutdown.clone(), config);
    let handle = submitter.start().expect("start batch submitter");

    // Let tick 1 enter `submit_batches` (which is now blocking on the delay),
    // then close the Tip so batch 1 is eligible by the time tick 2 runs.
    tokio::time::sleep(Duration::from_millis(100)).await;
    close_current_tip(&path);

    // Budget: ~2x the submit delay. With immediate-retry this is plenty.
    tokio::time::sleep(Duration::from_millis(1100)).await;

    shutdown.request_shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    let submissions = mock.submissions();
    assert_eq!(
        submissions.len(),
        2,
        "Submitted-then-new-work must re-enter without sleeping idle_poll=10s; \
         got submissions {submissions:?}"
    );
    assert_eq!(submissions[0].0, 0);
    assert_eq!(submissions[1].0, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitter_recovers_from_transient_poster_error_without_exiting() {
    // Design of the test:
    //
    //   t=0ms   Submitter starts. Tick 1 calls submit_batches, which returns
    //           a Provider error (the first of N injected failures).
    //   t=0ms   Loop maps Err(Poster) → TickOutcome::Transient → sleep idle_poll.
    //   t~80ms  Tick 2 runs. submit_batches succeeds, batch 0 recorded.
    //   t=250ms Test asserts: exactly 1 submission AND loop is still alive.
    //
    // Regressions this catches:
    //   - Propagating Poster errors as fatal (loop would exit; handle would
    //     resolve with BatchSubmitterError before shutdown fires).
    //   - Forgetting the sleep on Transient (would work, but could busy-loop
    //     on a persistent error — not tested here, but the retry-count path
    //     documents the intended cadence).
    let TestDb { _dir, path } = temp_db("loop-transient-retry");
    seed_one_closed_batch(&path);

    let mock = TestMock::new();
    mock.fail_next_n_submits(1);
    let shutdown = ShutdownSignal::default();
    let config = BatchSubmitterConfig {
        // Short poll interval so the retry sleep completes well within the
        // test window. Still long enough that accidentally always-sleeping
        // would delay the single submission past the assertion.
        idle_poll_interval_ms: 50,
    };
    let submitter = BatchSubmitter::new(path.clone(), mock.clone(), shutdown.clone(), config);
    let handle = submitter.start().expect("start batch submitter");

    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(
        !handle.is_finished(),
        "loop must not exit on a transient Poster error — it should log and retry",
    );

    let submissions = mock.submissions();
    assert_eq!(
        submissions.len(),
        1,
        "transient failure followed by success should land exactly one submission; got {submissions:?}",
    );
    assert_eq!(submissions[0].0, 0);

    shutdown.request_shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
