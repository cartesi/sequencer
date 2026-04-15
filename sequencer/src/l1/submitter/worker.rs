// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter worker: stateless, at-least-once submission to L1.
//!
//! On each tick the worker:
//! 1. Assigns nonces to any un-nonced valid batches (via `batch_nonces` table).
//! 2. Checks if any valid batch is in the danger zone — triggers shutdown if found.
//! 3. Queries L1 for the next expected batch nonce.
//! 4. Loads the valid unresolved suffix with nonce >= next expected.
//! 5. Submits the pending suffix to L1 with incrementing wallet nonces.
//! 6. Waits for confirmations or timeout, then loops.

use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Address;
use thiserror::Error;
use tracing::{debug, error};

use crate::l1::submitter::{BatchPoster, BatchPosterError, BatchSubmitterConfig};
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{PendingBatch, Storage, StorageOpenError};

#[derive(Debug, Error)]
pub enum BatchSubmitterError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("batch submitter join error: {0}")]
    Join(String),
    #[error(transparent)]
    Poster(#[from] BatchPosterError),
    #[error(
        "danger zone: batch {batch_index} approaching staleness — sequencer must flush and recover"
    )]
    DangerZone { batch_index: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    Idle,
    Submitted { count: usize },
}

pub struct BatchSubmitter<P: BatchPoster> {
    db_path: String,
    batch_submitter_address: Address,
    poster: Arc<P>,
    idle_poll_interval: Duration,
    max_wait_blocks: u64,
    danger_threshold: u64,
    seconds_per_block: u64,
    shutdown: ShutdownSignal,
}

