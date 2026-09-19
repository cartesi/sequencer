// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Recovery storage: danger inspection, guarded suffix invalidation, and Tip creation.
//!
//! See `docs/recovery/README.md` for the procedure, safety arguments, and
//! bounded model coverage. This file's job is to enforce that design
//! locally — read the design first if you're touching this code.
//!
//! Free functions here are shared with the batch submitter
//! (`l1_submission.rs`); they take `&Connection` / `&Transaction` so the
//! startup path can compose them into one atomic transaction.
//!
//! ## Fault model
//!
//! Recovery is robust to submission and outage failures (crashes, network
//! errors, mempool drops, extended downtime). It is NOT designed to defend
//! against arbitrarily malformed self-submissions: the scheduler-frontier
//! materialization in [`super::safe_accepted_batches`] trusts that on-chain
//! batches from the sequencer's own address are structurally valid. The
//! sequencer controls its own submissions; the threat model records that
//! self-trust boundary.

use rusqlite::{Connection, OptionalExtension, Result, Transaction, TransactionBehavior, params};
use sequencer_core::protocol::{ProtocolTiming, age_exceeds};

use super::Storage;
use super::convert::{i64_to_u64, now_unix_ms, u64_to_i64};
use super::history::advance_recovery_generation_in;
use super::ingress::open_fresh_tip_in_tx;
use super::queries::{
    current_safe_block_required, current_safe_block_timestamp, last_safe_progress_ms,
};
use super::safe_accepted_batches::{canonical_divergence_in, frontier_nonce};
use super::snapshot_dumps::has_rollback_safe_snapshot_in;

/// Outcome of a danger-zone check.
///
/// Each variant maps to a distinct response in the startup recovery procedure:
///
/// - `L1ViewStale` → retry boot. The L1 safe block is too old or unknown.
/// - `ClosedBatchInDanger(closed_idx)` → Flush → Sync → Cascade.
/// - `TipInDanger(tip_idx)` → direct Tip recovery, no flush. The Tip has no L1
///   footprint, so we can invalidate it and open a fresh one without
///   any L1 round-trip.
/// - `EstimatedBatchInDanger(idx)` → retry boot. The observed safe block is
///   still below the danger threshold, but wall-clock time since the last
///   safe-head advance has consumed the batch's remaining runway.
/// - `Safe` → admit when a Tip exists, otherwise run `EnsureOpenTip` and
///   re-inspect.
///
/// The runtime danger detector treats every non-`Safe` variant as
/// "exit for recovery" — the difference between them only matters at the
/// next startup, where the dispatch differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerStatus {
    /// No danger detected — none of the checks tripped.
    Safe,
    /// A fully-accepted L1 landing failed the content-identity check:
    /// canonical state contains executed effects with no
    /// reliable local source. Carries the diverged batch nonce. Ranked
    /// ahead of every other arm so the respawn loop can never route a
    /// diverged node into a provider call, mutation, or admission. The remedy
    /// is cockroach recovery (wipe + rebuild from L1), never standard recovery.
    CanonicalDivergence(u64),
    /// L1 safe-head timestamp is too old/unknown, or the current clock
    /// predates one of the persisted safety baselines. Recovery cannot reason
    /// from the local L1 view, so startup must retry.
    L1ViewStale,
    /// Observed-safe check tripped on a *closed* batch past the
    /// accepted frontier: aged beyond `protocol.danger_threshold()` against
    /// the observed safe block. L1 view is fresh; flushing and cascading is
    /// meaningful.
    ClosedBatchInDanger(u64),
    /// Observed-safe check tripped on the open *Tip*: aged beyond
    /// `protocol.danger_threshold()` against the observed safe block, but
    /// no closed batch is in danger. L1 view is fresh; the Tip has no L1
    /// footprint, so direct recovery (no flush) is correct.
    TipInDanger(u64),
    /// Batch-relative wall-clock estimate tripped after the global L1 view
    /// freshness check passed. We refuse rather than recover because the batch
    /// only crossed danger in estimated time, not observed safe-state.
    EstimatedBatchInDanger(u64),
}

