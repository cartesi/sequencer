// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter worker: stateless, at-least-once submission to L1.
//!
//! The worker alternates between running one tick of work and sleeping for
//! `idle_poll_interval`, until either shutdown fires or a fatal error
//! propagates. A tick:
//!
//! 1. Reads a lightweight snapshot ([`TickSnapshot`]) — safe block, next
//!    expected batch nonce, and a folded danger-zone check (strict
//!    block-based + wall-clock adjusted). The scheduler-accepted frontier is
//!    maintained by the input reader via `append_safe_inputs`; the worker is
//!    a pure reader.
//! 2. Crashes with `DangerZone` if the snapshot flags any batch past the
//!    (possibly adjusted) threshold — startup recovery will then flush and
//!    cascade.
//! 3. Queries L1 for batch submissions past the accepted frontier, advances
//!    the expected nonce over any contiguous matches, and submits the remaining
//!    suffix. Provider errors propagate and the outer loop logs + retries.
//!
//! Intentional simplifications:
//! - The worker sleeps for one `idle_poll_interval` after every non-fatal
//!   tick outcome, including a successful submission attempt. This keeps the
//!   loop single-cadence rather than special-casing "productive" ticks.
//! - Danger detection and frontier reads are eventually consistent rather than
//!   transactionally atomic. A danger transition may lag by up to one worker
//!   tick, which the preemptive margin is expected to absorb.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, error};

use crate::l1::submitter::{BatchPoster, BatchPosterError, BatchSubmitterConfig};
use crate::recovery::RecoveryParams;
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{DangerStatus, PendingBatch, Storage, StorageOpenError};

/// In-memory snapshot the worker builds from two storage reads each tick.
#[derive(Debug, Clone, Copy)]
struct TickSnapshot {
    safe_block: u64,
    safe_next_expected_nonce: u64,
    danger: DangerStatus,
}

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
        "danger zone: batch {batch_index} approaching staleness — sequencer must stop for recovery"
    )]
    DangerZone { batch_index: u64 },
}

pub struct BatchSubmitter<P: BatchPoster> {
    db_path: String,
    poster: Arc<P>,
    idle_poll_interval: Duration,
    recovery_params: RecoveryParams,
    shutdown: ShutdownSignal,
}

