// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Runtime danger detector.
//!
//! A tiny background task that, every `poll_interval`, asks [`Storage::check_danger`]
//! whether any batch is past the preemptive threshold. If so, the task exits
//! with [`DetectorExit::DangerZone`] — the runtime turns that into a deliberate
//! non-error process shutdown, the orchestrator respawns, and
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
use sequencer_core::protocol::ProtocolConfig;

/// How the detector's loop exited.
///
/// `DangerZone` is a *deliberate* exit — not an error. The runtime maps it to
/// a distinct `RunError` variant so operators can tell "time to recover" apart
/// from "something crashed".
#[derive(Debug)]
pub enum DetectorExit {
    /// Shutdown signal fired before any danger was detected.
    Shutdown,
    /// The strict or wall-clock-adjusted check flagged a batch. Stop for
    /// recovery.
    DangerZone { batch_index: u64 },
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
    protocol: ProtocolConfig,
    poll_interval: Duration,
    shutdown: ShutdownSignal,
}

impl DangerDetector {
    pub fn new(
        db_path: impl Into<String>,
        protocol: ProtocolConfig,
        poll_interval: Duration,
        shutdown: ShutdownSignal,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            protocol,
            poll_interval,
            shutdown,
        }
    }

    pub fn start(
        self,
    ) -> Result<tokio::task::JoinHandle<Result<DetectorExit, DangerDetectorError>>, StorageOpenError>
    {
        let _ = Storage::open_read_only(self.db_path.as_str())?;
        Ok(tokio::spawn(async move { self.run_forever().await }))
    }

    async fn run_forever(self) -> Result<DetectorExit, DangerDetectorError> {
        loop {
            if self.shutdown.is_shutdown_requested() {
                return Ok(DetectorExit::Shutdown);
            }

            match self.check_once().await? {
                DangerStatus::Safe => {
                    debug!("danger check: safe");
                }
                DangerStatus::Strict(batch_index) | DangerStatus::Stalled(batch_index) => {
                    tracing::error!(
                        batch_index,
                        danger_threshold = self.protocol.danger_threshold(),
                        "danger zone detected — triggering shutdown for flush and recovery"
                    );
                    return Ok(DetectorExit::DangerZone { batch_index });
                }
            }

            tokio::select! {
                biased;
                _ = self.shutdown.wait_for_shutdown() => return Ok(DetectorExit::Shutdown),
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
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

    fn test_protocol() -> ProtocolConfig {
        ProtocolConfig {
            batch_submitter: SENDER_A,
            max_wait_blocks: 1200,
            preemptive_margin_blocks: 75,
            seconds_per_block: 12,
        }
    }

    fn make_stale_batch_payload(nonce: u64, safe_block: u64) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce,
            frames: vec![sequencer_core::batch::Frame {
                user_ops: Vec::new(),
                safe_block,
                fee_price: 0,
            }],
        })
    }

    #[tokio::test]
    async fn exits_on_shutdown_when_safe() {
        let db = temp_db("detector-shutdown");
        let mut storage = Storage::open(&db.path).expect("open storage");
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        drop(storage);

        let shutdown = ShutdownSignal::default();
        let detector = DangerDetector::new(
            db.path.clone(),
            test_protocol(),
            Duration::from_millis(50),
            shutdown.clone(),
        );
        let handle = detector.start().expect("start detector");

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
    async fn exits_with_danger_zone_when_strict_check_fires() {
        // Closed frontier batch is aged past `danger_threshold` against the
        // observed safe block — the strict arm of `check_danger` trips.
        let db = temp_db("detector-strict-danger");
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
        storage
            .append_safe_inputs(
                1135,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 20,
                }],
                &protocol,
            )
            .expect("append");
        drop(storage);

        let shutdown = ShutdownSignal::default();
        let detector = DangerDetector::new(
            db.path.clone(),
            protocol,
            Duration::from_millis(50),
            shutdown,
        );
        let handle = detector.start().expect("start detector");

        let exit = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("detector exits within timeout")
            .expect("join")
            .expect("detector result");
        match exit {
            DetectorExit::DangerZone { batch_index } => {
                assert_eq!(batch_index, 1, "closed frontier batch 1 is in danger");
            }
            other => panic!("expected DangerZone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exits_with_danger_zone_when_wall_clock_fallback_fires() {
        // Safe head appears frozen — the strict block-based arm wouldn't trip
        // (ages look fine against the last observed safe block), but the
        // wall-clock-adjusted check infers extended L1 silence and lowers the
        // effective threshold.
        //
        // The detector treats Strict and Stalled identically (both exit with
        // DangerZone), but the Stalled path goes through `wall_clock_adjusted_threshold`
        // — a completely separate code path that deserves its own test.
        let db = temp_db("detector-stalled-danger");
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
        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 100),
                    block_number: 200,
                }],
                &protocol,
            )
            .expect("append accepted batch 0");

        // Strict check: batch 1's first_frame_safe_block = 100, current safe = 1200.
        // age = 1100 < danger_threshold (1125). Strict would NOT fire.
        //
        // Rewind synced_at_ms by 25 blocks' worth of wall-clock time so the
        // wall-clock arm shaves 25 off the threshold (1125 → 1100). At 1100,
        // batch 1's age = 1100 trips `>=`. Stalled fires.
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
        let detector = DangerDetector::new(
            db.path.clone(),
            protocol,
            Duration::from_millis(50),
            shutdown,
        );
        let handle = detector.start().expect("start detector");

        let exit = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("detector exits within timeout")
            .expect("join")
            .expect("detector result");
        match exit {
            DetectorExit::DangerZone { batch_index } => {
                assert_eq!(
                    batch_index, 1,
                    "wall-clock-adjusted check must report the same batch the strict arm would",
                );
            }
            other => panic!("expected DangerZone from wall-clock fallback, got {other:?}"),
        }
    }
}
