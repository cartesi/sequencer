// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Preemptive recovery: detect danger zone, then recover via Tip invalidation
//! or post-flush cascade.
//!
//! At runtime a dedicated [`DangerDetector`] worker polls `Storage::check_danger` each
//! tick. If the L1 view is stale, a closed batch or Tip crosses
//! `danger_threshold`, or the batch-relative wall-clock estimate fires during
//! an L1 outage, the detector exits with `DetectorExit::RecoveryRequired`, the
//! runtime maps that to `DangerDetectorExit::DangerDetected` under
//! `RunError::Worker`, and the process exits. The detector tripping is *only*
//! a trigger to enter startup recovery/refusal — it doesn't make the cascade
//! decision. External orchestration restarts the sequencer, and this startup
//! path runs.
//!
//! Startup recovery branches on [`decide_startup_action`]:
//!
//! - `FlushAndCascade`: a closed batch past gold is dangerous. Flush the mempool,
//!   re-sync the safe head, then call [`crate::storage::Storage::recover_post_flush`] which cascades
//!   everything past the gold frontier (every non-gold batch is doomed:
//!   Silver-stale, Silver-poisoned, or Pending no-op'd). If all closed are gold,
//!   falls through to a Tip danger-zone check — see `docs/recovery/README.md` Step 5.
//! - `RecoverTip`: only the open Tip is dangerous. It has no L1 footprint, so call
//!   [`crate::storage::Storage::recover_aging_tip`] directly without flushing.
//! - `Proceed`: no danger detected. No DB writes here; the genesis Tip (on a
//!   fresh DB) is opened by the structural [`crate::storage::Storage::ensure_open_tip`]
//!   step in `Workers::spawn`, after recovery and before the lane starts.
//! - `Refuse`: L1 view is stale or batch-relative estimated danger fired; bail
//!   out and surface to the operator.
//!
//! ## Fault model
//!
//! Recovery is designed to handle **submission and outage failures**: the sequencer
//! crashes, the L1 provider becomes unreachable, transactions are dropped from the
//! mempool, or the process is offline for an extended period. It is **not** designed
//! to handle arbitrarily malformed self-submissions. The scheduler frontier
//! reconstruction (`populate_safe_accepted_batches`) trusts that on-chain batches
//! from the sequencer's own address are structurally valid. This is a deliberate
//! system assumption, not a gap — the sequencer controls its own submissions.
//!
//! See `docs/recovery/` for the full design, TLA+ specs, and design history.

mod detector;
mod flusher;

use thiserror::Error;

use crate::l1::reader::{InputReader, InputReaderError};
use crate::runtime::config::L1Config;
use crate::storage::{self, DangerStatus, StorageOpenError};
pub use detector::{DangerDetector, DangerDetectorError, DetectorExit};
pub use flusher::MempoolFlusher;
use sequencer_core::protocol::ProtocolTiming;

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("flush: {0}")]
    Flush(#[from] flusher::FlushError),
    #[error("input reader: {0}")]
    InputReader(#[from] InputReaderError),
    #[error("provider: {0}")]
    Provider(String),
    #[error("startup refused: {0:?}")]
    Refuse(RefuseReason),
}

/// Why startup cannot proceed safely.
///
/// Each variant captures a DB/L1-view state that makes recovery or normal
/// startup unsafe. The operator sees the variant in logs and must intervene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The L1 safe block timestamp is too old or unknown, so the local L1 view
    /// is not usable for recovery or continued soft confirmations.
    L1ViewStale,
    /// Batch-relative wall-clock estimation says this batch consumed its
    /// remaining runway, but the observed safe block has not crossed danger.
    /// Refuse rather than recover from estimated state.
    EstimatedBatchInDanger { batch_index: u64 },
}