impl<P: BatchPoster + 'static> BatchSubmitter<P> {
    pub fn new(
        db_path: impl Into<String>,
        batch_submitter_address: Address,
        poster: Arc<P>,
        shutdown: ShutdownSignal,
        config: BatchSubmitterConfig,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            batch_submitter_address,
            poster,
            idle_poll_interval: config.idle_poll_interval(),
            max_wait_blocks: config.max_wait_blocks,
            danger_threshold: config.danger_threshold(),
            seconds_per_block: config.seconds_per_block,
            shutdown,
        }
    }

    pub fn start(
        self,
    ) -> Result<tokio::task::JoinHandle<Result<(), BatchSubmitterError>>, StorageOpenError> {
        let _ = Storage::open_read_only(self.db_path.as_str())?;
        Ok(tokio::spawn(async move { self.run_forever().await }))
    }

    async fn run_forever(self) -> Result<(), BatchSubmitterError> {
        loop {
            if self.shutdown.is_shutdown_requested() {
                return Ok(());
            }

            match self.tick_once().await {
                Ok(TickOutcome::Submitted { .. }) => continue,
                Ok(TickOutcome::Idle) => {}
                Err(BatchSubmitterError::Poster(source)) => {
                    error!(error = %source, "L1 provider error — will retry");

                    // Wall-clock danger check: read last_l1_sync_ms from DB and
                    // estimate how many blocks have passed since. Same logic as
                    // the startup check — stateless, reads from DB each time.
                    let in_danger = crate::recovery::wall_clock_danger_estimate(
                        &self.db_path,
                        self.batch_submitter_address,
                        crate::recovery::RecoveryParams {
                            max_wait_blocks: self.max_wait_blocks,
                            danger_threshold: self.danger_threshold,
                            seconds_per_block: self.seconds_per_block,
                        },
                    );
                    match in_danger {
                        Ok(Some(batch_index)) => {
                            return Err(BatchSubmitterError::DangerZone { batch_index });
                        }
                        Ok(None) => {} // safe to retry
                        Err(e) => {
                            error!(error = %e, "wall-clock danger check failed");
                        }
                    }
                }
                Err(err) => return Err(err),
            }

            tokio::select! {
                _ = self.shutdown.wait_for_shutdown() => return Ok(()),
                _ = tokio::time::sleep(self.idle_poll_interval) => {}
            }
        }
    }

    pub(crate) async fn tick_once(&self) -> Result<TickOutcome, BatchSubmitterError> {
        // Refresh `safe_accepted_batches` + `batch_nonces` so the danger check and
        // pending-batch query observe the latest L1 frontier.
        self.refresh_recovery_metadata().await?;

        // Crash on danger zone so the startup sequence can flush the mempool and recover.
        self.check_danger_zone().await?;

        // Step 3: Derive the next unresolved batch nonce from the safe frontier plus
        // latest-chain mined submissions beyond that safe prefix.
        let next_nonce = {
            let (safe_block, safe_next_expected) =
                self.load_safe_next_expected_batch_nonce().await?;

            let recent_observed_nonces = self
                .poster
                .observed_submitted_batch_nonces(safe_block.saturating_add(1))
                .await?;
            advance_expected_batch_nonce(safe_next_expected, recent_observed_nonces)
        };

        // Step 4: Load the unresolved suffix (all valid batches with nonce >= next_nonce).
        let pending = self.load_pending_batches(next_nonce).await?;
        if pending.is_empty() {
            return Ok(TickOutcome::Idle);
        }

        // Step 5: Submit the whole suffix in one shot, then let the poster wait for
        // confirmations serially. Using latest mined submissions plus the latest L1
        // account nonce makes the next tick naturally replace unresolved txs at the
        // same wallet nonces after a timeout.
        for batch in &pending {
            debug!(
                batch_index = batch.batch_index,
                nonce = batch.nonce,
                "queueing batch for L1 submission"
            );
        }
        let submitted_batches: Vec<(u64, u64)> =
            pending.iter().map(|b| (b.batch_index, b.nonce)).collect();
        let payloads: Vec<Vec<u8>> = pending.into_iter().map(|b| b.encoded).collect();
        let tx_hashes = self.poster.submit_batches(payloads).await?;
        if tx_hashes.len() != submitted_batches.len() {
            return Err(BatchSubmitterError::Poster(BatchPosterError::Provider(
                format!(
                    "poster returned {} tx hashes for {} submitted batches",
                    tx_hashes.len(),
                    submitted_batches.len()
                ),
            )));
        }

        Ok(TickOutcome::Submitted {
            count: submitted_batches.len(),
        })
    }

    async fn load_safe_next_expected_batch_nonce(&self) -> Result<(u64, u64), BatchSubmitterError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            storage
                .load_safe_accepted_frontier()
                .map_err(BatchSubmitterError::from)
        })
        .await
        .map_err(|err| BatchSubmitterError::Join(err.to_string()))?
    }

    async fn refresh_recovery_metadata(&self) -> Result<(), BatchSubmitterError> {
        let db_path = self.db_path.clone();
        let batch_submitter_address = self.batch_submitter_address;
        let max_wait_blocks = self.max_wait_blocks;
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path, "NORMAL")?;
            storage
                .refresh_recovery_metadata(batch_submitter_address, max_wait_blocks)
                .map_err(BatchSubmitterError::from)
        })
        .await
        .map_err(|err| BatchSubmitterError::Join(err.to_string()))?
    }

    async fn check_danger_zone(&self) -> Result<(), BatchSubmitterError> {
        let db_path = self.db_path.clone();
        let danger_threshold = self.danger_threshold;
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            if let Some(batch_index) = storage.check_danger_zone(danger_threshold)? {
                tracing::error!(
                    batch_index,
                    danger_threshold,
                    "danger zone detected — triggering shutdown for flush and recovery"
                );
                return Err(BatchSubmitterError::DangerZone { batch_index });
            }
            Ok(())
        })
        .await
        .map_err(|err| BatchSubmitterError::Join(err.to_string()))?
    }

    async fn load_pending_batches(
        &self,
        min_nonce: u64,
    ) -> Result<Vec<PendingBatch>, BatchSubmitterError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            storage
                .load_pending_batches(min_nonce)
                .map_err(BatchSubmitterError::from)
        })
        .await
        .map_err(|err| BatchSubmitterError::Join(err.to_string()))?
    }
}

