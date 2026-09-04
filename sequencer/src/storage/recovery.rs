// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Recovery writer: cascade-invalidates stale batches, opens recovery batches,
//! and composes the startup-recovery transaction.
//!
//! See `docs/recovery/README.md` for the full design (batch tree, coloring,
//! nonce poisoning, TLA+ proof). This file's job is to enforce that design
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
//! sequencer controls its own submissions — this is a deliberate system
//! assumption, not a gap.

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
use super::snapshot_dumps::{batch_nonce_in, clear_pending_dumps_from_nonce_in};

/// Outcome of a danger-zone check.
///
/// Each variant maps to a distinct response in the startup recovery reducer:
///
/// - `L1ViewStale` → retry boot. The L1 safe block is too old or unknown.
/// - `ClosedBatchInDanger(closed_idx)` → enter the phase-granular
///   Flush/Sync/Cascade sequence.
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
/// recovery reducer.
///
/// Keeping these facts together is load-bearing: admission and recovery-phase
/// selection must not combine a danger verdict from one SQLite snapshot with
/// Tip/snapshot/head facts from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecoveryInspection {
    pub(crate) danger: DangerStatus,
    pub(crate) has_finalized_snapshot: bool,
    pub(crate) has_open_tip: bool,
    pub(crate) current_safe_block: Option<u64>,
}

/// A recovery mutation was refused because the transaction no longer
/// satisfies the phase selected by the reducer.
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
    #[error("cannot open the Tip without a finalized snapshot")]
    MissingFinalizedSnapshot,
    /// The `EnsureOpenTip` phase found a valid open Tip already present. A
    /// stale no-Tip decision, not a danger change; unreachable under the
    /// process lock, and retryable if it ever fires.
    #[error("the Tip was already open when the EnsureOpenTip phase ran")]
    TipAlreadyOpen,
    /// The `EnsureOpenTip` phase's own transaction left no valid open Tip
    /// after opening one. Impossible by construction; refused rather than
    /// committed, so the reducer's one cycle (Repaired → EnsureOpenTip →
    /// Repaired) cannot spin on it.
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

    /// Read every local fact used by the startup reducer in one transaction.
    pub(crate) fn inspect_recovery(
        &mut self,
        protocol: &ProtocolTiming,
        now_ms: u64,
    ) -> Result<RecoveryInspection> {
        self.read(|tx| inspect_recovery_in(tx, protocol, now_ms))
    }

    /// Execute the reducer's `EnsureOpenTip` phase only if its local decision
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
        if !facts.has_finalized_snapshot {
            return Err(RecoveryMutationError::MissingFinalizedSnapshot);
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
        // Postcondition, enforced where it can be violated: this phase is the
        // only edge back into `Repaired` without a Tip, so it must never
        // commit without one. A violation is a typed refuse (exit 30), never
        // a retry that would spin the reducer, and never a `debug_assert`
        // that compiles out.
        if !has_valid_open_batch(&tx)? {
            return Err(RecoveryMutationError::TipMissingAfterOpen);
        }
        tx.commit()?;
        Ok(())
    }

    /// Execute the reducer's `RecoverTip` phase only while the same Tip is
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
        if !facts.has_finalized_snapshot {
            return Err(RecoveryMutationError::MissingFinalizedSnapshot);
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

    /// Execute the reducer's `Cascade` phase. The ephemeral flush witness is
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
        if !facts.has_finalized_snapshot {
            return Err(RecoveryMutationError::MissingFinalizedSnapshot);
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
    /// through [`Storage::recover_post_flush`] or [`Storage::recover_aging_tip`].
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
    /// reasserts the exact reducer decision. Design rationale on
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
    let has_finalized_snapshot = has_finalized_snapshot_in(conn)?;
    Ok(RecoveryInspection {
        danger,
        has_finalized_snapshot,
        has_open_tip: has_valid_open_batch(conn)?,
        current_safe_block: super::queries::current_safe_block(conn)?,
    })
}

fn has_finalized_snapshot_in(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM finalized_snapshot)",
        [],
        |row| row.get(0),
    )
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

