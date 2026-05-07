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

use rusqlite::{Connection, OptionalExtension, Result, Transaction, params};
use sequencer_core::protocol::{ProtocolConfig, age_exceeds};

use super::Storage;
use super::convert::{i64_to_u64, now_unix_ms, u64_to_i64};
use super::mutations::{insert_new_batch, insert_open_frame, persist_frame_direct_sequence};
use super::queries::{
    query_batch_policy, query_current_safe_block, query_latest_safe_input_index_exclusive,
};
use super::safe_accepted_batches::query_latest_safe_accepted_batch;

/// Outcome of a danger-zone check.
///
/// Each variant maps to a distinct recovery response, encoded in
/// [`super::super::recovery::StartupAction`]:
///
/// - `Strict(closed_idx)` → flush + cascade. A closed batch past the
///   accepted frontier has L1 transactions that may already be on chain;
///   we need the flush to resolve their fate before cascading.
/// - `Tip(tip_idx)` → direct Tip recovery, no flush. The Tip has no L1
///   footprint, so we can invalidate it and open a fresh one without
///   any L1 round-trip.
/// - `Stalled(idx)` → refuse boot. Safe head appears frozen; flushing
///   would spin waiting for `Pending <= Safe`.
/// - `Safe` → no recovery work; just ensure the Tip exists (torn-state
///   crash recovery branch).
///
/// The runtime danger detector treats `Strict`, `Tip`, and `Stalled` as
/// "exit for recovery" — the difference between them only matters at the
/// next startup, where the dispatch differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerStatus {
    /// No danger detected — none of the checks tripped.
    Safe,
    /// Strict, block-based check tripped on a *closed* batch past the
    /// accepted frontier: aged beyond `protocol.danger_threshold()` against
    /// the observed safe block. L1 view is fresh; flushing and cascading is
    /// meaningful.
    Strict(u64),
    /// Strict, block-based check tripped on the open *Tip*: aged beyond
    /// `protocol.danger_threshold()` against the observed safe block, but
    /// no closed batch is in danger. L1 view is fresh; the Tip has no L1
    /// footprint, so direct recovery (no flush) is correct.
    Tip(u64),
    /// Wall-clock-adjusted check tripped: an unresolved batch is estimated
    /// past the adjusted threshold because wall-clock time has elapsed past
    /// our last safe-head observation. The safe-head view may be stale or
    /// frozen — flushing against L1 may not terminate.
    Stalled(u64),
}

/// Wall-clock-adjusted danger threshold, if a correction applies.
///
/// Returns `None` when either:
/// - `last_safe_progress_ms` is `None` (no baseline — correction is undefined).
/// - Elapsed wall-clock hasn't reached at least one block interval yet (no
///   correction needed).
///
/// Returns `Some(adjusted_threshold)` where
/// `adjusted = danger_threshold - (elapsed_secs / seconds_per_block)`,
/// saturating at 0. The caller picks which DB-view query to run against this
/// threshold.
pub(super) fn wall_clock_adjusted_threshold(
    last_safe_progress_ms: Option<u64>,
    now_ms: u64,
    protocol: &ProtocolConfig,
) -> Option<u64> {
    let last = last_safe_progress_ms?;
    let elapsed_secs = now_ms.saturating_sub(last) / 1000;
    let missed = elapsed_secs / protocol.seconds_per_block.max(1);
    if missed == 0 {
        return None;
    }
    Some(protocol.danger_threshold().saturating_sub(missed))
}

