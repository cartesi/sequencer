// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Preemptive recovery: detect danger zone, flush mempool, cascade-invalidate stale batches.
//!
//! At startup the sequencer checks if any batch is approaching the staleness deadline.
//! If so, it flushes the L1 mempool (competing with pending batch transactions using
//! no-op replacements), re-syncs the safe head, and runs the atomic recovery procedure
//! (populate scheduler frontier, assign nonces, detect stale, cascade-invalidate,
//! open recovery batch).
//!
//! At runtime a dedicated [`DangerDetector`] worker performs the same danger-zone
//! check each tick. If it fires, the detector exits with `DetectorExit::DangerZone`,
//! the runtime treats that as a `RunError::DangerZoneDetected`, and the process exits.
//! External orchestration restarts the sequencer, and this startup path runs again.
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
//! ## Lifecycle
//!
//! ```text
//!   steady state                      danger
//!   ┌──────────┐                      ┌──────────┐
//!   │ running  │──detector tick──▶ 🚨 │ exiting  │
//!   └──────────┘                      └─────┬────┘
//!        ▲                                  │ non-zero exit
//!        │                                  ▼
//!   ┌────┴─────┐                ┌─────────────────┐
//!   │  normal  │◀───────────────│ orchestrator    │──respawn──▶ startup
//!   │  ticks   │                │ (systemd/k8s)   │                │
//!   └──────────┘                └─────────────────┘                ▼
//!                                                         ┌────────────────────┐
//!                                                         │ run_preemptive_    │
//!                                                         │   recovery         │
//!                                                         │  ├─ try L1 resync  │
//!                                                         │  ├─ decide action  │
//!                                                         │  ├─ flush + cascade│
//!                                                         │  └─ open batch     │
//!                                                         └────────────────────┘
//! ```
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    /// No danger; proceed to run the recovery transaction (which is a no-op
    /// on a healthy state apart from opening a Tip if one is missing).
    Proceed,
    /// Strict danger with a fresh L1 view. Flush the mempool, re-sync, then
    /// run the recovery transaction.
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
    last_safe_progress_ms: u64,
) -> StartupAction {
    let ever_synced = last_safe_progress_ms != 0;

    // First-boot guard: if we've never seen a real safe-head observation AND
    // we can't contact L1 to refresh it, we have nothing to base a safety
    // decision on. Refuse.
    if !ever_synced && !l1_reachable {
        return StartupAction::Refuse(RefuseReason::NeverSyncedAndUnreachable);
    }

    match (danger, l1_reachable) {
        (DangerStatus::Safe, _) => StartupAction::Proceed,
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
    match action {
        StartupAction::Proceed => {
            tracing::info!("no danger zone detected — skipping flush");
        }
        StartupAction::FlushAndCascade { batch_index } => {
            tracing::error!(
                batch_index,
                danger_threshold = protocol.danger_threshold(),
                max_wait_blocks = protocol.max_wait_blocks,
                "danger zone detected — entering preemptive recovery"
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
        }
        StartupAction::Refuse(reason) => {
            tracing::error!(
                ?reason,
                reachable = l1_reachable,
                "startup refused: flush cannot run safely"
            );
            return Err(RecoveryError::Refuse(reason));
        }
    }

    // ── Step 4: Atomic recovery ────────────────────────────────────
    //
    // `safe_accepted_batches` is already caught up to `l1_safe_head` (step 1
    // and, if we flushed, step 3 re-synced it). The recovery transaction only
    // needs to cascade + open.
    tracing::info!("running startup recovery (detect stale, cascade-invalidate, open recovery)");
    let mut det_storage = storage::Storage::open(db_path)?;
    let invalidated = det_storage.detect_and_recover(protocol.max_wait_blocks)?;

    if invalidated.is_empty() {
        tracing::info!("no stale batches found — continuing normally");
    } else {
        tracing::error!(
            count = invalidated.len(),
            batches = ?invalidated,
            "stale batches invalidated — recovery batch opened"
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
            decide_startup_action(DangerStatus::Safe, true, 0),
            StartupAction::Proceed
        );
        assert_eq!(
            decide_startup_action(DangerStatus::Safe, false, 1_000_000),
            StartupAction::Proceed
        );
    }

    #[test]
    fn flush_and_cascade_on_strict_plus_reachable() {
        assert_eq!(
            decide_startup_action(DangerStatus::Strict(42), true, 1_000_000),
            StartupAction::FlushAndCascade { batch_index: 42 }
        );
    }

    #[test]
    fn refuse_on_strict_plus_unreachable() {
        assert_eq!(
            decide_startup_action(DangerStatus::Strict(42), false, 1_000_000),
            StartupAction::Refuse(RefuseReason::StrictDangerButUnreachable { batch_index: 42 })
        );
    }

    #[test]
    fn refuse_on_stalled_regardless_of_l1() {
        assert_eq!(
            decide_startup_action(DangerStatus::Stalled(7), true, 1_000_000),
            StartupAction::Refuse(RefuseReason::StalledSafeHead { batch_index: 7 })
        );
        assert_eq!(
            decide_startup_action(DangerStatus::Stalled(7), false, 1_000_000),
            StartupAction::Refuse(RefuseReason::StalledSafeHead { batch_index: 7 })
        );
    }

    #[test]
    fn refuse_when_never_synced_and_unreachable() {
        assert_eq!(
            decide_startup_action(DangerStatus::Safe, false, 0),
            StartupAction::Refuse(RefuseReason::NeverSyncedAndUnreachable)
        );
    }

    #[test]
    fn never_synced_but_reachable_proceeds() {
        // First-boot happy path: we've never observed the safe head before,
        // but L1 is reachable so step 1 just did the first sync.
        assert_eq!(
            decide_startup_action(DangerStatus::Safe, true, 0),
            StartupAction::Proceed
        );
    }
}
