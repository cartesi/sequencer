// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Preemptive recovery: detect danger zone, then recover via Tip invalidation
//! or post-flush cascade.
//!
//! At runtime a dedicated [`DangerDetector`] worker polls `Storage::check_danger` each
//! tick. If a closed batch past the gold frontier crosses `danger_threshold`, the
//! open Tip crosses `danger_threshold`, or the wall-clock-adjusted variant fires
//! during an L1 outage, the detector exits with `DetectorExit::DangerZone`, the
//! runtime maps that to `RunError::DangerZoneDetected`, and the process exits.
//! The detector tripping is *only* a trigger to enter recovery — it doesn't make
//! the cascade decision. External orchestration restarts the sequencer, and this
//! startup path runs.
//!
//! Startup recovery branches on [`decide_startup_action`]:
//!
//! - `FlushAndCascade`: a closed batch past gold is dangerous. Flush the mempool,
//!   re-sync the safe head, then call [`Storage::recover_post_flush`] which cascades
//!   everything past the gold frontier (every non-gold batch is doomed:
//!   Silver-stale, Silver-poisoned, or Pending no-op'd). If all closed are gold,
//!   falls through to a Tip danger-zone check — see `docs/recovery/README.md` Step 5.
//! - `RecoverTip`: only the open Tip is dangerous. It has no L1 footprint, so call
//!   [`Storage::recover_aging_tip`] directly without flushing.
//! - `Proceed`: no danger detected. Call [`Storage::recover_aging_tip`] which only
//!   invalidates the open Tip if it has aged past `danger_threshold`. Closed batches
//!   past gold are left alone (they may still be in their natural lifecycle).
//! - `Refuse`: bail out and surface to the operator.
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
use sequencer_core::protocol::ProtocolConfig;

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
/// Each variant captures a distinct combination of L1 reachability and DB
/// state that makes the flush-then-cascade recovery either unsafe or
/// impossible. The operator sees the variant in logs and must intervene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// No prior safe-head observation was ever recorded AND L1 is unreachable.
    /// We have no baseline to trust for the wall-clock estimate, and can't
    /// refresh it either. First boot requires L1.
    NeverSyncedAndUnreachable,
    /// The wall-clock-adjusted check flagged a stale batch, which means the
    /// safe head itself appears frozen. `flush_and_wait` would spin waiting
    /// for a safe head that isn't advancing, so we refuse instead.
    StalledSafeHead { batch_index: u64 },
    /// Strict danger detected but L1 is unreachable. We can't run the flush
    /// step safely without a live L1 provider; refusing gives the operator a
    /// chance to restore L1 before retrying.
    StrictDangerButUnreachable { batch_index: u64 },
}

/// What a fresh startup must do, given the current (danger, L1-reachable,
/// ever-synced) state.
///
/// Pure function output — no side effects. The `run_preemptive_recovery`
/// driver executes the chosen action.
///
/// The four non-Refuse variants encode the recovery split:
///
/// - `Proceed`: no danger detected. Just ensure the Tip exists (torn-state
///   crash recovery branch).
/// - `RecoverTip`: aging Tip, no closed batch in danger. The Tip has no L1
///   footprint, so we cascade it directly with no flush.
/// - `FlushAndCascade`: closed batch in danger. We need a flush to resolve
///   its L1 transaction's fate before the cascade decision.
/// - `Refuse`: can't proceed safely; surface to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    /// No danger; ensure the Tip exists if missing (no cascade).
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