/// Advance `expected` past any contiguous run of matching nonces in the input.
/// Assumes `observed_nonces` are in chronological (L1 event) order — out-of-order
/// inputs cause early termination, which is correct (the gap means a nonce is missing).
fn advance_expected_batch_nonce(
    mut expected: u64,
    observed_nonces: impl IntoIterator<Item = u64>,
) -> u64 {
    for nonce in observed_nonces {
        if nonce == expected {
            expected = expected.saturating_add(1);
        }
    }
    expected
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::Address;

    use crate::l1::submitter::{
        BatchSubmitterConfig, BatchSubmitterError, TickOutcome, poster::mock::MockBatchPoster,
    };
    use crate::runtime::shutdown::ShutdownSignal;
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
    use tempfile::TempDir;

    const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";
    const BATCH_SUBMITTER_ADDRESS: Address = Address::repeat_byte(0x11);

    fn temp_db(name: &str) -> (TempDir, String) {
        let dir = tempfile::Builder::new()
            .prefix(format!("sequencer-batch-submitter-{name}-").as_str())
            .tempdir()
            .expect("create temporary test directory");
        let path = dir.path().join("sequencer.sqlite");
        (dir, path.to_string_lossy().into_owned())
    }

    fn seed_two_closed_batches(db_path: &str) {
        let mut storage = Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
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

    fn seed_safe_submitted_batches(db_path: &str, safe_block: u64, nonces: &[u64]) {
        let mut storage = Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        let inputs: Vec<_> = nonces
            .iter()
            .map(|nonce| StoredSafeInput {
                sender: BATCH_SUBMITTER_ADDRESS,
                payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                    nonce: *nonce,
                    frames: Vec::new(),
                }),
                block_number: safe_block,
            })
            .collect();
        storage
            .append_safe_inputs(safe_block, inputs.as_slice())
            .expect("append safe submitted batches");
    }

    #[tokio::test]
    async fn tick_once_submits_first_missing_closed_batch() {
        let (_dir, path) = temp_db("tick-submits");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        let config = BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
            max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
            preemptive_margin_blocks: 75,
            seconds_per_block: 12,
        };
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            BATCH_SUBMITTER_ADDRESS,
            mock.clone(),
            ShutdownSignal::default(),
            config,
        );

        let outcome = submitter.tick_once().await.expect("tick once");
        // seed_two_closed_batches creates 3 closed batches (0, 1, 2) + open batch 3.
        assert_eq!(outcome, TickOutcome::Submitted { count: 3 });

        let submissions = mock.submissions();
        assert_eq!(submissions.len(), 3);
        assert_eq!(submissions[0].0, 0);
        assert_eq!(submissions[1].0, 1);
        assert_eq!(submissions[2].0, 2);
    }

    #[tokio::test]
    async fn tick_once_submits_nothing_when_already_caught_up() {
        let (_dir, path) = temp_db("tick-caught-up");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0, 1]);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_observed_submitted_nonces(vec![2]);
        let config = BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
            max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
            preemptive_margin_blocks: 75,
            seconds_per_block: 12,
        };
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            BATCH_SUBMITTER_ADDRESS,
            mock.clone(),
            ShutdownSignal::default(),
            config,
        );

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Idle);
        assert!(mock.submissions().is_empty());
        assert_eq!(mock.last_from_block(), Some(11));
    }

    #[tokio::test]
    async fn tick_once_skips_already_submitted() {
        let (_dir, path) = temp_db("tick-combines-prefix-and-suffix");
        seed_two_closed_batches(&path);
        // Seed safe_inputs for all 3 closed batches (nonces 0, 1, 2).
        seed_safe_submitted_batches(&path, 10, &[0, 1, 2]);

        let mock = Arc::new(MockBatchPoster::new());
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            BATCH_SUBMITTER_ADDRESS,
            mock.clone(),
            ShutdownSignal::default(),
            BatchSubmitterConfig {
                idle_poll_interval_ms: 1000,
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                seconds_per_block: 12,
            },
        );

        let outcome = submitter.tick_once().await.expect("tick once");
        // All 3 closed batches already submitted (nonces 0, 1, 2 in safe_inputs).
        assert_eq!(outcome, TickOutcome::Idle);
    }

    #[tokio::test]
    async fn tick_once_submits_only_missing_suffix_from_safe_frontier() {
        let (_dir, path) = temp_db("tick-safe-frontier-suffix");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0, 1]);

        let mock = Arc::new(MockBatchPoster::new());
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            BATCH_SUBMITTER_ADDRESS,
            mock.clone(),
            ShutdownSignal::default(),
            BatchSubmitterConfig {
                idle_poll_interval_ms: 1000,
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                seconds_per_block: 12,
            },
        );

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Submitted { count: 1 });
        assert_eq!(mock.last_from_block(), Some(11));

        let submissions = mock.submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].0, 2);
    }

    #[tokio::test]
    async fn tick_once_replaces_from_latest_mined_prefix_not_safe_prefix() {
        let (_dir, path) = temp_db("tick-latest-mined-prefix");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0]);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_observed_submitted_nonces(vec![1]);
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            BATCH_SUBMITTER_ADDRESS,
            mock.clone(),
            ShutdownSignal::default(),
            BatchSubmitterConfig {
                idle_poll_interval_ms: 1000,
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                seconds_per_block: 12,
            },
        );

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Submitted { count: 1 });
        assert_eq!(mock.last_from_block(), Some(11));

        let submissions = mock.submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].0, 2);
    }

    #[tokio::test]
    async fn tick_once_propagates_poster_errors() {
        let (_dir, path) = temp_db("tick-poster-error");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_observed_submitted_error(Some("rpc fail"));
        let submitter = super::BatchSubmitter::new(
            path,
            BATCH_SUBMITTER_ADDRESS,
            mock,
            ShutdownSignal::default(),
            BatchSubmitterConfig {
                idle_poll_interval_ms: 1000,
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                seconds_per_block: 12,
            },
        );

        let err = submitter
            .tick_once()
            .await
            .expect_err("poster error should propagate");
        assert!(matches!(err, BatchSubmitterError::Poster(_)));
    }

    #[tokio::test]
    async fn check_danger_zone_detects_reused_nonce_after_recovery() {
        let (_dir, path) = temp_db("tick-stale-reused-nonce");
        let batch_submitter = BATCH_SUBMITTER_ADDRESS;

        let mut storage = Storage::open(&path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage.assign_batch_nonces().expect("assign nonces gen1");

        let gen1_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: gen1_payload,
                    block_number: 1210,
                }],
            )
            .expect("append gen1 stale submission");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate gen1 frontier");
        let invalidated = storage.detect_and_recover(1200).expect("recover gen1");
        assert_eq!(invalidated, vec![0, 1]);

        let mut head = storage
            .load_open_state()
            .expect("load open state")
            .expect("recovery batch");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close gen2 batch");
        storage.assign_batch_nonces().expect("assign nonces gen2");

        let gen2_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 100,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                2410,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: gen2_payload,
                    block_number: 2410,
                }],
            )
            .expect("append gen2 stale submission");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate gen2 frontier");
        drop(storage);

        let submitter = super::BatchSubmitter::new(
            path,
            batch_submitter,
            Arc::new(MockBatchPoster::new()),
            ShutdownSignal::default(),
            BatchSubmitterConfig {
                idle_poll_interval_ms: 1000,
                max_wait_blocks: 1200,
                preemptive_margin_blocks: 75,
                seconds_per_block: 12,
            },
        );

        let err = submitter
            .check_danger_zone()
            .await
            .expect_err("reused frontier nonce should still be detected as in danger zone");
        assert!(matches!(err, BatchSubmitterError::DangerZone { .. }));
    }

    #[test]
    fn advance_expected_batch_nonce_matches_scheduler_nonce_rule() {
        assert_eq!(super::advance_expected_batch_nonce(0, Vec::<u64>::new()), 0);
        assert_eq!(super::advance_expected_batch_nonce(0, vec![0, 1, 2]), 3);
        assert_eq!(super::advance_expected_batch_nonce(0, vec![0, 2, 3]), 1);
        assert_eq!(super::advance_expected_batch_nonce(0, vec![1, 2, 3]), 0);
        assert_eq!(super::advance_expected_batch_nonce(0, vec![0, 1, 1, 2]), 3);
        assert_eq!(
            super::advance_expected_batch_nonce(0, vec![6, 4, 3, 2, 2, 0, 1]),
            2
        );
        assert_eq!(super::advance_expected_batch_nonce(0, vec![0, 2, 1]), 2);
        assert_eq!(super::advance_expected_batch_nonce(2, vec![2, 3]), 4);
    }
}