/// One transactionally consistent local view consumed by the startup
/// recovery procedure.
///
/// Keeping these facts together is load-bearing: admission and repair
/// selection must not combine a danger verdict from one SQLite snapshot with
/// Tip/snapshot/head facts from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecoveryInspection {
    pub(crate) danger: DangerStatus,
    pub(crate) has_recovery_checkpoint: bool,
    pub(crate) has_open_tip: bool,
    pub(crate) current_safe_block: Option<u64>,
}

/// A recovery mutation was refused because the transaction no longer
/// satisfies the selected startup action's preconditions.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RecoveryMutationError {
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("canonical divergence at batch nonce {nonce} forbids standard recovery")]
    CanonicalDivergence { nonce: u64 },
    #[error("recovery decision is stale: expected {expected:?}, found {actual:?}")]
    StaleDecision {
        expected: DangerStatus,
        actual: DangerStatus,
    },
    #[error("cannot open the Tip without a recovery checkpoint")]
    MissingRecoveryCheckpoint,
    /// `EnsureOpenTip` found a valid open Tip already present. A
    /// stale no-Tip decision, not a danger change; unreachable under the
    /// process lock, and retryable if it ever fires.
    #[error("the Tip was already open when the EnsureOpenTip phase ran")]
    TipAlreadyOpen,
    /// `EnsureOpenTip` left no valid open Tip after opening one. This broken
    /// postcondition must roll back and refuse startup.
    #[error("the EnsureOpenTip phase left no valid open Tip in its own transaction")]
    TipMissingAfterOpen,
    #[error(
        "post-flush re-sync reached safe block {resynced_safe_block}, behind the flush observation at {flush_observed_safe_block}"
    )]
    ResyncBehindFlushView {
        resynced_safe_block: u64,
        flush_observed_safe_block: u64,
    },
    #[error("post-flush re-sync did not persist a safe head")]
    MissingSafeHead,
}

impl DangerStatus {
    /// Stable label for logs/metrics. An inherent method (not a free
    /// projection) so a new variant must add its label right here.
    pub(crate) fn label(self) -> &'static str {
        match self {
            DangerStatus::Safe => "safe",
            DangerStatus::CanonicalDivergence(_) => "canonical_divergence",
            DangerStatus::L1ViewStale => "l1_view_stale",
            DangerStatus::ClosedBatchInDanger(_) => "closed_batch_in_danger",
            DangerStatus::TipInDanger(_) => "tip_in_danger",
            DangerStatus::EstimatedBatchInDanger(_) => "estimated_batch_in_danger",
        }
    }

    /// The batch nonce a danger arm points at, if any (log context). The
    /// `CanonicalDivergence` nonce is deliberately not reported here — it is a
    /// diverged-state nonce, not a batch in the danger pipeline.
    pub(crate) fn batch_index(self) -> Option<u64> {
        match self {
            DangerStatus::ClosedBatchInDanger(batch_index)
            | DangerStatus::TipInDanger(batch_index)
            | DangerStatus::EstimatedBatchInDanger(batch_index) => Some(batch_index),
            DangerStatus::Safe
            | DangerStatus::L1ViewStale
            | DangerStatus::CanonicalDivergence(_) => None,
        }
    }
}

impl Storage {
    /// Whether the canonical-divergence marker (I15) is present, and the
    /// recorded `(nonce, safe_input_index)` if so. Standard
    /// recovery is forbidden while the marker exists; callers on the recovery
    /// path must check this before any batch-tree mutation.
    pub fn canonical_divergence(&mut self) -> Result<Option<(u64, u64)>> {
        self.read(|tx| canonical_divergence_in(tx))
    }