impl Storage {
    /// Unified danger-zone detection.
    ///
    /// Runs three checks inside a single read transaction, in priority order:
    ///
    /// 1. **Wall-clock adjusted (stalled-safe-head)**: if a correction applies
    ///    ([`wall_clock_adjusted_threshold`] returns `Some`), widens to
    ///    `find_first_batch_in_danger` against
    ///    `danger_threshold − missed_blocks`. Catches the case where the safe
    ///    head appears frozen.
    /// 2. **Strict on closed-frontier**: `find_closed_frontier_batch_in_danger`
    ///    against `protocol.danger_threshold()`. Uses the observed safe block.
    /// 3. **Strict on open Tip**: `find_tip_batch_in_danger` against
    ///    `protocol.danger_threshold()`. Catches the case where all closed
    ///    batches are gold but the Tip is aging — the lane is stuck or the
    ///    Tip rotated without a safe-block advance.
    ///
    /// Returns the first variant that fires, in the order
    /// `Stalled` → `Strict` → `Tip` → `Safe`. The order encodes the
    /// "trust" hierarchy:
    ///
    /// - **Stalled wins ties.** A frozen safe head invalidates the strict
    ///   reading; refusing boot is the safe call. Earlier ordering returned
    ///   `Strict` first and routed to `FlushAndCascade`, which would then
    ///   spin in `flush_and_wait` against the frozen head.
    /// - **Closed-Strict beats Tip.** When a closed batch is in danger,
    ///   we need a flush (to resolve its L1 transaction's fate) regardless
    ///   of the Tip's state. The cascade naturally catches the Tip via
    ///   `batch_index >= N`.
    /// - **Tip is the residual.** Only fires when no closed batch is in
    ///   danger. Routes to direct Tip recovery — no flush needed.
    ///
    /// `now_ms` is passed in (rather than read from `SystemTime::now()` here)
    /// so the storage layer stays testable without time mocking. Production
    /// callers pass the current Unix-ms clock.
    pub fn check_danger(&mut self, protocol: &ProtocolConfig, now_ms: u64) -> Result<DangerStatus> {
        self.read(|tx| {
            // Wall-clock first: a frozen safe head invalidates the strict
            // check's input. If wall-clock-adjusted catches anything, refuse
            // (Stalled) regardless of what the strict checks would say.
            let last_raw: i64 = tx.query_row(
                "SELECT synced_at_ms FROM l1_safe_head WHERE singleton_id = 0",
                [],
                |row| row.get(0),
            )?;
            let last_safe_progress_ms = Some(i64_to_u64(last_raw)).filter(|&v| v != 0);

            if let Some(adjusted) =
                wall_clock_adjusted_threshold(last_safe_progress_ms, now_ms, protocol)
                && let Some(idx) = find_first_batch_in_danger(tx, adjusted)?
            {
                return Ok(DangerStatus::Stalled(idx));
            }

            let danger_threshold = protocol.danger_threshold();
            if let Some(idx) = find_closed_frontier_batch_in_danger(tx, danger_threshold)? {
                return Ok(DangerStatus::Strict(idx));
            }

            if let Some(idx) = find_tip_batch_in_danger(tx, danger_threshold)? {
                return Ok(DangerStatus::Tip(idx));
            }

            Ok(DangerStatus::Safe)
        })
    }

    /// Test-only wrapper around the strict (closed-frontier) danger helper,
    /// isolated so tests can target it directly without also running the
    /// wall-clock arm inside `check_danger`.
    #[cfg(test)]
    pub(crate) fn check_danger_zone(&mut self, danger_threshold: u64) -> Result<Option<u64>> {
        find_closed_frontier_batch_in_danger(&self.conn, danger_threshold)
    }

    /// Test-only wrapper around the broader (any-unresolved) danger helper.
    /// Same role as `check_danger_zone`: targeted testing of one arm in
    /// isolation.
    #[cfg(test)]
    pub(crate) fn check_any_unresolved_batch_in_danger(
        &mut self,
        threshold: u64,
    ) -> Result<Option<u64>> {
        find_first_batch_in_danger(&self.conn, threshold)
    }

    /// Mark a single batch as invalid. Test-only seeder — production code goes
    /// through [`Storage::recover_post_flush`] or [`Storage::recover_aging_tip`].
    #[cfg(test)]
    pub(crate) fn insert_invalid_batch(&mut self, batch_index: u64) -> Result<()> {
        let now_ms = now_unix_ms();
        // Only set if currently NULL — leaves already-invalid rows alone so this
        // remains idempotent (matching the previous `INSERT OR IGNORE` semantic).
        self.conn.execute(
            "UPDATE batches SET invalidated_at_ms = ?1 \
             WHERE batch_index = ?2 AND invalidated_at_ms IS NULL",
            params![now_ms, u64_to_i64(batch_index)],
        )?;
        Ok(())
    }

    /// Cascade everything past the gold frontier. Called from the
    /// `FlushAndCascade` startup path, after the mempool flush has resolved
    /// every wallet-nonce slot and `safe_accepted_batches` has been re-synced.
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
    pub fn recover_post_flush(&mut self, danger_threshold: u64) -> Result<Vec<u64>> {
        self.write(|tx| recover_post_flush_inner(tx, danger_threshold))
    }

