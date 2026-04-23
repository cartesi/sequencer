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
/// Callers pattern-match on the variant to decide what action the condition
/// warrants. The runtime danger detector treats Strict and Stalled the same
/// (both trigger a crash-for-recovery); the startup recovery path distinguishes
/// because the two variants imply different responses (fresh-L1
/// flush-and-cascade vs stalled-L1 refuse-boot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerStatus {
    /// No danger detected — neither check tripped.
    Safe,
    /// Strict, block-based check tripped: a closed batch past the accepted
    /// frontier is aged beyond `protocol.danger_threshold()` against the
    /// observed safe block. L1 view is fresh; flushing and cascading is
    /// meaningful.
    Strict(u64),
    /// Wall-clock-adjusted check tripped: an unresolved batch is estimated
    /// past the adjusted threshold because wall-clock time has elapsed past
    /// our last safe-head observation. The safe-head view may be stale or
    /// frozen — flushing against L1 may not terminate.
    Stalled(u64),
}

/// Wall-clock-adjusted danger threshold, if a correction applies.
///
/// Returns `None` when either:
/// - `last_safe_progress_ms == 0` (no baseline — correction is undefined).
/// - Elapsed wall-clock hasn't reached at least one block interval yet (no
///   correction needed).
///
/// Returns `Some(adjusted_threshold)` where
/// `adjusted = danger_threshold - (elapsed_secs / seconds_per_block)`,
/// saturating at 0. The caller picks which DB-view query to run against this
/// threshold.
pub(super) fn wall_clock_adjusted_threshold(
    last_safe_progress_ms: u64,
    now_ms: u64,
    protocol: &ProtocolConfig,
) -> Option<u64> {
    if last_safe_progress_ms == 0 {
        return None;
    }
    let elapsed_secs = now_ms.saturating_sub(last_safe_progress_ms) / 1000;
    let missed = elapsed_secs / protocol.seconds_per_block.max(1);
    if missed == 0 {
        return None;
    }
    Some(protocol.danger_threshold().saturating_sub(missed))
}

impl Storage {
    /// Unified danger-zone detection.
    ///
    /// Runs two checks inside a single read transaction:
    ///
    /// 1. **Strict (block-based)**: `find_closed_frontier_batch_in_danger`
    ///    against `protocol.danger_threshold()`. Uses the observed safe block.
    /// 2. **Wall-clock adjusted**: if a correction applies
    ///    ([`wall_clock_adjusted_threshold`] returns `Some`), widens to
    ///    `find_first_batch_in_danger` against `danger_threshold − missed_blocks`.
    ///
    /// Returns [`DangerStatus::Strict`] if (1) fires (stronger statement about
    /// fresh data takes priority), [`DangerStatus::Stalled`] if only (2) fires,
    /// [`DangerStatus::Safe`] otherwise.
    ///
    /// `now_ms` is passed in (rather than read from `SystemTime::now()` here)
    /// so the storage layer stays testable without time mocking. Production
    /// callers pass the current Unix-ms clock.
    pub fn check_danger(&mut self, protocol: &ProtocolConfig, now_ms: u64) -> Result<DangerStatus> {
        self.read(|tx| {
            if let Some(idx) =
                find_closed_frontier_batch_in_danger(tx, protocol.danger_threshold())?
            {
                return Ok(DangerStatus::Strict(idx));
            }

            let last_safe_progress_ms: i64 = tx.query_row(
                "SELECT synced_at_ms FROM l1_safe_head WHERE singleton_id = 0",
                [],
                |row| row.get(0),
            )?;
            let last_safe_progress_ms = i64_to_u64(last_safe_progress_ms);

            if let Some(adjusted) =
                wall_clock_adjusted_threshold(last_safe_progress_ms, now_ms, protocol)
                && let Some(idx) = find_first_batch_in_danger(tx, adjusted)?
            {
                return Ok(DangerStatus::Stalled(idx));
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
    /// through [`Storage::detect_and_recover`].
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

    /// Detect stale batches, cascade-invalidate, and restore the open-batch
    /// invariant. Called once per boot and by direct tests.
    ///
    /// Runs detection, cascade invalidation, and recovery-batch opening inside
    /// a single `Immediate` transaction so the operation is crash-safe and
    /// atomic.
    ///
    /// Handles the edge case where a previous boot invalidated the suffix but
    /// crashed before opening the fresh batch: if no new invalidations are
    /// found but no valid open batch exists, a recovery batch is opened.
    ///
    /// Does NOT populate `safe_accepted_batches` — the caller is expected to
    /// have already synced L1 state via [`Storage::append_safe_inputs`], which
    /// maintains the frontier view atomically with each sync.
    ///
    /// Returns the newly invalidated batch indices (empty if none).
    pub fn detect_and_recover(&mut self, max_wait_blocks: u64) -> Result<Vec<u64>> {
        self.write(|tx| detect_and_recover_inner(tx, max_wait_blocks))
    }
}

// ── Free functions used by both recovery and the batch submitter ──────────

/// Detect stale batches, cascade-invalidate, and restore the open-batch invariant.
/// See `Storage::detect_and_recover` for full doc.
fn detect_and_recover_inner(tx: &Transaction<'_>, max_wait_blocks: u64) -> Result<Vec<u64>> {
    let invalidated = match find_first_batch_in_danger(tx, max_wait_blocks)? {
        Some(bi) => cascade_invalidate_from(tx, bi)?,
        None => Vec::new(),
    };

    if !invalidated.is_empty() || !has_valid_open_batch(tx)? {
        open_recovery_batch_in_tx(tx)?;
    }
    Ok(invalidated)
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
/// Used by:
///   - [`Storage::check_danger`]'s wall-clock-adjusted arm.
///   - [`detect_and_recover_inner`] — atomic cascade-invalidation path.
///
/// Keeping both call sites behind this single helper keeps the "any unresolved
/// batch may already be too old" logic symmetric between the startup fallback
/// and the recovery transaction.
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
