// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter worker: stateless, at-least-once submission to L1.
//!
//! The submitter never observes danger — that is the [`crate::recovery::DangerDetector`]
//! worker's job. Each tick here is a pure "what pending work is left?" step:
//!
//! 1. Read the scheduler-accepted frontier (safe block + next-expected nonce)
//!    from SQLite. Shared snapshot maintained by the input reader via
//!    `append_safe_inputs`.
//! 2. Query L1 for batch submissions newer than the safe block; fold any
//!    matching observed nonces to advance the local expected nonce past
//!    already-mined submissions.
//! 3. Load every valid closed batch whose nonce is still past the advanced
//!    frontier and submit them all in one shot.
//!
//! The outer loop is uniform: tick, maybe sleep, repeat. A tick that produced
//! submissions re-enters immediately (no sleep) so the suffix drains quickly;
//! an idle or transient-error tick sleeps `idle_poll_interval`, while a
//! fee-ceiling hold sleeps the confirmation cadence before the next probe.
//!
//! Mid-tick cancellation is crash-safe: storage transactions either commit or
//! auto-roll-back on drop, and any already-sent L1 transaction is picked up by
//! the next startup's `observed_submitted_batch_nonces` scan.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, error};

use crate::l1::submitter::{
    BatchPoster, BatchPosterError, BatchSubmitterConfig, SubmitBatchesOutcome,
};
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{PendingBatch, Storage, StorageOpenError, SubmitterFrontier};

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
}

/// How the submitter loop exited.
///
/// There is only one deliberate exit path (shutdown). Danger detection lives
/// in the [`crate::recovery::DangerDetector`] worker; this type does not
/// concern itself with that signal.
#[derive(Debug)]
pub enum SubmitterExit {
    /// Shutdown signal fired.
    Shutdown,
}

/// Outcome of one tick. Drives the outer loop's sleep cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickOutcome {
    /// Nothing pending; sleep before the next tick.
    Idle,
    /// Submitted one or more batches; re-enter immediately so the suffix
    /// drains without idle-sleep.
    Submitted(usize),
    /// Fee ceiling prevented a valid replacement floor. This is not an
    /// internal retry loop; the outer loop waits on confirmation cadence.
    Held,
    /// Transient provider error; log and sleep before retrying.
    Transient,
}

/// Pure: given the current submitter frontier and the batch nonces we just
/// observed on L1 past that frontier, compute the nonce at which we should
/// start submitting the remaining suffix. When the observed list is empty
/// (nothing new on L1) the result is just `frontier.accepted_next_nonce`.
fn decide_submit_start(frontier: SubmitterFrontier, recently_observed_nonces: &[u64]) -> u64 {
    // Fold observed nonces over the safe-accepted frontier to derive the next
    // unresolved nonce. The scan starts at `safe_block + 1` (the submitter
    // asks the poster for that), so wallet-nonce ordering guarantees the
    // observed list mirrors our submission order.
    sequencer_core::protocol::advance_expected_batch_nonce(
        frontier.accepted_next_nonce,
        recently_observed_nonces.iter().copied(),
    )
}

pub struct BatchSubmitter<P: BatchPoster> {
    db_path: String,
    poster: Arc<P>,
    idle_poll_interval: Duration,
    confirmation_cadence: Duration,
    /// Write-before-broadcast hook (review R1a): the poster raises the
    /// persisted wallet-nonce watermark through this before every send.
    watermark_sink: crate::l1::watermark::StorageWatermarkSink,
}