/// Cascade the non-gold suffix and open a fresh recovery batch (the shared
/// body behind [`Storage::recover_post_flush_for_recovery`], which the
/// reducer reaches after carrying a Flush witness through a caught-up
/// Sync). Homed here, not on the test wrapper, so rustdoc builds it and a
/// wrapper cleanup cannot delete the design record.
///
/// # The "everything past gold is doomed" rule
///
/// At this point the gold frontier is at its maximum extent: every
/// submitted batch has either been accepted (gold) or rejected by the
/// scheduler simulation (Silver-stale, since nonce-mismatch is impossible
/// at the frontier under self-trust), or its tx was killed by a flush
/// no-op (Pending, no `safe_input`). All three non-gold states are doomed:
///
/// - **Silver-stale:** scheduler skipped it; downstream batches are
///   nonce-poisoned.
/// - **Pending:** the original L1 tx is dead. Re-submission could in
///   principle land fresh, but the *next* recovery cycle's flush would
///   compete with the resub at its new wallet-nonce slot and the bumped
///   no-op typically wins. The system would loop until current staleness
///   crossed `MAX_WAIT_BLOCKS`. Cascading now converges in one cycle.
///
/// So once we've committed to recovery (the danger detector tripped, the
/// flush ran), the right move is to cascade the entire non-gold suffix
/// and open a fresh recovery batch.
///
/// Three aftermath shapes:
///
/// 1. **Everything worked:** all in-flight batches landed fresh and were
///    accepted. Gold extends to the last submitted batch; no first
///    non-gold closed. (See "Tip handling" below for the subtle subcase.)
/// 2. **Mixed:** some landed (stale or poisoned), some replaced. First
///    non-gold closed is either Silver-stale or Pending. Cascade from
///    there; the `batch_index >= N` rule catches the rest of the suffix
///    including the open Tip.
/// 3. **All replaced:** flush no-ops won every race. Gold doesn't
///    advance; first non-gold closed is the very first non-accepted batch.
///
/// # Tip handling
///
/// In cases (2)/(3) the cascade catches the Tip via `batch_index >= N`.
/// In case (1), there's no closed pivot — but the Tip can still be in
/// the danger zone:
///
/// When the lane rotates a batch without a safe-block advance between
/// frames (e.g. immediately after init, when both share the bootstrap
/// `safe_block`), the Tip's `first_frame.safe_block` equals the closed
/// batch's. The closed batch can become gold by inclusion-staleness
/// (`inclusion_block - first_frame < MAX_WAIT`) while the Tip's age,
/// computed against `current_safe_block` after the flush wait, has
/// crossed `danger_threshold`. Pure monotonicity (`S_tip ≥ S_closed`) doesn't
/// rule this out — equality is allowed.
///
/// So in the no-pivot branch we additionally check the Tip against
/// `danger_threshold` (the same threshold that would have triggered
/// recovery had the Tip been a closed batch). We're already committed
/// to recovery; the Tip is past gold; if it's also in the danger zone,
/// cascade it and open a fresh one.
///
/// # Atomicity
///
/// Runs as a single SQLite write transaction. On crash mid-way, the
/// txn rolls back; on commit, the cascade and the recovery batch open
/// land together. Idempotent on re-run because `valid_*` views filter
/// out already-invalidated rows.
///
/// # Precondition
///
/// The caller MUST have just synced L1 state via
/// [`Storage::append_safe_inputs`]; the gold frontier in
/// `safe_accepted_batches` must reflect the latest safe head. Otherwise
/// the cascade may invalidate batches that haven't yet had a chance to
/// be processed by the scheduler simulation.
///
/// Returns the newly-invalidated batch indices (empty if none).
fn recover_post_flush_inner(tx: &Transaction<'_>, danger_threshold: u64) -> Result<Vec<u64>> {
    // Path 1: any closed batch past gold cascades unconditionally.
    let pivot = match first_non_gold_closed_batch(tx)? {
        Some(batch_index) => Some(batch_index),
        // Path 2 (corner case): all closed are gold, but the Tip might be
        // in the danger zone — see `recover_post_flush` doc on Tip handling.
        None => find_tip_batch_in_danger(tx, danger_threshold)?,
    };
    cascade_and_reopen(tx, pivot)
}