/// Pure decision: given the danger status, whether L1 is reachable, and
/// whether we've ever recorded a safe-head observation, return what startup
/// should do. All the startup-policy complexity lives here, isolated from
/// storage and RPC side effects.
pub fn decide_startup_action(
    danger: DangerStatus,
    l1_reachable: bool,
    last_safe_progress_ms: Option<u64>,
) -> StartupAction {
    // First-boot guard: if we've never seen a real safe-head observation AND
    // we can't contact L1 to refresh it, we have nothing to base a safety
    // decision on. Refuse.
    if last_safe_progress_ms.is_none() && !l1_reachable {
        return StartupAction::Refuse(RefuseReason::NeverSyncedAndUnreachable);
    }

    match (danger, l1_reachable) {
        (DangerStatus::Safe, _) => StartupAction::Proceed,
        // Tip-in-danger doesn't require L1 (no flush needed), so we recover
        // even when L1 is unreachable. The freshly-opened recovery Tip uses
        // the cached `current_safe_block`; if L1 stays down, the wall-clock
        // arm will eventually fire on the new Tip and we'll Refuse.
        (DangerStatus::Tip(batch_index), _) => StartupAction::RecoverTip { batch_index },
        (DangerStatus::Strict(batch_index), true) => StartupAction::FlushAndCascade { batch_index },
        (DangerStatus::Strict(batch_index), false) => {
            StartupAction::Refuse(RefuseReason::StrictDangerButUnreachable { batch_index })
        }
        (DangerStatus::Stalled(batch_index), _) => {
            StartupAction::Refuse(RefuseReason::StalledSafeHead { batch_index })
        }
    }
}

