// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Runtime danger detector.
//!
//! A tiny background task that, every `poll_interval`, asks [`Storage::check_danger`]
//! whether recovery or refusal is needed. If so, the task exits with
//! [`DetectorExit::RecoveryRequired`] — the runtime turns that into a
//! deliberate non-error process shutdown, the orchestrator respawns, and
//! `run_preemptive_recovery` takes over on startup.
//!
//! This is its own worker (not part of the batch submitter) because the two
//! concerns are orthogonal: the submitter makes progress on L1, which involves
//! slow confirmations; the detector just reads the DB + wall clock at a fixed
//! cadence. Keeping them separate means one never delays the other, and each
//! stays a ~20-line state machine.
//!
//! Detection is eventually consistent with the input reader: a transition into
//! danger may lag by up to one `poll_interval`. The preemptive margin absorbs
//! this bounded lag.

use std::time::Duration;

use thiserror::Error;
use tracing::debug;

use crate::runtime::clock::unix_now_ms;
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{DangerStatus, Storage, StorageOpenError};
use sequencer_core::protocol::ProtocolTiming;

/// How the detector's loop exited.
///
/// `RecoveryRequired` is a *deliberate* exit — not an error. The runtime maps
/// it to a distinct `RunError` variant so operators can tell "time to recover
/// or refuse startup" apart from "something crashed".
#[derive(Debug)]
pub enum DetectorExit {
    /// Shutdown signal fired before any danger was detected.
    Shutdown,
    /// A non-safe danger status was observed. Stop and let startup dispatch
    /// the recovery/refusal path from a fresh read.
    RecoveryRequired { status: DangerStatus },
}

#[derive(Debug, Error)]
pub enum DangerDetectorError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("danger detector join error: {0}")]
    Join(String),
}

pub struct DangerDetector {
    db_path: String,
    protocol: ProtocolTiming,
    poll_interval: Duration,
}