    /// Unified danger-zone detection.
    ///
    /// Runs checks inside a single read transaction, in priority order:
    ///
    /// 1. **Canonical divergence**: an already-confirmed mismatch is an
    ///    absorbing terminal fact and outranks every view/clock condition.
    /// 2. **L1 view freshness**: if the safe block timestamp is missing or
    ///    older than `protocol.l1_read_stale_after_blocks`, return
    ///    `L1ViewStale`. A stale L1 *view* is unusable even if the RPC
    ///    answers — recovery itself needs a trustworthy view, so this gate
    ///    stays ahead of everything.
    /// 3. **Observed closed-frontier**: `find_closed_frontier_batch_in_danger`
    ///    against `protocol.danger_threshold()`. Uses the observed safe block.
    /// 4. **Observed open Tip**: `find_tip_batch_in_danger` against
    ///    `protocol.danger_threshold()`. Catches the case where all closed
    ///    batches are gold but the Tip is aging — the lane is stuck or the
    ///    Tip rotated without a safe-block advance.
    /// 5. **Clock faults**, deliberately after the observed arms: a local
    ///    clock a full block-time or more out of step with either persisted
    ///    baseline (behind the safe-block timestamp, or behind the local
    ///    last-progress baseline) is a *clock* fault, not a view fault. The
    ///    observed arms (3, 4) are pure block arithmetic, and a wall-clock
    ///    fault must never suppress a danger verdict that stands on L1
    ///    observation alone. Sub-block skew in either direction is
    ///    quantization noise, not a fault.
    /// 6. **Batch-relative wall-clock estimate**: if a correction applies
    ///    ([`ProtocolTiming::wall_clock_adjusted_danger_threshold`] returns
    ///    `Some`), widens to `find_first_batch_in_danger` against
    ///    `danger_threshold − missed_blocks`. This is a fallback for when the
    ///    observed safe block has not crossed danger yet, but wall-clock time
    ///    since the last safe-head advance says the provider view is too stale
    ///    to trust for continued soft confirmations.
    ///
    /// Returns the first variant that fires, in the order
    /// `CanonicalDivergence` → `L1ViewStale` (stale view) →
    /// `ClosedBatchInDanger` → `TipInDanger` → `L1ViewStale` (clock fault) →
    /// `EstimatedBatchInDanger` → `Safe`. The order encodes the
    /// "trust" hierarchy:
    ///
    /// - **View staleness gates everything after divergence.** If the safe
    ///   block timestamp is too old or unknown, neither recovery nor
    ///   continued soft confirmations are honest.
    /// - **Clock faults yield to observed danger.** A local clock a full
    ///   block-time or more out of step with either persisted baseline
    ///   refuses only when no observed danger stands; sub-block skew is
    ///   tolerated.
    /// - **Closed observed danger beats Tip.** When a closed batch is in danger,
    ///   we need a flush (to resolve its L1 transaction's fate) regardless
    ///   of the Tip's state. The cascade naturally catches the Tip via
    ///   `batch_index >= N`.
    /// - **Tip is the residual.** Only fires when no closed batch is in
    ///   danger. Routes to direct Tip recovery — no flush needed.
    /// - **Estimated danger is the fallback.** If the observed safe-state checks have
    ///   not crossed the threshold, but wall-clock extrapolation says they
    ///   would have crossed had the safe head kept advancing, startup refuses
    ///   instead of issuing soft confirmations on a stale L1 view.
    ///
    /// `now_ms` is passed in (rather than read from `SystemTime::now()` here)
    /// so the storage layer stays testable without time mocking. Production
    /// callers pass the current Unix-ms clock.
    pub fn check_danger(&mut self, protocol: &ProtocolTiming, now_ms: u64) -> Result<DangerStatus> {
        self.read(|tx| check_danger_in(tx, protocol, now_ms))
    }

    /// Read every local fact used by the startup recovery procedure in one transaction.
    pub(crate) fn inspect_recovery(
        &mut self,
        protocol: &ProtocolTiming,
        now_ms: u64,
    ) -> Result<RecoveryInspection> {
        self.read(|tx| inspect_recovery_in(tx, protocol, now_ms))
    }