/// Cascade the open Tip if its first frame has aged past
/// `danger_threshold` (the shared body behind
/// [`Storage::recover_aging_tip_for_recovery`]). Homed here, not on the
/// test wrapper, so rustdoc builds it and a wrapper cleanup cannot delete
/// the design record.
///
/// # Why a threshold here, but no closed-frontier check
///
/// Outside a flush path, closed batches past the gold
/// frontier (if any) might still be in their natural lifecycle —
/// pending in the mempool, recently included, awaiting safe finality.
/// Cascading them would prematurely abort their progression.
///
/// The Tip is different: it has no L1 footprint at all (no `w_nonce`,
/// no `safe_input`), so there's no L1 outcome to wait on. Once its
/// first frame has aged into the danger zone, the rule "everything
/// past gold is bad once we're committed to recovery" applies, and in
/// the `RecoverTip` path startup is already committed.
///
/// # Threshold = danger_threshold, not MAX_WAIT
///
/// We use `danger_threshold` (= `MAX_WAIT_BLOCKS - margin`) rather than
/// `MAX_WAIT_BLOCKS`. The Tip threshold is the same one that would
/// trigger the recovery cycle had the Tip been a closed batch. If the
/// Tip is past that threshold, the next danger detector tick after
/// resume would re-trip on the Tip's eventual first close + submission
/// anyway (the closed batch would inherit its first frame's safe_block).
/// Cascading now saves the cycle.
///
/// # Precondition
///
/// As with [`Storage::recover_post_flush`], the caller must have synced
/// L1 state. (Threshold comparison reads `current_safe_block` from
/// `l1_safe_head`.)
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
/// 2. **Clear doomed pending snapshots, scoped to the cascade**: delete
///    pending rows with `nonce >= pivot.nonce` — exactly the cascaded
///    batches' pendings, states the canonical replay will never reach.
///    Gold-but-unpromoted pendings (batches that landed while the process
///    was down) carry lower nonces and *survive*: catch-up resumes from a
///    fresher checkpoint, and the rows are cleaned up by the next
///    promotion's `DELETE <= max_nonce`. Scoping is load-bearing: a
///    blanket clear would arm a promote-wedge crash-loop whenever a
///    *valid in-flight* closed batch existed at clear time — its pending
///    row would be deleted while the batch stayed valid, and the lane's
///    later promotion of its landing would hit the deleted row with no
///    danger arm ever firing to heal it. With the scope, any nonce the
///    lane can later observe as accepted either has its pending row intact
///    or belongs to a post-recovery batch with a fresh row. In the
///    `RecoverTip` path the scope deletes nothing — the Tip never has a
///    pending row. Finalized is untouched (L1-confirmed bytes).
/// 3. **Advance `RecoveryGeneration`** exactly once when the cascade
///    invalidated any valid batch. This is the externally visible statement
///    that the current era's soft-history reality changed; composing it here
///    makes generation and invalidation inseparable across crashes.
/// 4. **Reopen the Tip** the cascade just invalidated (or one a torn crash
///    left missing), atomically with the cascade. Same mechanism the
///    runtime's genesis path uses — see `ingress::open_fresh_tip_in_tx`.
fn cascade_and_reopen(tx: &Transaction<'_>, pivot: Option<u64>) -> Result<Vec<u64>> {
    let invalidated = match pivot {
        Some(batch_index) => {
            let pivot_nonce = batch_nonce_in(tx, batch_index)?;
            let invalidated = cascade_invalidate_from(tx, batch_index)?;
            clear_pending_dumps_from_nonce_in(tx, pivot_nonce)?;
            invalidated
        }
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
/// (`latest_accepted.nonce + 1`), so the actual cascade-pivot batch carries
/// `nonce == frontier_nonce`. Using `>` would skip it.
///
/// On the valid path, batch nonces are contiguous (enforced by the
/// `trg_enforce_nonce_contiguity` trigger), so the first match always has
/// `nonce == frontier_nonce`. We don't double-check that invariant here —
/// the trigger is the source of truth (see AGENTS.md "Self-trust": no
/// defense-in-depth checks against the sequencer's own bugs). Returns
/// `None` if all closed batches are gold.
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
/// is the same (`Refuse`) regardless of which one fired.
///
/// Closed-frontier wins: frame `safe_block`s are non-decreasing along the
/// spine, so the closed frontier is at least as *old* as the Tip — whenever
/// the Tip is in danger, the closed frontier is too, and cascading from the
/// closed batch covers the Tip via `batch_index >= N`. (This ordering is
/// load-bearing for the pending-snapshot clear — see `docs/invariants.md`.)
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
/// Why look only at the frontier batch, not "every batch past gold"?
/// `safe_accepted_batches` is updated atomically with each safe-head advance
/// (see [`super::safe_accepted_batches`]) and walks the spine until it hits
/// a barrier — a stale batch, or a missing slot the scheduler can't bridge.
/// So the first batch past the frontier IS the barrier; downstream batches
/// are nonce-poisoned by definition (a stale frontier ⇒ scheduler skips ⇒
/// every later batch arrives at an unexpected nonce). Looking further is
/// redundant.
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