impl<P: BatchPoster + 'static> BatchSubmitter<P> {
    pub fn new(db_path: impl Into<String>, poster: Arc<P>, config: BatchSubmitterConfig) -> Self {
        let db_path = db_path.into();
        Self {
            watermark_sink: crate::l1::watermark::StorageWatermarkSink::new(db_path.clone()),
            db_path,
            poster,
            idle_poll_interval: config.idle_poll_interval(),
            confirmation_cadence: config.confirmation_cadence(),
        }
    }

    /// Spawn the worker loop. The `shutdown` signal is what the loop respects;
    /// passing it at start time (instead of construction time) keeps the
    /// construction phase pure.
    pub fn start(
        self,
        shutdown: ShutdownSignal,
    ) -> Result<tokio::task::JoinHandle<Result<SubmitterExit, BatchSubmitterError>>, StorageOpenError>
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
    ) -> Result<SubmitterExit, BatchSubmitterError> {
        tokio::select! {
            biased;
            _ = shutdown.wait_for_shutdown() => Ok(SubmitterExit::Shutdown),
            result = self.run_loop() => result,
        }
    }

    /// Tick → sleep-if-idle → tick. Productive ticks re-enter immediately;
    /// idle or transient-error ticks wait `idle_poll_interval`; held ticks
    /// wait the confirmation cadence. Fatal errors propagate.
    async fn run_loop(&self) -> Result<SubmitterExit, BatchSubmitterError> {
        loop {
            let outcome = match self.tick_once().await {
                Ok(o) => o,
                // A wrong-chain RPC is terminal — never retry-loop signing onto
                // it. Lift it out of the transient `Poster` bucket below.
                Err(e @ BatchSubmitterError::Poster(BatchPosterError::ChainIdMismatch { .. })) => {
                    error!(error = %e, "RPC serves the wrong chain — refusing to submit");
                    return Err(e);
                }
                Err(BatchSubmitterError::Poster(source)) => {
                    error!(error = %source, "L1 provider error — will retry");
                    TickOutcome::Transient
                }
                Err(fatal) => return Err(fatal),
            };
            match outcome {
                TickOutcome::Submitted(_) => continue,
                TickOutcome::Held => {
                    tokio::time::sleep(self.confirmation_cadence).await;
                }
                TickOutcome::Idle | TickOutcome::Transient => {
                    tokio::time::sleep(self.idle_poll_interval).await;
                }
            }
        }
    }

    pub(crate) async fn tick_once(&self) -> Result<TickOutcome, BatchSubmitterError> {
        let frontier = self.load_frontier().await?;

        // Must start scanning at `safe_block + 1`: after a danger-zone shutdown
        // the flusher only returns once `Pending <= Safe`, so any wallet-nonce
        // slots backed by blocks at or below the safe head are already
        // resolved and folded into `accepted_next_nonce`. Re-scanning those
        // blocks here would double-count the finalized prefix.
        let recent_observed = self
            .poster
            .observed_submitted_batch_nonces(frontier.safe_block.saturating_add(1))
            .await?;

        let from_nonce = decide_submit_start(frontier, &recent_observed);
        let pending = self.pending_batches(from_nonce).await?;
        if pending.is_empty() {
            return Ok(TickOutcome::Idle);
        }

        for batch in &pending {
            debug!(
                batch_index = batch.batch_index,
                nonce = batch.nonce,
                "queueing batch for L1 submission"
            );
        }
        let submitted_count = pending.len();
        let payloads: Vec<Vec<u8>> = pending.into_iter().map(|b| b.encoded).collect();
        let outcome = self
            .poster
            .submit_batches(payloads, &self.watermark_sink)
            .await?;
        match outcome {
            SubmitBatchesOutcome::Submitted(tx_hashes) => {
                if tx_hashes.len() != submitted_count {
                    return Err(BatchSubmitterError::Poster(BatchPosterError::Provider(
                        format!(
                            "poster returned {} tx hashes for {submitted_count} submitted batches",
                            tx_hashes.len(),
                        ),
                    )));
                }
                Ok(TickOutcome::Submitted(submitted_count))
            }
            SubmitBatchesOutcome::Held(tx_hashes) => {
                if tx_hashes.len() > submitted_count {
                    return Err(BatchSubmitterError::Poster(BatchPosterError::Provider(
                        format!(
                            "held poster returned {} tx hashes for {submitted_count} submitted batches",
                            tx_hashes.len(),
                        ),
                    )));
                }
                Ok(TickOutcome::Held)
            }
        }
    }

    async fn load_frontier(&self) -> Result<SubmitterFrontier, BatchSubmitterError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            storage
                .submitter_frontier()
                .map_err(BatchSubmitterError::from)
        })
        .await
        .map_err(|err| BatchSubmitterError::Join(err.to_string()))?
    }

    async fn pending_batches(
        &self,
        min_nonce: u64,
    ) -> Result<Vec<PendingBatch>, BatchSubmitterError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            storage
                .pending_batches(min_nonce)
                .map_err(BatchSubmitterError::from)
        })
        .await
        .map_err(|err| BatchSubmitterError::Join(err.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::Address;

    use super::{TickOutcome, decide_submit_start};
    use crate::l1::submitter::{BatchSubmitterConfig, poster::mock::MockBatchPoster};
    use crate::storage::test_helpers::{TestDb, temp_db};
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput, SubmitterFrontier};
    use sequencer_core::protocol::ProtocolTiming;

    const BATCH_SUBMITTER_ADDRESS: Address = Address::repeat_byte(0x11);

    /// Protocol pinned to `BATCH_SUBMITTER_ADDRESS` — worker tests use that as
    /// their test submitter, so populate sees the seeded safe_inputs.
    fn submitter_test_protocol() -> ProtocolTiming {
        ProtocolTiming {
            max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
            preemptive_margin_blocks: 75,
            l1_read_stale_after_blocks: 900,
            seconds_per_block: 12,
        }
    }

    fn default_test_config() -> BatchSubmitterConfig {
        BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
            confirmation_depth: 0,
            seconds_per_block: 1,
        }
    }

    fn seed_two_closed_batches(db_path: &str) {
        let mut storage = Storage::open(db_path).expect("open storage");
        storage
            .append_safe_inputs(0, &[], BATCH_SUBMITTER_ADDRESS, &submitter_test_protocol())
            .expect("record observed safe head");
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
        let mut storage = Storage::open(db_path).expect("open storage");
        // Landings carry the local batch's real wire bytes so the
        // content-identity check (review R2) accepts them.
        let inputs: Vec<_> = nonces
            .iter()
            .map(|nonce| StoredSafeInput {
                sender: BATCH_SUBMITTER_ADDRESS,
                payload: crate::storage::test_helpers::local_batch_payload(&mut storage, *nonce),
                block_number: safe_block,
            })
            .collect();
        storage
            .append_safe_inputs(
                safe_block,
                inputs.as_slice(),
                BATCH_SUBMITTER_ADDRESS,
                &submitter_test_protocol(),
            )
            .expect("append safe submitted batches");
    }

    #[tokio::test]
    async fn tick_once_submits_first_missing_closed_batch() {
        let TestDb { _dir, path } = temp_db("tick-submits");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        let submitter =
            super::BatchSubmitter::new(path.clone(), mock.clone(), default_test_config());

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Submitted(3));

        let submissions = mock.submissions();
        assert_eq!(submissions.len(), 3);
        assert_eq!(submissions[0].0, 0);
        assert_eq!(submissions[1].0, 1);
        assert_eq!(submissions[2].0, 2);
    }

    #[tokio::test]
    async fn tick_once_surfaces_fee_ceiling_hold() {
        let TestDb { _dir, path } = temp_db("tick-held");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_held(true);
        let submitter =
            super::BatchSubmitter::new(path.clone(), mock.clone(), default_test_config());

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Held);
        assert_eq!(mock.submissions().len(), 3);
    }

    #[tokio::test]
    async fn tick_once_submits_nothing_when_already_caught_up() {
        let TestDb { _dir, path } = temp_db("tick-caught-up");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0, 1]);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_observed_submitted_nonces(vec![2]);
        let submitter =
            super::BatchSubmitter::new(path.clone(), mock.clone(), default_test_config());

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Idle);
        assert!(mock.submissions().is_empty());
        assert_eq!(mock.last_from_block(), Some(11));
    }

    #[tokio::test]
    async fn tick_once_skips_already_submitted() {
        let TestDb { _dir, path } = temp_db("tick-combines-prefix-and-suffix");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0, 1, 2]);

        let mock = Arc::new(MockBatchPoster::new());
        let submitter =
            super::BatchSubmitter::new(path.clone(), mock.clone(), default_test_config());

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Idle);
        assert!(mock.submissions().is_empty());
    }

    #[tokio::test]
    async fn tick_once_submits_only_missing_suffix_from_safe_frontier() {
        let TestDb { _dir, path } = temp_db("tick-safe-frontier-suffix");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0, 1]);

        let mock = Arc::new(MockBatchPoster::new());
        let submitter =
            super::BatchSubmitter::new(path.clone(), mock.clone(), default_test_config());

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Submitted(1));
        assert_eq!(mock.last_from_block(), Some(11));

        let submissions = mock.submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].0, 2);
    }

    #[tokio::test]
    async fn tick_once_replaces_from_latest_mined_prefix_not_safe_prefix() {
        let TestDb { _dir, path } = temp_db("tick-latest-mined-prefix");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0]);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_observed_submitted_nonces(vec![1]);
        let submitter =
            super::BatchSubmitter::new(path.clone(), mock.clone(), default_test_config());

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Submitted(1));
        assert_eq!(mock.last_from_block(), Some(11));

        let submissions = mock.submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].0, 2);
    }

    #[tokio::test]
    async fn tick_once_propagates_poster_errors() {
        let TestDb { _dir, path } = temp_db("tick-poster-error");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_observed_submitted_error(Some("rpc fail"));
        let submitter = super::BatchSubmitter::new(path, mock, default_test_config());

        let err = submitter
            .tick_once()
            .await
            .expect_err("poster error should propagate");
        assert!(matches!(err, super::BatchSubmitterError::Poster(_)));
    }

    // ── decide_submit_start (pure) ────────────────────────────────────────

    #[test]
    fn decide_submit_start_advances_past_observed_prefix() {
        let from_nonce = decide_submit_start(
            SubmitterFrontier {
                safe_block: 10,
                accepted_next_nonce: 0,
            },
            &[0, 1, 2],
        );
        assert_eq!(from_nonce, 3);
    }

    #[test]
    fn decide_submit_start_stops_at_first_gap() {
        let from_nonce = decide_submit_start(
            SubmitterFrontier {
                safe_block: 10,
                accepted_next_nonce: 0,
            },
            &[0, 2, 3],
        );
        assert_eq!(from_nonce, 1);
    }

    #[test]
    fn decide_submit_start_handles_empty_observed_list() {
        let from_nonce = decide_submit_start(
            SubmitterFrontier {
                safe_block: 10,
                accepted_next_nonce: 5,
            },
            &[],
        );
        assert_eq!(from_nonce, 5);
    }

    #[test]
    fn decide_submit_start_advances_once_per_matching_nonce_across_recovery_generations() {
        // Post-recovery scenario the `advance_expected_batch_nonce` doc calls
        // out: batch nonces can repeat across recovery generations because a
        // cascade re-uses the last valid ancestor's `nonce + 1`. The observed
        // event stream can therefore contain the same batch nonce twice (once
        // from the invalidated generation, once from the recovery generation).
        //
        // decide_submit_start must advance exactly ONCE per matching nonce —
        // the second occurrence at a nonce that no longer equals `expected` is
        // a no-op, as intended. The underlying fold is table-tested below; this
        // pins the wrapper at the nonce-reuse case explicitly.
        let from_nonce = decide_submit_start(
            SubmitterFrontier {
                safe_block: 10,
                accepted_next_nonce: 2,
            },
            // Two events reporting nonce=2 (one per generation), then nonce=3.
            &[2, 2, 3],
        );
        // 2 matches expected=2 → advance to 3. Second 2 doesn't match
        // expected=3, skip. 3 matches → advance to 4.
        assert_eq!(from_nonce, 4);
    }
}