/// What a fresh startup must do, given the current danger state.
///
/// Pure function output — no side effects. The `run_preemptive_recovery`
/// driver executes the chosen action.
///
/// The four non-Refuse variants encode the recovery split:
///
/// - `Proceed`: no danger detected. No recovery work needed; the genesis Tip
///   (fresh DB) is opened by the structural `ensure_open_tip` step, not here.
/// - `RecoverTip`: aging Tip, no closed batch in danger. The Tip has no L1
///   footprint, so we cascade it directly with no flush.
/// - `FlushAndCascade`: closed batch in danger. We need a flush to resolve
///   its L1 transaction's fate before the cascade decision.
/// - `Refuse`: can't proceed safely; surface to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    /// No danger; no DB writes (genesis Tip handled by `ensure_open_tip`).
    Proceed,
    /// Open Tip is past `danger_threshold` and no closed batch is in danger.
    /// No flush needed (Tip has no L1 slot to resolve); cascade the Tip
    /// directly.
    RecoverTip { batch_index: u64 },
    /// Closed batch past the gold frontier is in danger. Flush the mempool,
    /// re-sync, then run the post-flush cascade.
    FlushAndCascade { batch_index: u64 },
    /// Can't proceed safely; return the reason and let the operator decide.
    Refuse(RefuseReason),
}

impl StartupAction {
    fn label(self) -> &'static str {
        match self {
            StartupAction::Proceed => "proceed",
            StartupAction::RecoverTip { .. } => "recover_tip",
            StartupAction::FlushAndCascade { .. } => "flush_and_cascade",
            StartupAction::Refuse(_) => "refuse",
        }
    }
}

fn danger_status_label(danger: DangerStatus) -> &'static str {
    match danger {
        DangerStatus::Safe => "safe",
        DangerStatus::L1ViewStale => "l1_view_stale",
        DangerStatus::ClosedBatchInDanger(_) => "closed_batch_in_danger",
        DangerStatus::TipInDanger(_) => "tip_in_danger",
        DangerStatus::EstimatedBatchInDanger(_) => "estimated_batch_in_danger",
    }
}

fn danger_batch_index(danger: DangerStatus) -> Option<u64> {
    match danger {
        DangerStatus::ClosedBatchInDanger(batch_index)
        | DangerStatus::TipInDanger(batch_index)
        | DangerStatus::EstimatedBatchInDanger(batch_index) => Some(batch_index),
        DangerStatus::Safe | DangerStatus::L1ViewStale => None,
    }
}

fn refuse_reason_label(reason: RefuseReason) -> &'static str {
    match reason {
        RefuseReason::L1ViewStale => "l1_view_stale",
        RefuseReason::EstimatedBatchInDanger { .. } => "estimated_batch_in_danger",
    }
}

/// Pure decision: given the danger status, return what startup should do. L1
/// reachability is an execution concern: if `FlushAndCascade` cannot reach L1,
/// the flusher returns an error and the orchestrator retries.
pub fn decide_startup_action(danger: DangerStatus) -> StartupAction {
    match danger {
        DangerStatus::Safe => StartupAction::Proceed,
        DangerStatus::ClosedBatchInDanger(batch_index) => {
            StartupAction::FlushAndCascade { batch_index }
        }
        DangerStatus::TipInDanger(batch_index) => StartupAction::RecoverTip { batch_index },
        DangerStatus::L1ViewStale => StartupAction::Refuse(RefuseReason::L1ViewStale),
        DangerStatus::EstimatedBatchInDanger(batch_index) => {
            StartupAction::Refuse(RefuseReason::EstimatedBatchInDanger { batch_index })
        }
    }
}