    /// Execute `EnsureOpenTip` only if its local decision
    /// still holds in the write transaction.
    pub(crate) fn ensure_open_tip_for_recovery(
        &mut self,
        protocol: &ProtocolTiming,
        now_ms: u64,
    ) -> std::result::Result<(), RecoveryMutationError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let facts = inspect_recovery_in(&tx, protocol, now_ms)?;
        refuse_divergence(facts.danger)?;
        if !facts.has_recovery_checkpoint {
            return Err(RecoveryMutationError::MissingRecoveryCheckpoint);
        }
        if facts.danger != DangerStatus::Safe {
            return Err(RecoveryMutationError::StaleDecision {
                expected: DangerStatus::Safe,
                actual: facts.danger,
            });
        }
        if facts.has_open_tip {
            return Err(RecoveryMutationError::TipAlreadyOpen);
        }
        open_fresh_tip_in_tx(&tx)?;
        // A broken postcondition must roll back this transaction and refuse
        // startup, including in release builds.
        if !has_valid_open_batch(&tx)? {
            return Err(RecoveryMutationError::TipMissingAfterOpen);
        }
        tx.commit()?;
        Ok(())
    }

    /// Execute `RecoverTip` only while the same Tip is
    /// still the observed-danger arm in the write transaction.
    pub(crate) fn recover_aging_tip_for_recovery(
        &mut self,
        expected_batch_index: u64,
        protocol: &ProtocolTiming,
        now_ms: u64,
    ) -> std::result::Result<Vec<u64>, RecoveryMutationError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let facts = inspect_recovery_in(&tx, protocol, now_ms)?;
        refuse_divergence(facts.danger)?;
        if !facts.has_recovery_checkpoint {
            return Err(RecoveryMutationError::MissingRecoveryCheckpoint);
        }
        let expected = DangerStatus::TipInDanger(expected_batch_index);
        if facts.danger != expected {
            return Err(RecoveryMutationError::StaleDecision {
                expected,
                actual: facts.danger,
            });
        }
        let invalidated = recover_aging_tip_inner(&tx, protocol.danger_threshold())?;
        tx.commit()?;
        Ok(invalidated)
    }

    /// Execute post-flush Cascade. The boot-local flush observation is
    /// represented by its observed safe-block floor; this transaction
    /// reasserts both I15 and the post-flush resync coherence check
    /// immediately before changing the batch tree.
    pub(crate) fn recover_post_flush_for_recovery(
        &mut self,
        flush_observed_safe_block: u64,
        protocol: &ProtocolTiming,
        now_ms: u64,
    ) -> std::result::Result<Vec<u64>, RecoveryMutationError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let facts = inspect_recovery_in(&tx, protocol, now_ms)?;
        refuse_divergence(facts.danger)?;
        if !facts.has_recovery_checkpoint {
            return Err(RecoveryMutationError::MissingRecoveryCheckpoint);
        }
        let resynced_safe_block = facts
            .current_safe_block
            .ok_or(RecoveryMutationError::MissingSafeHead)?;
        if resynced_safe_block < flush_observed_safe_block {
            return Err(RecoveryMutationError::ResyncBehindFlushView {
                resynced_safe_block,
                flush_observed_safe_block,
            });
        }
        let invalidated = recover_post_flush_inner(&tx, protocol.danger_threshold())?;
        tx.commit()?;
        Ok(invalidated)
    }

    /// Mark a single batch as invalid. Test-only seeder — production code goes
    /// through [`Storage::recover_post_flush_for_recovery`] or
    /// [`Storage::recover_aging_tip_for_recovery`].
    /// Idempotent: leaves already-invalid rows alone.
    #[cfg(test)]
    pub(crate) fn insert_invalid_batch(&mut self, batch_index: u64) -> Result<()> {
        let now_ms = now_unix_ms();
        self.conn.execute(
            "UPDATE batches SET invalidated_at_ms = ?1 \
             WHERE batch_index = ?2 AND invalidated_at_ms IS NULL",
            params![now_ms, u64_to_i64(batch_index)],
        )?;
        Ok(())
    }

    /// Test-only unguarded Cascade primitive. Production calls
    /// [`Storage::recover_post_flush_for_recovery`]; the design rationale
    /// lives on [`recover_post_flush_inner`], the shared body.
    #[cfg(test)]
    pub fn recover_post_flush(&mut self, danger_threshold: u64) -> Result<Vec<u64>> {
        self.write(|tx| recover_post_flush_inner(tx, danger_threshold))
    }

    /// Test-only unguarded primitive; production calls
    /// [`Storage::recover_aging_tip_for_recovery`], which transactionally
    /// reasserts the exact startup decision. Design rationale on
    /// [`recover_aging_tip_inner`], the shared body.
    #[cfg(test)]
    pub fn recover_aging_tip(&mut self, danger_threshold: u64) -> Result<Vec<u64>> {
        self.write(|tx| recover_aging_tip_inner(tx, danger_threshold))
    }
}