impl<P: BatchPoster + 'static> BatchSubmitter<P> {
    pub fn new(
        db_path: impl Into<String>,
        poster: Arc<P>,
        shutdown: ShutdownSignal,
        config: BatchSubmitterConfig,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poster,
            idle_poll_interval: config.idle_poll_interval(),
            recovery_params: RecoveryParams {
                max_wait_blocks: config.max_wait_blocks,
                danger_threshold: config.danger_threshold(),
                seconds_per_block: config.seconds_per_block,
            },
            shutdown,
        }
    }

    pub fn start(
        self,
    ) -> Result<tokio::task::JoinHandle<Result<(), BatchSubmitterError>>, StorageOpenError> {
        let _ = Storage::open_read_only(self.db_path.as_str())?;
        Ok(tokio::spawn(async move { self.run_forever().await }))
    }

    /// Top-level driver: race the work loop against the shutdown signal.
    ///
    /// Any mid-tick await (DB read, RPC call, confirmation watch, sleep) is
    /// cancellable at a shutdown. Mid-tick cancellation is crash-safe:
    /// storage operations either commit or auto-roll-back on drop, and any
    /// already-sent L1 transaction will be picked up by the next startup's
    /// `observed_submitted_batch_nonces` scan.
    async fn run_forever(self) -> Result<(), BatchSubmitterError> {
        tokio::select! {
            biased;
            _ = self.shutdown.wait_for_shutdown() => Ok(()),
            result = self.run_loop() => result,
        }
    }

    /// Infinite work loop: tick, sleep, repeat. Only fatal errors propagate;
    /// provider errors are logged and the next tick retries.
    ///
    /// The cadence is intentionally uniform: even after a successful submit,
    /// the worker waits `idle_poll_interval` before re-entering. That trades a
    /// small amount of responsiveness for a simpler, one-state loop.
    async fn run_loop(&self) -> Result<(), BatchSubmitterError> {
        loop {
            if let Err(err) = self.tick_once().await {
                match err {
                    BatchSubmitterError::Poster(source) => {
                        error!(error = %source, "L1 provider error — will retry");
                    }
                    fatal => return Err(fatal),
                }
            }
            tokio::time::sleep(self.idle_poll_interval).await;
        }
    }

    pub(crate) async fn tick_once(&self) -> Result<(), BatchSubmitterError> {
        let snapshot = self.load_tick_snapshot().await?;

        // Either kind of danger exits for recovery. The submitter doesn't
        // distinguish Strict vs Stalled — both imply "stop and let startup
        // decide what to do next."
        if let Some(batch_index) = snapshot.danger.batch_index() {
            tracing::error!(
                batch_index,
                status = ?snapshot.danger,
                danger_threshold = self.recovery_params.danger_threshold,
                "danger zone detected — triggering shutdown for flush and recovery"
            );
            return Err(BatchSubmitterError::DangerZone { batch_index });
        }

        // Derive the next unresolved batch nonce from the safe frontier plus
        // latest-chain mined submissions beyond that safe prefix.
        //
        // This must start at `safe_block + 1`: after a danger-zone shutdown, the
        // flusher only returns once `Pending <= Safe`, so any wallet-nonce slots
        // backed by blocks at or below the safe head are already resolved and
        // folded into `safe_next_expected_nonce`. Re-scanning those blocks here
        // would double-count the finalized prefix and can skew post-recovery
        // resubmission.
        let next_nonce = {
            let recent_observed_nonces = self
                .poster
                .observed_submitted_batch_nonces(snapshot.safe_block.saturating_add(1))
                .await?;
            advance_expected_batch_nonce(snapshot.safe_next_expected_nonce, recent_observed_nonces)
        };

        let pending = self.load_pending_batches(next_nonce).await?;
        if pending.is_empty() {
            return Ok(());
        }

        // Submit the whole suffix in one shot, then let the poster wait for
        // confirmations serially. Using latest mined submissions plus the
        // latest L1 account nonce makes the next tick naturally replace
        // unresolved txs at the same wallet nonces after a timeout.
        for batch in &pending {
            debug!(
                batch_index = batch.batch_index,
                nonce = batch.nonce,
                "queueing batch for L1 submission"
            );
        }
        let submitted_count = pending.len();
        let payloads: Vec<Vec<u8>> = pending.into_iter().map(|b| b.encoded).collect();
        let tx_hashes = self.poster.submit_batches(payloads).await?;
        if tx_hashes.len() != submitted_count {
            return Err(BatchSubmitterError::Poster(BatchPosterError::Provider(
                format!(
                    "poster returned {} tx hashes for {submitted_count} submitted batches",
                    tx_hashes.len(),
                ),
            )));
        }

        Ok(())
    }

    /// Two storage reads in one `spawn_blocking` — not an SQL transaction but
    /// a single blocking task.
    ///
    /// This is intentionally eventual-consistent: the danger decision and the
    /// frontier view may come from slightly different DB moments if the input
    /// reader advances between reads. The design tolerates that bounded lag in
    /// exchange for keeping danger detection and submitter frontier logic
    /// decoupled.
    async fn load_tick_snapshot(&self) -> Result<TickSnapshot, BatchSubmitterError> {
        let db_path = self.db_path.clone();
        let params = self.recovery_params;
        let now_ms = crate::recovery::unix_now_ms();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            let danger = storage.check_danger(params, now_ms)?;
            let (safe_block, safe_next_expected_nonce) = storage.submitter_frontier_view()?;
            Ok::<_, BatchSubmitterError>(TickSnapshot {
                safe_block,
                safe_next_expected_nonce,
                danger,
            })
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

/// Advance `expected` by greedily consuming any matching observed nonce.
///
/// `observed_nonces` is the stream of **batch nonces** (from the SSZ payload)
/// decoded from `InputAdded` events sent by our batch-submitter EOA, in L1
/// event order. Because L1 mines txs from a single EOA in strict wallet-nonce
/// order, this stream is naturally gap-less at the wallet-nonce level:
/// tx[k]'s event cannot appear on-chain without tx[k-1]'s event, and the
/// observed batch nonce sequence therefore mirrors our submission order.
///
/// Batch nonces themselves (unlike wallet nonces) CAN repeat across recovery
/// generations — e.g., after a cascade, a fresh batch reuses its invalidated
/// predecessor's nonce. That's why we still match on equality rather than
/// trusting a sort: in a post-recovery window, the same batch nonce can be
/// observed twice (once from the invalidated generation, once from the new
/// one), and we only want to advance once.
///
/// Under the wallet-nonce ordering above, once the next `expected` doesn't
/// appear in the stream the frontier naturally stops advancing — the gap
/// means the scheduler hasn't seen that nonce on-chain yet (or observed it at
/// a different wallet nonce from an earlier generation).
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
        BatchSubmitterConfig, BatchSubmitterError, poster::mock::MockBatchPoster,
    };
    use crate::runtime::shutdown::ShutdownSignal;
    use crate::storage::test_helpers::{TestDb, temp_db};
    use crate::storage::{SafeInputRange, SchedulerRules, Storage, StoredSafeInput};

    const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";
    const BATCH_SUBMITTER_ADDRESS: Address = Address::repeat_byte(0x11);

    /// Rules pinned to `BATCH_SUBMITTER_ADDRESS` — worker tests use that as
    /// their test submitter, so populate sees the seeded safe_inputs.
    fn submitter_test_rules() -> SchedulerRules {
        SchedulerRules::new(BATCH_SUBMITTER_ADDRESS, sequencer_core::MAX_WAIT_BLOCKS)
    }

    fn default_test_config() -> BatchSubmitterConfig {
        BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
            max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
            preemptive_margin_blocks: 75,
            seconds_per_block: 12,
        }
    }

    fn set_last_safe_progress_ms(db_path: &str, synced_at_ms: u64) {
        let conn = Storage::open_connection(db_path, SQLITE_SYNCHRONOUS_PRAGMA)
            .expect("open raw sqlite connection");
        conn.execute(
            "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
            [i64::try_from(synced_at_ms).unwrap_or(i64::MAX)],
        )
        .expect("update sync timestamp");
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
        // Rules must use the same sender these inputs are attributed to, otherwise
        // populate_safe_accepted_batches (run inside append_safe_inputs) filters
        // them out and the test's frontier stays empty.
        storage
            .append_safe_inputs(safe_block, inputs.as_slice(), &submitter_test_rules())
            .expect("append safe submitted batches");
    }

    #[tokio::test]
    async fn tick_once_submits_first_missing_closed_batch() {
        let TestDb { _dir, path } = temp_db("tick-submits");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            default_test_config(),
        );

        submitter.tick_once().await.expect("tick once");

        // seed_two_closed_batches creates 3 closed batches (0, 1, 2) + open batch 3.
        let submissions = mock.submissions();
        assert_eq!(submissions.len(), 3);
        assert_eq!(submissions[0].0, 0);
        assert_eq!(submissions[1].0, 1);
        assert_eq!(submissions[2].0, 2);
    }

    #[tokio::test]
    async fn tick_once_submits_nothing_when_already_caught_up() {
        let TestDb { _dir, path } = temp_db("tick-caught-up");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0, 1]);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_observed_submitted_nonces(vec![2]);
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            default_test_config(),
        );

        submitter.tick_once().await.expect("tick once");
        assert!(mock.submissions().is_empty());
        assert_eq!(mock.last_from_block(), Some(11));
    }

    #[tokio::test]
    async fn tick_once_skips_already_submitted() {
        let TestDb { _dir, path } = temp_db("tick-combines-prefix-and-suffix");
        seed_two_closed_batches(&path);
        // Seed safe_inputs for all 3 closed batches (nonces 0, 1, 2).
        seed_safe_submitted_batches(&path, 10, &[0, 1, 2]);

        let mock = Arc::new(MockBatchPoster::new());
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            default_test_config(),
        );

        submitter.tick_once().await.expect("tick once");
        // All 3 closed batches already submitted (nonces 0, 1, 2 in safe_inputs).
        assert!(mock.submissions().is_empty());
    }

    #[tokio::test]
    async fn tick_once_submits_only_missing_suffix_from_safe_frontier() {
        let TestDb { _dir, path } = temp_db("tick-safe-frontier-suffix");
        seed_two_closed_batches(&path);
        seed_safe_submitted_batches(&path, 10, &[0, 1]);

        let mock = Arc::new(MockBatchPoster::new());
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            default_test_config(),
        );

        submitter.tick_once().await.expect("tick once");
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
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            default_test_config(),
        );

        submitter.tick_once().await.expect("tick once");
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
        let submitter = super::BatchSubmitter::new(
            path,
            mock,
            ShutdownSignal::default(),
            default_test_config(),
        );

        let err = submitter
            .tick_once()
            .await
            .expect_err("poster error should propagate");
        assert!(matches!(err, BatchSubmitterError::Poster(_)));
    }

    #[tokio::test]
    async fn tick_once_detects_stalled_safe_head_from_snapshot() {
        let TestDb { _dir, path } = temp_db("tick-stalled-safe-head");
        let mut storage = Storage::open(&path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");
        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: BATCH_SUBMITTER_ADDRESS,
                    payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                        nonce: 0,
                        frames: vec![sequencer_core::batch::Frame {
                            safe_block: 100,
                            fee_price: 0,
                            user_ops: vec![],
                        }],
                    }),
                    block_number: 200,
                }],
                &submitter_test_rules(),
            )
            .expect("append accepted batch 0");
        drop(storage);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        set_last_safe_progress_ms(&path, now_ms.saturating_sub(25 * 12 * 1000));

        let mock = Arc::new(MockBatchPoster::new());
        let submitter = super::BatchSubmitter::new(
            path,
            mock,
            ShutdownSignal::default(),
            default_test_config(),
        );

        let err = submitter
            .tick_once()
            .await
            .expect_err("stalled safe head should trip the danger-zone estimate");
        assert!(matches!(
            err,
            BatchSubmitterError::DangerZone { batch_index: 1 }
        ));
    }

    #[tokio::test]
    async fn snapshot_reports_reused_nonce_as_danger_after_recovery() {
        let TestDb { _dir, path } = temp_db("tick-stale-reused-nonce");
        let batch_submitter = BATCH_SUBMITTER_ADDRESS;

        let mut storage = Storage::open(&path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

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
                &submitter_test_rules(),
            )
            .expect("append gen1 stale submission");
        let invalidated = storage.detect_and_recover(1200).expect("recover gen1");
        assert_eq!(invalidated, vec![0, 1]);

        let mut head = storage
            .load_open_state()
            .expect("load open state")
            .expect("recovery batch");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close gen2 batch");

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
                &submitter_test_rules(),
            )
            .expect("append gen2 stale submission");
        drop(storage);

        let submitter = super::BatchSubmitter::new(
            path,
            Arc::new(MockBatchPoster::new()),
            ShutdownSignal::default(),
            BatchSubmitterConfig {
                idle_poll_interval_ms: 1000,
                max_wait_blocks: 1200,
                preemptive_margin_blocks: 75,
                seconds_per_block: 12,
            },
        );

        let snapshot = submitter
            .load_tick_snapshot()
            .await
            .expect("load coherent submitter snapshot");
        assert!(
            snapshot.danger.is_dangerous(),
            "reused frontier nonce should still be detected as in danger zone"
        );
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