/// Run the full preemptive recovery procedure at startup.
///
/// 1. Try to sync the safe head from L1. If L1 is unreachable, continue with
///    the persisted view; whether that view is fresh enough is decided by
///    `check_danger` in step 2 — a stale persisted view returns
///    `L1ViewStale` and step 3 refuses.
/// 2. Consult [`decide_startup_action`] to pick what to do.
/// 3. If the decision is `FlushAndCascade`: flush the mempool, re-sync, then
///    continue. If `Refuse`: bail out and let the orchestrator retry.
/// 4. Run the atomic recovery transaction (cascade stale batches if any,
///    always re-open the Tip if missing).
///
/// Returns the list of invalidated batch indices (empty if no stale batches).
pub async fn run_preemptive_recovery(
    db_path: &str,
    input_reader: &mut InputReader,
    l1_config: &L1Config,
    protocol: &ProtocolTiming,
) -> Result<Vec<u64>, RecoveryError> {
    // ── Step 1: Sync safe head (tolerate L1 failure) ───────────────
    //
    // `sync_to_current_safe_head` goes through `append_safe_inputs`, which
    // maintains `safe_accepted_batches` atomically with each advance. After
    // a successful sync, the scheduler-frontier view is consistent with
    // l1_safe_head for every downstream reader.
    let l1_reachable = match input_reader.sync_to_current_safe_head().await {
        Ok(()) => {
            tracing::info!("L1 safe head synced");
            true
        }
        Err(e) => {
            let InputReaderError::Provider(error) = e else {
                return Err(RecoveryError::InputReader(e));
            };
            tracing::error!(error = %error, "L1 unreachable during startup safe-head sync");
            false
        }
    };

    // ── Step 2: Read danger and decide action ─────────────────────
    let danger = {
        let mut storage = storage::Storage::open(db_path)?;
        storage.check_danger(protocol, crate::runtime::clock::unix_now_ms())?
    };
    let action = decide_startup_action(danger);
    tracing::info!(
        danger_status = danger_status_label(danger),
        danger_batch_index = ?danger_batch_index(danger),
        startup_action = action.label(),
        l1_reachable,
        danger_threshold = protocol.danger_threshold(),
        max_wait_blocks = protocol.max_wait_blocks,
        l1_read_stale_after_blocks = protocol.l1_read_stale_after_blocks,
        "startup recovery decision"
    );

    // ── Step 3: Execute decision ───────────────────────────────────
    //
    // The three non-Refuse paths split the recovery work:
    //
    // - `Proceed`: no DB writes. A `Proceed` decision means no batch is in
    //   danger and the persisted state is fine as-is. Closed batches past
    //   gold (if any) stay in their natural lifecycle.
    //
    // - `RecoverTip`: no flush. Only the open Tip crossed `danger_threshold`;
    //   it has no L1 slot to resolve, so it can be invalidated directly.
    //
    // - `FlushAndCascade`: flush resolves every wallet-nonce slot, then
    //   re-sync brings the gold frontier to its maximum extent. After that
    //   point, *everything past gold is doomed* (Silver-stale,
    //   Silver-poisoned, or Pending-killed — see `Storage::recover_post_flush`
    //   docs). Cascade unconditionally from the first non-gold.
    let invalidated = match action {
        StartupAction::Proceed => {
            tracing::info!(
                danger_status = danger_status_label(danger),
                danger_batch_index = ?danger_batch_index(danger),
                startup_action = action.label(),
                "no danger zone detected — proceeding without recovery"
            );
            // No DB writes here. A `Proceed` decision means no batch is in
            // danger and the persisted state is fine as-is; closed batches past
            // gold (if any) stay in their natural lifecycle. The tip-existence
            // invariant — including opening the genesis Tip on a fresh DB — is
            // established structurally by `Storage::ensure_open_tip` in
            // `Workers::spawn`, after this returns and before the lane starts.
            Vec::new()
        }
        StartupAction::RecoverTip { batch_index } => {
            tracing::error!(
                danger_status = danger_status_label(danger),
                danger_batch_index = ?danger_batch_index(danger),
                startup_action = action.label(),
                tip_batch_index = batch_index,
                danger_threshold = protocol.danger_threshold(),
                "open Tip in danger zone — invalidating and opening fresh Tip (no flush)"
            );
            let mut storage = storage::Storage::open(db_path)?;
            storage.recover_aging_tip(protocol.danger_threshold())?
        }
        StartupAction::FlushAndCascade { batch_index } => {
            tracing::error!(
                danger_status = danger_status_label(danger),
                danger_batch_index = ?danger_batch_index(danger),
                startup_action = action.label(),
                batch_index,
                danger_threshold = protocol.danger_threshold(),
                max_wait_blocks = protocol.max_wait_blocks,
                "closed batch in danger zone — entering preemptive recovery (flush + cascade)"
            );
            run_flush_and_cascade(db_path, input_reader, l1_config, protocol).await?
        }
        StartupAction::Refuse(reason) => {
            tracing::error!(
                danger_status = danger_status_label(danger),
                danger_batch_index = ?danger_batch_index(danger),
                startup_action = action.label(),
                ?reason,
                refuse_reason = refuse_reason_label(reason),
                l1_reachable,
                "startup refused: cannot recover safely"
            );
            return Err(RecoveryError::Refuse(reason));
        }
    };

    if invalidated.is_empty() {
        tracing::info!(
            danger_status = danger_status_label(danger),
            danger_batch_index = ?danger_batch_index(danger),
            startup_action = action.label(),
            invalidated_count = 0,
            "startup recovery complete — no batches invalidated"
        );
    } else {
        // Successful self-heal: the system invalidated the doomed suffix and
        // opened a recovery batch as designed. The upstream "danger detected"
        // log already alerted the operator at error level; this completes
        // that incident with a non-error outcome.
        tracing::warn!(
            danger_status = danger_status_label(danger),
            danger_batch_index = ?danger_batch_index(danger),
            startup_action = action.label(),
            invalidated_count = invalidated.len(),
            batches = ?invalidated,
            "startup recovery complete — batches invalidated and recovery batch opened"
        );
    }

    Ok(invalidated)
}