pub(super) fn inspect_recovery_in(
    conn: &Connection,
    protocol: &ProtocolTiming,
    now_ms: u64,
) -> Result<RecoveryInspection> {
    let danger = check_danger_in(conn, protocol, now_ms)?;
    let has_recovery_checkpoint = has_rollback_safe_snapshot_in(conn)?;
    Ok(RecoveryInspection {
        danger,
        has_recovery_checkpoint,
        has_open_tip: has_valid_open_batch(conn)?,
        current_safe_block: super::queries::current_safe_block(conn)?,
    })
}

fn check_danger_in(
    conn: &Connection,
    protocol: &ProtocolTiming,
    now_ms: u64,
) -> Result<DangerStatus> {
    // The divergence marker outranks everything — including the L1-staleness
    // gate. It records an already-confirmed canonical fact.
    if let Some((nonce, _)) = canonical_divergence_in(conn)? {
        return Ok(DangerStatus::CanonicalDivergence(nonce));
    }

    let safe_block_timestamp = current_safe_block_timestamp(conn)?;
    let last_progress = last_safe_progress_ms(conn)?;
    if protocol.l1_view_is_stale(safe_block_timestamp, now_ms) {
        return Ok(DangerStatus::L1ViewStale);
    }

    let danger_threshold = protocol.danger_threshold();
    if let Some(idx) = find_closed_frontier_batch_in_danger(conn, danger_threshold)? {
        return Ok(DangerStatus::ClosedBatchInDanger(idx));
    }
    if let Some(idx) = find_tip_batch_in_danger(conn, danger_threshold)? {
        return Ok(DangerStatus::TipInDanger(idx));
    }

    if protocol.clock_cannot_age_l1_view(safe_block_timestamp, now_ms) {
        return Ok(DangerStatus::L1ViewStale);
    }
    let adjusted_danger_threshold =
        match protocol.wall_clock_adjusted_danger_threshold(last_progress, now_ms) {
            Ok(adjusted) => adjusted,
            Err(_) => return Ok(DangerStatus::L1ViewStale),
        };
    if let Some(adjusted) = adjusted_danger_threshold
        && let Some(idx) = find_first_batch_in_danger(conn, adjusted)?
    {
        return Ok(DangerStatus::EstimatedBatchInDanger(idx));
    }
    Ok(DangerStatus::Safe)
}

fn refuse_divergence(danger: DangerStatus) -> std::result::Result<(), RecoveryMutationError> {
    if let DangerStatus::CanonicalDivergence(nonce) = danger {
        return Err(RecoveryMutationError::CanonicalDivergence { nonce });
    }
    Ok(())
}

// ── Free functions used by both recovery and the batch submitter ──────────

/// Discard the entire non-accepted closed suffix after flush and caught-up Sync.
/// This is a convergence policy: replaced or never-submitted work could be
/// submitted fresh, but preserving it can re-enter the same danger/recovery cycle.
/// The caller must settle wallet slots and refresh acceptance through the flush
/// observation; the production guard checks that the local view caught up.
///
/// If no closed pivot remains, only an aging Tip is invalidated. A closed batch
/// can have landed fresh while the Tip, even with the same first-frame clock,
/// has since crossed the danger threshold. Cascade and reopening share `tx`.
///
/// Returns the newly-invalidated batch indices (empty if none).
fn recover_post_flush_inner(tx: &Transaction<'_>, danger_threshold: u64) -> Result<Vec<u64>> {
    // Path 1: any closed batch past gold cascades unconditionally.
    let pivot = match first_non_gold_closed_batch(tx)? {
        Some(batch_index) => Some(batch_index),
        // All closed batches are accepted; the Tip can still have aged.
        None => find_tip_batch_in_danger(tx, danger_threshold)?,
    };
    cascade_and_reopen(tx, pivot)
}