impl DangerDetector {
    pub fn new(
        db_path: impl Into<String>,
        protocol: ProtocolTiming,
        poll_interval: Duration,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            protocol,
            poll_interval,
        }
    }

    /// Spawn the detector loop. The `shutdown` signal is what the loop
    /// respects; passing it at start time (instead of construction time) keeps
    /// the construction phase pure.
    pub fn start(
        self,
        shutdown: ShutdownSignal,
    ) -> Result<tokio::task::JoinHandle<Result<DetectorExit, DangerDetectorError>>, StorageOpenError>
    {
        let _ = Storage::open_read_only(self.db_path.as_str())?;
        Ok(tokio::spawn(
            async move { self.run_forever(shutdown).await },
        ))
    }

    /// Top-level driver. Races the work loop against the shutdown signal.
    ///
    /// `biased;` polls the shutdown arm first on every wakeup so a concurrent
    /// shutdown wins over an in-flight `run_loop` step. Without `biased`,
    /// `select!` would pick randomly between two ready branches and could
    /// process one more iteration before shutting down.
    async fn run_forever(
        self,
        shutdown: ShutdownSignal,
    ) -> Result<DetectorExit, DangerDetectorError> {
        tokio::select! {
            biased;
            _ = shutdown.wait_for_shutdown() => Ok(DetectorExit::Shutdown),
            result = self.run_loop() => result,
        }
    }

    /// Tick → sleep → tick. Returns `RecoveryRequired` when a non-Safe danger
    /// status fires. Shutdown is handled by the outer `run_forever` select,
    /// so this loop has no shutdown concerns.
    async fn run_loop(self) -> Result<DetectorExit, DangerDetectorError> {
        loop {
            match self.check_once().await? {
                DangerStatus::Safe => {
                    debug!("danger check: safe");
                }
                status => {
                    // All non-Safe variants exit for recovery/refusal. The
                    // dispatch difference (flush vs no-flush vs refuse)
                    // only matters at the next startup — `decide_startup_action`
                    // re-runs `check_danger` and routes based on which variant
                    // fires this time.
                    tracing::error!(
                        ?status,
                        danger_threshold = self.protocol.danger_threshold(),
                        l1_read_stale_after_blocks = self.protocol.l1_read_stale_after_blocks,
                        "danger detected — triggering shutdown for startup recovery"
                    );
                    return Ok(DetectorExit::RecoveryRequired { status });
                }
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn check_once(&self) -> Result<DangerStatus, DangerDetectorError> {
        let db_path = self.db_path.clone();
        let protocol = self.protocol;
        let now_ms = unix_now_ms();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            storage
                .check_danger(&protocol, now_ms)
                .map_err(DangerDetectorError::from)
        })
        .await
        .map_err(|err| DangerDetectorError::Join(err.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::{SENDER_A, temp_db};
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
    use std::time::Duration;

    fn test_protocol() -> ProtocolTiming {
        ProtocolTiming {
            max_wait_blocks: 1200,
            preemptive_margin_blocks: 75,
            l1_read_stale_after_blocks: 900,
            seconds_per_block: 12,
        }
    }

    #[tokio::test]
    async fn exits_on_shutdown_when_safe() {
        let db = temp_db("detector-shutdown");
        let mut storage = Storage::open(&db.path).expect("open storage");
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .append_safe_inputs(10, &[], SENDER_A, &test_protocol())
            .expect("record fresh safe-head observation");
        drop(storage);

        let shutdown = ShutdownSignal::default();
        let detector =
            DangerDetector::new(db.path.clone(), test_protocol(), Duration::from_millis(50));
        let handle = detector.start(shutdown.clone()).expect("start detector");

        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.request_shutdown();
        let exit = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("detector exits within timeout")
            .expect("join")
            .expect("detector result");
        assert!(matches!(exit, DetectorExit::Shutdown));
    }

    #[tokio::test]
    async fn exits_with_recovery_required_when_observed_closed_check_fires() {
        // Closed frontier batch is aged past `danger_threshold` against the
        // observed safe block — the closed-batch arm of `check_danger` trips.
        let db = temp_db("detector-observed-closed-danger");
        let mut storage = Storage::open(&db.path).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 1");

        let protocol = test_protocol();
        let landed = crate::storage::test_helpers::local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                1135,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 20,
                    ..Default::default()
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append");
        drop(storage);

        let shutdown = ShutdownSignal::default();
        let detector = DangerDetector::new(db.path.clone(), protocol, Duration::from_millis(50));
        let handle = detector.start(shutdown).expect("start detector");

        let exit = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("detector exits within timeout")
            .expect("join")
            .expect("detector result");
        match exit {
            DetectorExit::RecoveryRequired { status } => {
                assert_eq!(status, DangerStatus::ClosedBatchInDanger(1));
            }
            other => panic!("expected recovery-required exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exits_with_recovery_required_when_wall_clock_fallback_fires() {
        // Safe head appears frozen — observed block-based checks wouldn't trip
        // (ages look fine against the last observed safe block), but the
        // wall-clock-adjusted check infers extended L1 silence and lowers the
        // effective threshold.
        //
        // The detector treats observed and estimated danger identically (both
        // exit for startup recovery), but the estimated path goes through
        // `wall_clock_adjusted_danger_threshold`
        // — a completely separate code path that deserves its own test.
        let db = temp_db("detector-estimated-danger");
        let mut storage = Storage::open(&db.path).expect("open storage");
        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");

        let protocol = test_protocol();
        let landed = crate::storage::test_helpers::local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 200,
                    ..Default::default()
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append accepted batch 0");

        // Observed check: batch 1's first_frame_safe_block = 100, current
        // safe = 1200. age = 1100 < danger_threshold (1125), so observed
        // closed-batch danger would NOT fire.
        //
        // Rewind synced_at_ms by 25 blocks' worth of wall-clock time so the
        // wall-clock arm shaves 25 off the threshold (1125 → 1100). At 1100,
        // batch 1's age = 1100 trips `>=`. Estimated batch danger fires.
        let now_ms = crate::runtime::clock::unix_now_ms();
        drop(storage);
        let rewind_conn =
            Storage::open_connection(&db.path).expect("open raw connection to rewind synced_at_ms");
        rewind_conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms.saturating_sub(25 * 12 * 1000)).unwrap_or(i64::MAX)],
            )
            .expect("rewind safe-progress timestamp");
        drop(rewind_conn);

        let shutdown = ShutdownSignal::default();
        let detector = DangerDetector::new(db.path.clone(), protocol, Duration::from_millis(50));
        let handle = detector.start(shutdown).expect("start detector");

        let exit = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("detector exits within timeout")
            .expect("join")
            .expect("detector result");
        match exit {
            DetectorExit::RecoveryRequired { status } => {
                assert_eq!(status, DangerStatus::EstimatedBatchInDanger(1));
            }
            other => {
                panic!("expected recovery-required exit from wall-clock fallback, got {other:?}")
            }
        }
    }
}