    /// Cascade the open Tip if its first frame has aged past
    /// `danger_threshold`. Called from the `RecoverTip` startup path (no flush
    /// happened), and defensively from `Proceed`.
    ///
    /// # Why a threshold here, but no closed-frontier check
    ///
    /// In the Proceed path no flush ran, so closed batches past the gold
    /// frontier (if any) might still be in their natural lifecycle —
    /// pending in the mempool, recently included, awaiting safe finality.
    /// Cascading them would prematurely abort their progression.
    ///
    /// The Tip is different: it has no L1 footprint at all (no `w_nonce`,
    /// no `safe_input`), so there's no L1 outcome to wait on. Once its
    /// first frame has aged into the danger zone, the rule "everything
    /// past gold is bad once we're committed to recovery" applies. In the
    /// `RecoverTip` path startup is already committed; in `Proceed`, this
    /// branch is defensive and should normally be a no-op.
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
    pub fn recover_aging_tip(&mut self, danger_threshold: u64) -> Result<Vec<u64>> {
        self.write(|tx| recover_aging_tip_inner(tx, danger_threshold))
    }
}

// ── Free functions used by both recovery and the batch submitter ──────────

/// See [`Storage::recover_post_flush`] for the design rationale.
fn recover_post_flush_inner(tx: &Transaction<'_>, danger_threshold: u64) -> Result<Vec<u64>> {
    // Path 1: any closed batch past gold cascades unconditionally.
    let pivot = match first_non_gold_closed_batch(tx)? {
        Some(bi) => Some(bi),
        // Path 2 (corner case): all closed are gold, but the Tip might be
        // in the danger zone — see `recover_post_flush` doc on Tip handling.
        None => find_tip_batch_in_danger(tx, danger_threshold)?,
    };
    let invalidated = match pivot {
        Some(bi) => cascade_invalidate_from(tx, bi)?,
        None => Vec::new(),
    };
    if !invalidated.is_empty() || !has_valid_open_batch(tx)? {
        open_recovery_batch_in_tx(tx)?;
    }
    Ok(invalidated)
}

/// See [`Storage::recover_aging_tip`] for the design rationale.
fn recover_aging_tip_inner(tx: &Transaction<'_>, danger_threshold: u64) -> Result<Vec<u64>> {
    let invalidated = match find_tip_batch_in_danger(tx, danger_threshold)? {
        Some(bi) => cascade_invalidate_from(tx, bi)?,
        None => Vec::new(),
    };
    if !invalidated.is_empty() || !has_valid_open_batch(tx)? {
        open_recovery_batch_in_tx(tx)?;
    }
    Ok(invalidated)
}