/// Run the full preemptive recovery procedure at startup.
///
/// 1. Try to sync the safe head from L1. If L1 is unreachable, fall through
///    using the persisted safe head plus the wall-clock estimator.
/// 2. Consult [`decide_startup_action`] to pick what to do.
/// 3. If the decision is `FlushAndCascade`: flush the mempool, re-sync, then
///    continue.
/// 4. Run the atomic recovery transaction (cascade stale batches if any,
///    always re-open the Tip if missing).
///
/// Returns the list of invalidated batch indices (empty if no stale batches).
pub async fn run_preemptive_recovery(
    db_path: &str,
    input_reader: &mut InputReader,
    l1_config: &L1Config,
    protocol: &ProtocolConfig,
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

    // ── Step 2: Read danger + last-progress, decide action ─────────
    let (danger, last_safe_progress_ms) = {
        let mut storage = storage::Storage::open(db_path)?;
        let last = storage.last_safe_progress_ms()?;
        let danger = storage.check_danger(protocol, crate::runtime::clock::unix_now_ms())?;
        (danger, last)
    };
    let action = decide_startup_action(danger, l1_reachable, last_safe_progress_ms);

    // ── Step 3: Execute decision ───────────────────────────────────
    //
    // The three non-Refuse paths split the recovery work:
    //
    // - `Proceed`: no flush. Closed batches past the gold frontier (if any)
    //   may still be in their natural lifecycle and shouldn't be cascaded. The
    //   Tip check is defensive and should normally be a no-op because `Tip`
    //   danger routes to `RecoverTip`.
    //
    // - `RecoverTip`: no flush. Only the open Tip crossed `danger_threshold`;
    //   it has no L1 slot to resolve, so it can be invalidated directly.
    //
    // - `FlushAndCascade`: flush resolves every wallet-nonce slot, then
    //   re-sync brings the gold frontier to its maximum extent. After that
    //   point, *everything past gold is doomed* (Silver-stale,
    //   Silver-poisoned, or Pending-killed — see `Storage::recover_post_flush`
    //   docs). Cascade unconditionally from the first non-gold.
    let mut det_storage = storage::Storage::open(db_path)?;
    let invalidated = match action {
        StartupAction::Proceed => {
            tracing::info!("no danger zone detected — skipping flush");
            // Pass the threshold defensively: in the Proceed path the Tip
            // shouldn't be in danger (the danger check would have surfaced
            // RecoverTip), but the threshold check inside `recover_aging_tip`
            // is cheap and guards against a stale check_danger reading.
            det_storage.recover_aging_tip(protocol.danger_threshold())?
        }
        StartupAction::RecoverTip { batch_index } => {
            tracing::error!(
                tip_batch_index = batch_index,
                danger_threshold = protocol.danger_threshold(),
                "open Tip in danger zone — invalidating and opening fresh Tip (no flush)"
            );
            det_storage.recover_aging_tip(protocol.danger_threshold())?
        }
        StartupAction::FlushAndCascade { batch_index } => {
            tracing::error!(
                batch_index,
                danger_threshold = protocol.danger_threshold(),
                max_wait_blocks = protocol.max_wait_blocks,
                "closed batch in danger zone — entering preemptive recovery (flush + cascade)"
            );

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

            tracing::info!("re-syncing L1 safe head after flush");
            input_reader.sync_to_current_safe_head().await?;

            tracing::info!("running post-flush recovery (cascade non-gold suffix)");
            det_storage.recover_post_flush(protocol.danger_threshold())?
        }
        StartupAction::Refuse(reason) => {
            tracing::error!(
                ?reason,
                reachable = l1_reachable,
                "startup refused: cannot recover safely"
            );
            return Err(RecoveryError::Refuse(reason));
        }
    };

    if invalidated.is_empty() {
        tracing::info!("no batches invalidated — continuing normally");
    } else {
        // Successful self-heal: the system invalidated the doomed suffix and
        // opened a recovery batch as designed. The upstream "danger detected"
        // log already alerted the operator at error level; this completes
        // that incident with a non-error outcome.
        tracing::warn!(
            count = invalidated.len(),
            batches = ?invalidated,
            "batches invalidated — recovery batch opened"
        );
    }

    Ok(invalidated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proceed_on_safe_regardless_of_l1() {
        assert_eq!(
            decide_startup_action(DangerStatus::Safe, true, None),
            StartupAction::Proceed
        );
        assert_eq!(
            decide_startup_action(DangerStatus::Safe, false, Some(1_000_000)),
            StartupAction::Proceed
        );
    }

    #[test]
    fn flush_and_cascade_on_strict_plus_reachable() {
        assert_eq!(
            decide_startup_action(DangerStatus::Strict(42), true, Some(1_000_000)),
            StartupAction::FlushAndCascade { batch_index: 42 }
        );
    }

    #[test]
    fn refuse_on_strict_plus_unreachable() {
        assert_eq!(
            decide_startup_action(DangerStatus::Strict(42), false, Some(1_000_000)),
            StartupAction::Refuse(RefuseReason::StrictDangerButUnreachable { batch_index: 42 })
        );
    }

    #[test]
    fn refuse_on_stalled_regardless_of_l1() {
        assert_eq!(
            decide_startup_action(DangerStatus::Stalled(7), true, Some(1_000_000)),
            StartupAction::Refuse(RefuseReason::StalledSafeHead { batch_index: 7 })
        );
        assert_eq!(
            decide_startup_action(DangerStatus::Stalled(7), false, Some(1_000_000)),
            StartupAction::Refuse(RefuseReason::StalledSafeHead { batch_index: 7 })
        );
    }

    #[test]
    fn recover_tip_regardless_of_l1_reachability() {
        // Tip recovery doesn't need a flush, so it works even when L1 is
        // unreachable. The Tip's first frame is in our DB; we can cascade
        // and open a fresh Tip without any RPC.
        assert_eq!(
            decide_startup_action(DangerStatus::Tip(11), true, Some(1_000_000)),
            StartupAction::RecoverTip { batch_index: 11 }
        );
        assert_eq!(
            decide_startup_action(DangerStatus::Tip(11), false, Some(1_000_000)),
            StartupAction::RecoverTip { batch_index: 11 }
        );
    }

    #[test]
    fn refuse_when_never_synced_and_unreachable() {
        assert_eq!(
            decide_startup_action(DangerStatus::Safe, false, None),
            StartupAction::Refuse(RefuseReason::NeverSyncedAndUnreachable)
        );
    }

    #[test]
    fn never_synced_but_reachable_proceeds() {
        // First-boot happy path: we've never observed the safe head before,
        // but L1 is reachable so step 1 just did the first sync.
        assert_eq!(
            decide_startup_action(DangerStatus::Safe, true, None),
            StartupAction::Proceed
        );
    }
}