/// Execute the flush-and-cascade phase: resolve every pending wallet-nonce
/// slot on L1, re-sync the safe head so the gold frontier reflects post-flush
/// state, then cascade-invalidate the doomed non-gold suffix and open a fresh
/// recovery Tip.
///
/// The four steps form one logical phase — they have no meaning on their own
/// and the orchestrator only ever runs them as a unit.
async fn run_flush_and_cascade(
    db_path: &str,
    input_reader: &mut InputReader,
    l1_config: &L1Config,
    protocol: &ProtocolTiming,
) -> Result<Vec<u64>, RecoveryError> {
    let flush_provider = crate::l1::provider::create_signer_provider(
        &l1_config.eth_rpc_url,
        &l1_config.batch_submitter_private_key,
    )
    .map_err(|e| RecoveryError::Provider(e.to_string()))?;
    let flusher = MempoolFlusher::new(
        flush_provider,
        l1_config.batch_submitter_address,
        protocol.seconds_per_block,
    );
    flusher.flush_and_wait().await?;

    // If this re-sync errors out, L1 has been flushed but the DB has NOT been
    // cascaded — we exit with the InputReaderError and rely on the orchestrator
    // to respawn. That's safe by design:
    //
    // - `flush_and_wait` is idempotent: on the next attempt it queries L1 for
    //   pending wallet-nonces, finds zero (the previous flush cleared them),
    //   and returns immediately.
    // - `check_danger` is stable across the failure window: safe_block only
    //   moves forward and flush doesn't retroactively change closed batches'
    //   `first_frame_safe_block`, so the danger condition that fired before
    //   still fires after the restart.
    // - `recover_post_flush` is idempotent against the resulting DB state
    //   (verified by `after_post_recovery_crash_is_no_op` in `recovery_tests`).
    //
    // So a failure here just costs an extra orchestrator respawn; correctness
    // is preserved.
    //
    // More importantly, it refuses to boot, during a recovery scenario, when
    // we can't reach L1.
    tracing::info!("re-syncing L1 safe head after flush");
    input_reader.sync_to_current_safe_head().await?;

    tracing::info!("running post-flush recovery (cascade non-gold suffix)");
    let mut storage = storage::Storage::open(db_path)?;
    Ok(storage.recover_post_flush(protocol.danger_threshold())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proceed_on_safe() {
        assert_eq!(
            decide_startup_action(DangerStatus::Safe),
            StartupAction::Proceed
        );
    }

    #[test]
    fn flush_and_cascade_on_closed_batch_in_danger() {
        assert_eq!(
            decide_startup_action(DangerStatus::ClosedBatchInDanger(42)),
            StartupAction::FlushAndCascade { batch_index: 42 }
        );
    }

    #[test]
    fn refuse_on_l1_view_stale() {
        assert_eq!(
            decide_startup_action(DangerStatus::L1ViewStale),
            StartupAction::Refuse(RefuseReason::L1ViewStale)
        );
    }

    #[test]
    fn refuse_on_estimated_batch_in_danger() {
        assert_eq!(
            decide_startup_action(DangerStatus::EstimatedBatchInDanger(7)),
            StartupAction::Refuse(RefuseReason::EstimatedBatchInDanger { batch_index: 7 })
        );
    }

    #[test]
    fn recover_tip_in_danger() {
        assert_eq!(
            decide_startup_action(DangerStatus::TipInDanger(11)),
            StartupAction::RecoverTip { batch_index: 11 }
        );
    }
}