/// Discard only the aging Tip. It has no L1 footprint, so no flush is required.
/// Using `danger_threshold` avoids restarting with the same age that triggered
/// recovery; it is a policy threshold, not proof of canonical staleness.
/// The production caller rechecks the exact `TipInDanger` decision against the
/// current local inspection before entering this shared body.
///
/// Returns the newly-invalidated batch indices (empty if Tip is fresh,
/// `[tip_index]` when the Tip was cascaded).
fn recover_aging_tip_inner(tx: &Transaction<'_>, danger_threshold: u64) -> Result<Vec<u64>> {
    let pivot = find_tip_batch_in_danger(tx, danger_threshold)?;
    cascade_and_reopen(tx, pivot)
}

/// Shared tail of both recovery paths — the pivot selection above is the
/// only thing that varies. In the caller's transaction:
///
/// 1. **Cascade** from `pivot` (no-op when `None`): invalidate it and every
///    successor, including the open Tip.
/// 2. Snapshot selection excludes invalidated batches in the same committed
///    state. Accepted batch snapshots survive; before any acceptance the
///    baseline supplies the rollback-safe restore point.
/// 3. **Advance `RecoveryGeneration`** exactly once when the cascade
///    invalidated any valid batch, recording the surviving application count
///    before replacement directs can reuse its offsets. Cut, generation, and
///    invalidation remain inseparable across crashes.
/// 4. **Reopen the Tip** the cascade just invalidated (or one a torn crash
///    left missing), atomically with the cascade. Same mechanism the
///    runtime's genesis path uses — see `ingress::open_fresh_tip_in_tx`.
fn cascade_and_reopen(tx: &Transaction<'_>, pivot: Option<u64>) -> Result<Vec<u64>> {
    let invalidated = match pivot {
        Some(batch_index) => cascade_invalidate_from(tx, batch_index)?,
        None => Vec::new(),
    };
    if !invalidated.is_empty() {
        advance_recovery_generation_in(tx)?;
    }
    if !invalidated.is_empty() || !has_valid_open_batch(tx)? {
        open_fresh_tip_in_tx(tx)?;
    }
    Ok(invalidated)
}