/// First valid closed batch with `nonce >= frontier_nonce`. Used by
/// [`recover_post_flush_inner`] as the cascade pivot.
///
/// On the valid path, batch nonces are contiguous (enforced by the
/// `trg_enforce_nonce_contiguity` trigger), so the first match always has
/// `nonce == frontier_nonce`. Returns `None` if all closed batches are gold
/// (every aftermath case (1) — "everything worked").
fn first_non_gold_closed_batch(conn: &Connection) -> Result<Option<u64>> {
    let frontier_nonce = query_latest_safe_accepted_batch(conn)?
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(0);
    let batch_index: Option<i64> = conn
        .query_row(
            "SELECT batch_index FROM valid_closed_batches \
             WHERE nonce >= ?1 ORDER BY nonce ASC LIMIT 1",
            rusqlite::params![u64_to_i64(frontier_nonce)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(batch_index.map(i64_to_u64))
}

/// The oldest unresolved batch (closed-unaccepted OR open) whose first frame is
/// older than `current_safe_block - threshold`, or `None` if no such batch.
///
/// "Unresolved" means either:
///   (a) a closed batch past the accepted frontier, or
///   (b) the current Tip (still at risk of aging into danger).
///
/// Closed-unaccepted batches are strictly older than the Tip (the sequencer
/// opens new batches at monotonically non-decreasing `safe_block`), so the
/// closed-frontier check takes precedence. Cascading from that batch covers
/// the Tip automatically via `batch_index >= N`.
///
/// Used by [`Storage::check_danger`]'s wall-clock-adjusted arm. The
/// post-flush cascade path goes through [`first_non_gold_closed_batch`]
/// (closed-only) with [`find_tip_batch_in_danger`] as a fall-through;
/// the Proceed path goes through [`find_tip_batch_in_danger`] only.
///
/// Reads `safe_accepted_batches`, which is maintained atomically with each
/// [`Storage::append_safe_inputs`] call.
pub(super) fn find_first_batch_in_danger(conn: &Connection, threshold: u64) -> Result<Option<u64>> {
    if let Some(bi) = find_closed_frontier_batch_in_danger(conn, threshold)? {
        return Ok(Some(bi));
    }
    find_tip_batch_in_danger(conn, threshold)
}

/// First valid closed batch past the accepted frontier whose `first_frame_safe_block`
/// is older than `current_safe_block - threshold`. Returns `None` if no such
/// batch matches.
///
/// Does not consider the Tip — the submitter's zombie-detection check must
/// NOT flag the Tip (it has no L1 tx to become a zombie). The unified
/// entrypoint `find_first_batch_in_danger` falls through to
/// `find_tip_batch_in_danger` for that case.
pub(super) fn find_closed_frontier_batch_in_danger(
    conn: &Connection,
    threshold: u64,
) -> Result<Option<u64>> {
    let frontier_nonce = query_latest_safe_accepted_batch(conn)?
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(0);

    let batch_ref: Option<(i64, i64)> = conn
        .query_row(
            "SELECT batch_index, nonce FROM valid_closed_batches \
             WHERE nonce >= ?1 ORDER BY nonce ASC LIMIT 1",
            rusqlite::params![u64_to_i64(frontier_nonce)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((batch_index, batch_nonce)) = batch_ref else {
        return Ok(None);
    };
    if i64_to_u64(batch_nonce) != frontier_nonce {
        return Ok(None);
    }

    let first_frame_safe_block = first_frame_safe_block_of(conn, batch_index)?;
    let safe_block = query_current_safe_block(conn)?;
    if age_exceeds(safe_block, first_frame_safe_block, threshold) {
        Ok(Some(i64_to_u64(batch_index)))
    } else {
        Ok(None)
    }
}

/// The Tip (if any) whose `first_frame_safe_block` is older than
/// `current_safe_block - threshold`. Returns `None` if no Tip exists or it's
/// not yet in danger.
fn find_tip_batch_in_danger(conn: &Connection, threshold: u64) -> Result<Option<u64>> {
    let tip_bi: Option<i64> = conn
        .query_row("SELECT batch_index FROM valid_open_batch", [], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(tip_bi) = tip_bi else {
        return Ok(None);
    };

    let first_frame_safe_block = first_frame_safe_block_of(conn, tip_bi)?;
    let safe_block = query_current_safe_block(conn)?;
    if age_exceeds(safe_block, first_frame_safe_block, threshold) {
        Ok(Some(i64_to_u64(tip_bi)))
    } else {
        Ok(None)
    }
}

/// `frames.safe_block` of the lowest `frame_in_batch` in `batch_index`.
/// Returns 0 if the batch has no frames yet.
fn first_frame_safe_block_of(conn: &Connection, batch_index: i64) -> Result<u64> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT safe_block FROM frames \
             WHERE batch_index = ?1 ORDER BY frame_in_batch ASC LIMIT 1",
            params![batch_index],
            |row| row.get(0),
        )
        .optional()?;
    Ok(i64_to_u64(value.unwrap_or(0)))
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

/// Open a fresh recovery batch inside an existing transaction.
///
/// The new Tip's parent is the highest-indexed valid batch (the last valid
/// ancestor after the cascade). If none exists — the torn-state case where
/// every batch has been invalidated — the new Tip has no parent (nonce 0,
/// like a fresh genesis).
fn open_recovery_batch_in_tx(tx: &Transaction<'_>) -> Result<()> {
    let now_ms = now_unix_ms();
    let safe_block = query_current_safe_block(tx)?;

    let parent_batch_index: Option<u64> = tx
        .query_row("SELECT MAX(batch_index) FROM valid_batches", [], |row| {
            row.get::<_, Option<i64>>(0)
        })?
        .map(i64_to_u64);

    let policy = query_batch_policy(tx)?;
    let next_bi = insert_new_batch(tx, None, parent_batch_index, now_ms)?;
    insert_open_frame(tx, next_bi, 0, now_ms, policy.recommended_fee, safe_block)?;

    // Drain leading directs into the new batch's first frame.
    // Direct inputs from invalidated batches are re-drained into the recovery batch
    // (the UNIQUE(safe_input_index) constraint was removed to allow this).
    let next_undrained: u64 = {
        // MAX(safe_input_index) + 1 over the valid drained rows. Cursor rewinds
        // when a batch is invalidated, so the recovery batch sees the same
        // undrained range its invalidated predecessor was working from.
        let value: i64 = tx.query_row(
            "SELECT COALESCE(MAX(safe_input_index) + 1, 0) FROM valid_sequenced_l2_txs \
             WHERE safe_input_index IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        i64_to_u64(value)
    };
    let safe_input_end = query_latest_safe_input_index_exclusive(tx)?;
    let leading_range = super::SafeInputRange::new(next_undrained, safe_input_end);
    persist_frame_direct_sequence(tx, next_bi, 0, leading_range)?;
    Ok(())
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