/// First valid closed batch sitting at the gold frontier — i.e., with
/// `nonce >= frontier_nonce` (the next nonce the scheduler is expected to
/// accept). Used by [`recover_post_flush_inner`] as the cascade pivot, and
/// by [`find_closed_frontier_batch_in_danger`] as the candidate to age-check.
///
/// `>=`, not `>`: `frontier_nonce` is the *next-expected* nonce
/// (`latest_accepted.nonce + 1`, or the anchor before any acceptance), so the
/// actual cascade-pivot batch carries `nonce == frontier_nonce`. Using `>`
/// would skip it.
///
/// Valid-path nonce contiguity (I16) makes the first match exactly
/// `frontier_nonce`. Returns `None` if all closed batches are accepted.
fn first_non_gold_closed_batch(conn: &Connection) -> Result<Option<u64>> {
    let frontier = frontier_nonce(conn)?;
    let batch_index: Option<i64> = conn
        .query_row(
            "SELECT batch_index FROM valid_closed_batches \
             WHERE nonce >= ?1 ORDER BY nonce ASC LIMIT 1",
            rusqlite::params![u64_to_i64(frontier)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(batch_index.map(i64_to_u64))
}

/// Either the closed-frontier batch or the Tip, whichever (if either) has
/// aged past `threshold` against `current_safe_block`. Used by
/// [`Storage::check_danger`]'s wall-clock-adjusted arm, where the dispatch
/// is the same (`Retry`) regardless of which one fired.
///
/// Closed-frontier wins: frame `safe_block`s are non-decreasing along the
/// spine, so an existing closed frontier is at least as old as the Tip.
///
/// Reads `safe_accepted_batches`, which is maintained atomically with each
/// [`Storage::append_safe_inputs`] call.
pub(super) fn find_first_batch_in_danger(conn: &Connection, threshold: u64) -> Result<Option<u64>> {
    if let Some(batch_index) = find_closed_frontier_batch_in_danger(conn, threshold)? {
        return Ok(Some(batch_index));
    }
    find_tip_batch_in_danger(conn, threshold)
}

/// First valid closed batch past the gold frontier whose first frame is older
/// than `current_safe_block - threshold`. Returns `None` if no such batch
/// exists.
///
/// First-frame clocks are non-decreasing along the valid path (I3), so the
/// earliest non-accepted closed batch is at least as old as its successors.
/// Checking younger batches cannot reveal danger that this check missed.
///
/// Does NOT consider the Tip — the Tip has no L1 transaction, so it's not
/// part of the closed-frontier-staleness category.
/// [`find_first_batch_in_danger`] composes with [`find_tip_batch_in_danger`]
/// when callers want both.
pub(super) fn find_closed_frontier_batch_in_danger(
    conn: &Connection,
    threshold: u64,
) -> Result<Option<u64>> {
    match first_non_gold_closed_batch(conn)? {
        Some(batch_index) => batch_in_danger(conn, batch_index, threshold),
        None => Ok(None),
    }
}

/// The Tip (if any) whose first frame is older than
/// `current_safe_block - threshold`. Returns `None` if no Tip exists or it
/// isn't in danger yet.
fn find_tip_batch_in_danger(conn: &Connection, threshold: u64) -> Result<Option<u64>> {
    let tip_batch_index: Option<i64> = conn
        .query_row("SELECT batch_index FROM valid_open_batch", [], |row| {
            row.get(0)
        })
        .optional()?;
    match tip_batch_index {
        Some(tip_batch_index) => batch_in_danger(conn, i64_to_u64(tip_batch_index), threshold),
        None => Ok(None),
    }
}

/// Shared age-check used by the closed-frontier and Tip helpers. Returns
/// `Some(batch_index)` if `current_safe_block - first_frame.safe_block >= threshold`.
fn batch_in_danger(conn: &Connection, batch_index: u64, threshold: u64) -> Result<Option<u64>> {
    let first_frame_safe_block = first_frame_safe_block_of(conn, u64_to_i64(batch_index))?;
    let safe_block = current_safe_block_required(conn)?;
    Ok(age_exceeds(safe_block, first_frame_safe_block, threshold).then_some(batch_index))
}

/// `frames.safe_block` of the lowest `frame_in_batch` in `batch_index`.
///
/// Every committed valid batch has a first frame. Missing one is an
/// invariant violation and propagates as `QueryReturnedNoRows`.
fn first_frame_safe_block_of(conn: &Connection, batch_index: i64) -> Result<u64> {
    conn.query_row(
        "SELECT safe_block FROM frames \
         WHERE batch_index = ?1 ORDER BY frame_in_batch ASC LIMIT 1",
        params![batch_index],
        |row| row.get::<_, i64>(0).map(i64_to_u64),
    )
}

/// Cascade-invalidate all valid batches with `batch_index >= from_batch_index`.
///
/// Reads the list BEFORE mutating — the SELECT must see the rows the UPDATE
/// will then mark invalid. The `invalidated_at_ms IS NULL` guard on the UPDATE
/// keeps this idempotent: rows already invalid are untouched.
fn cascade_invalidate_from(tx: &Transaction<'_>, from_batch_index: u64) -> Result<Vec<u64>> {
    let from_i64 = u64_to_i64(from_batch_index);

    let invalidated: Vec<u64> = {
        let mut stmt = tx.prepare(
            "SELECT batch_index FROM valid_batches \
             WHERE batch_index >= ?1 ORDER BY batch_index ASC",
        )?;
        stmt.query_map(params![from_i64], |row| {
            row.get::<_, i64>(0).map(i64_to_u64)
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    if !invalidated.is_empty() {
        let now_ms = now_unix_ms();
        tx.execute(
            "UPDATE batches SET invalidated_at_ms = ?1 \
             WHERE batch_index >= ?2 AND invalidated_at_ms IS NULL",
            params![now_ms, from_i64],
        )?;
    }

    Ok(invalidated)
}

/// Check whether the DB has a valid Tip (`sealed_at_ms IS NULL AND
/// `invalidated_at_ms IS NULL`).
fn has_valid_open_batch(tx: &Connection) -> Result<bool> {
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM valid_open_batch", [], |row| {
        row.get(0)
    })?;
    Ok(count > 0)
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
