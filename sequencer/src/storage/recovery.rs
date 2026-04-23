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
use sequencer_core::protocol::{ProtocolConfig, age_exceeds};

use super::Storage;
use super::internals::{
    i64_to_u64, insert_new_batch, insert_open_frame, now_unix_ms, persist_frame_direct_sequence,
    query_batch_policy, query_current_safe_block, query_latest_safe_input_index_exclusive,
    u64_to_i64,
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;

        if let Some(idx) = find_closed_frontier_batch_in_danger(&tx, protocol.danger_threshold())? {
            tx.commit()?;
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
            && let Some(idx) = find_first_batch_in_danger(&tx, adjusted)?
        {
            tx.commit()?;
            return Ok(DangerStatus::Stalled(idx));
        }

        tx.commit()?;
        Ok(DangerStatus::Safe)
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
    /// through [`Storage::detect_and_recover`] / [`Storage::run_startup_recovery`].
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let to_invalidate = detect_and_recover_inner(&tx, max_wait_blocks)?;
        tx.commit()?;
        Ok(to_invalidate)
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
mod tests {
    use super::super::test_helpers::{
        SENDER_A, default_protocol_config, load_all_ordered_l2_txs, make_stale_batch_payload,
        seed_closed_batches, temp_db,
    };
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
    use alloy_primitives::Address;
    use sequencer_core::l2_tx::SequencedL2Tx;

    mod invalid_batches {
        use super::*;

        // ── invalid_batches filtering ──────────────────────────────────────

        #[test]
        fn invalid_batches_excluded_from_latest_batch_index() {
            let db = temp_db("invalid-latest-batch");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            seed_closed_batches(&mut storage, 3);
            assert_eq!(
                storage.latest_batch_index().expect("latest").unwrap(),
                3,
                "open batch should be 3"
            );

            storage.insert_invalid_batch(3).expect("mark invalid");
            assert_eq!(storage.latest_batch_index().expect("latest").unwrap(), 2,);

            storage.insert_invalid_batch(2).expect("mark invalid");
            assert_eq!(storage.latest_batch_index().expect("latest").unwrap(), 1,);
        }

        #[test]
        fn invalid_batches_excluded_from_ordered_l2_txs() {
            let db = temp_db("invalid-ordered-txs");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            let directs_0 = vec![StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }];
            storage
                .append_safe_inputs(10, directs_0.as_slice(), &default_protocol_config())
                .expect("append");
            storage
                .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
                .expect("close frame");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            let directs_1 = vec![StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 20,
            }];
            storage
                .append_safe_inputs(20, directs_1.as_slice(), &default_protocol_config())
                .expect("append");
            storage
                .close_frame_only(&mut head, 20, SafeInputRange::new(1, 2))
                .expect("close frame");

            let all = load_all_ordered_l2_txs(&mut storage);
            assert_eq!(all.len(), 2);

            storage.insert_invalid_batch(0).expect("mark invalid");

            let filtered = load_all_ordered_l2_txs(&mut storage);
            assert_eq!(filtered.len(), 1);
            match &filtered[0] {
                SequencedL2Tx::Direct(d) => assert_eq!(d.payload.as_slice(), &[0xbb]),
                _ => panic!("expected direct input"),
            }
        }

        #[test]
        fn invalid_batches_excluded_from_ordered_l2_txs_for_batch() {
            let db = temp_db("invalid-ordered-for-batch");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            let directs = vec![StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }];
            storage
                .append_safe_inputs(10, directs.as_slice(), &default_protocol_config())
                .expect("append");
            storage
                .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
                .expect("close frame");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            let txs = storage
                .load_ordered_l2_txs_for_batch(0)
                .expect("load batch 0");
            assert_eq!(txs.len(), 1);

            storage.insert_invalid_batch(0).expect("mark invalid");
            let txs = storage
                .load_ordered_l2_txs_for_batch(0)
                .expect("load batch 0 after invalidation");
            assert!(txs.is_empty(), "invalid batch should return no txs");
        }

        #[test]
        fn invalid_batches_excluded_from_drained_direct_count() {
            let db = temp_db("invalid-drained-count");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            let directs = vec![
                StoredSafeInput {
                    sender: Address::ZERO,
                    payload: vec![0xaa],
                    block_number: 10,
                },
                StoredSafeInput {
                    sender: Address::ZERO,
                    payload: vec![0xbb],
                    block_number: 10,
                },
            ];
            storage
                .append_safe_inputs(10, directs.as_slice(), &default_protocol_config())
                .expect("append");
            storage
                .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
                .expect("close frame");
            assert_eq!(
                storage
                    .load_next_undrained_safe_input_index()
                    .expect("cursor"),
                2
            );

            storage.insert_invalid_batch(0).expect("mark invalid");
            assert_eq!(
                storage
                    .load_next_undrained_safe_input_index()
                    .expect("cursor after invalidation"),
                0
            );
        }
    }

    mod detect_and_recover {
        use super::*;

        // ── detect_and_recover ─────────────────────────────────────────────

        #[test]
        fn detect_and_recover_cascades_from_stale() {
            let db = temp_db("detect-stale");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            for _ in 0..3 {
                storage
                    .close_frame_and_batch(&mut head, 10)
                    .expect("close batch");
            }

            let batch_submitter = Address::repeat_byte(0xAA);
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append safe input");
            let invalidated = storage
                .detect_and_recover(1200)
                .expect("detect and recover");
            assert_eq!(invalidated, vec![0, 1, 2, 3]);

            let head = storage.load_open_state().expect("load open state");
            assert!(head.is_some(), "recovery should have opened a fresh batch");
            assert_eq!(head.unwrap().batch_index, 4);
        }

        #[test]
        fn detect_and_recover_is_idempotent() {
            let db = temp_db("detect-idempotent");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch");

            let batch_submitter = Address::repeat_byte(0xAA);
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append safe input");
            let first = storage.detect_and_recover(1200).expect("first detect");
            assert_eq!(first, vec![0, 1]);

            let second = storage.detect_and_recover(1200).expect("second detect");
            assert!(second.is_empty());
        }

        #[test]
        fn detect_and_recover_does_not_false_match_after_nonce_reuse() {
            let db = temp_db("detect-nonce-reuse");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            let batch_submitter = Address::repeat_byte(0xAA);
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append stale safe input");
            let first = storage.detect_and_recover(1200).expect("first recovery");
            assert_eq!(first, vec![0, 1]);

            let mut head = storage.load_open_state().expect("load").unwrap();
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close recovery batch");

            let second = storage.detect_and_recover(1200).expect("second recovery");
            assert!(
                second.is_empty(),
                "old stale row must not false-match new-generation batch with reused nonce"
            );
        }

        #[test]
        fn detect_and_recover_detects_stale_reused_nonce_in_new_generation() {
            let db = temp_db("detect-reused-stale");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            let batch_submitter = Address::repeat_byte(0xAA);
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append gen1 stale safe input");
            let first = storage.detect_and_recover(1200).expect("gen1 recovery");
            assert_eq!(first, vec![0, 1]);

            let mut head = storage.load_open_state().expect("load").unwrap();
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close gen2 batch");

            storage
                .append_safe_inputs(
                    2410,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 100),
                        block_number: 2410,
                    }],
                    &default_protocol_config(),
                )
                .expect("append gen2 stale safe input");
            let second = storage.detect_and_recover(1200).expect("gen2 recovery");
            assert_eq!(
                second,
                vec![2, 3],
                "stale reused nonce in gen2 must still be detected"
            );
        }
    }

    mod tip_staleness {
        use super::*;

        // ── §7.3 — Tip staleness regression ───────────────────────────────────
        //
        // Original bug: a Tip (unsealed) whose first frame was pinned to an old
        // safe_block escaped detection. The frontier lookup only considered
        // closed batches, leaving the Tip out of scope.
        //
        // Fix: `find_first_batch_in_danger` first tries the closed-frontier
        // check, then falls through to `find_tip_batch_in_danger`. Both the
        // preemptive danger check and the reactive cascade path go through this
        // helper, so they can never diverge on what counts as "in danger".
        //
        // Below covers four cases:
        //   - positive: Tip IS stale → invalidated
        //   - negative: Tip is fresh → NOT invalidated (no false positives)
        //   - combined: closed+stale AND tip+stale → both invalidated in one cascade
        //   - no-batch: empty DB with no Tip → no-op, no panic

        #[test]
        fn open_batch_stale_by_current_safe_block_is_invalidated() {
            // Scenario: sequencer opened batch 0 at safe_block=10, never closed it,
            // then stayed down until safe advanced to 1500 (>1200 past safe_block).
            // Recovery must invalidate the open batch.
            let db = temp_db("open-batch-stale");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize open state at safe_block=10");

            // Advance the safe head so the open batch's first frame (safe_block=10)
            // is now stale: 1500 - 10 >= 1200.
            storage
                .append_safe_inputs(1500, &[], &default_protocol_config())
                .expect("advance safe head past MAX_WAIT_BLOCKS");

            let invalidated = storage
                .detect_and_recover(1200)
                .expect("recover from stale open batch");
            assert_eq!(
                invalidated,
                vec![0],
                "open batch 0 should be invalidated by current staleness"
            );

            // A fresh recovery batch must be opened at batch_index=1.
            let head = storage.load_open_state().expect("load").expect("head");
            assert_eq!(head.batch_index, 1, "recovery batch is the next index");
        }

        #[test]
        fn open_batch_not_yet_stale_is_not_invalidated() {
            // Negative: open batch's first frame safe_block=10 with current safe=1100.
            // 1100 - 10 = 1090 < 1200. Must NOT cascade.
            // Catches false-positive regressions in the open-batch arm of
            // `find_first_batch_in_danger`.
            let db = temp_db("open-batch-fresh");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize open state at safe_block=10");

            storage
                .append_safe_inputs(1100, &[], &default_protocol_config())
                .expect("advance safe head below threshold");

            let invalidated = storage
                .detect_and_recover(1200)
                .expect("recover with non-stale open batch");
            assert!(
                invalidated.is_empty(),
                "fresh open batch must not be cascade-invalidated, got: {invalidated:?}"
            );

            // The open batch must still be the live one (no recovery batch opened).
            let head = storage.load_open_state().expect("load").expect("head");
            assert_eq!(
                head.batch_index, 0,
                "original open batch 0 must still be the head"
            );
        }

        #[test]
        fn open_batch_exactly_at_threshold_is_invalidated() {
            // Boundary: 1210 - 10 = 1200, which is >= MAX_WAIT_BLOCKS.
            // The staleness comparison is `>=`, so this must invalidate.
            let db = temp_db("open-batch-boundary");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");

            storage
                .append_safe_inputs(1210, &[], &default_protocol_config())
                .expect("advance safe head to exact threshold");

            let invalidated = storage.detect_and_recover(1200).expect("recover");
            assert_eq!(invalidated, vec![0], "boundary (>= threshold) invalidates");
        }

        #[test]
        fn open_batch_one_block_below_threshold_is_not_invalidated() {
            // Boundary: 1209 - 10 = 1199 < 1200. One-block margin must NOT invalidate.
            let db = temp_db("open-batch-below-boundary");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");

            storage
                .append_safe_inputs(1209, &[], &default_protocol_config())
                .expect("advance safe head to one block below threshold");

            let invalidated = storage.detect_and_recover(1200).expect("recover");
            assert!(
                invalidated.is_empty(),
                "one-block-below-threshold must not invalidate, got: {invalidated:?}"
            );
        }

        #[test]
        fn closed_unsubmitted_stale_and_open_stale_both_cascade() {
            // Scenario: batch 0 is closed and nonced but never submitted to L1
            // (safe_accepted_batches is empty). Batch 1 is open and also stale.
            // `find_first_batch_in_danger` should return closed batch 0 at the
            // frontier (nonce 0, no acceptance yet) and cascade through batch 1.
            let db = temp_db("closed-unsubmitted-and-open-stale");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize at safe_block=10");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            // Advance safe head so batch 0's first frame (safe_block=10) is stale.
            storage
                .append_safe_inputs(1500, &[], &default_protocol_config())
                .expect("advance safe head past staleness");

            let invalidated = storage.detect_and_recover(1200).expect("recover");
            assert_eq!(
                invalidated,
                vec![0, 1],
                "closed unsubmitted batch 0 and subsequent open batch 1 cascade together"
            );
        }

        #[test]
        fn detect_and_recover_opens_batch_after_torn_invalidation() {
            let db = temp_db("detect-torn");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            storage.insert_invalid_batch(0).expect("invalidate 0");
            storage.insert_invalid_batch(1).expect("invalidate 1");

            let invalidated = storage
                .detect_and_recover(1200)
                .expect("recover from torn state");
            assert!(invalidated.is_empty(), "no new invalidations");

            let head = storage.load_open_state().expect("load open state");
            assert!(head.is_some(), "recovery should have opened a fresh batch");
            assert_eq!(head.unwrap().batch_index, 2);
        }

        #[test]
        fn detect_and_recover_rolls_back_when_cascade_update_aborts() {
            let db = temp_db("detect-cascade-abort");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            // Advance safe head so batch 0's first frame (safe_block=10) is stale.
            storage
                .append_safe_inputs(1500, &[], &default_protocol_config())
                .expect("advance safe head past staleness");

            storage
                .conn
                .execute_batch(
                    "CREATE TRIGGER fail_cascade_invalidation
                     AFTER UPDATE OF invalidated_at_ms ON batches
                     WHEN NEW.invalidated_at_ms IS NOT NULL
                      AND OLD.invalidated_at_ms IS NULL
                     BEGIN
                         SELECT RAISE(ABORT, 'injected cascade failure');
                     END;",
                )
                .expect("install failure trigger");

            let err = storage
                .detect_and_recover(1200)
                .expect_err("trigger should abort recovery transaction");
            assert!(
                err.to_string().contains("injected cascade failure"),
                "unexpected error: {err:?}"
            );
            drop(storage);

            let conn =
                Storage::open_connection(db.path.as_str(), "NORMAL").expect("open read conn");
            let invalidated_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM batches WHERE invalidated_at_ms IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .expect("count invalidated");
            assert_eq!(
                invalidated_count, 0,
                "failed cascade must not persist torn invalidation state"
            );

            let batch_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM batches", [], |row| row.get(0))
                .expect("count batches");
            assert_eq!(
                batch_count, 2,
                "failed recovery must not open an extra batch"
            );

            let open_batch_index: i64 = conn
                .query_row("SELECT batch_index FROM valid_open_batch", [], |row| {
                    row.get(0)
                })
                .expect("query valid open batch");
            assert_eq!(
                open_batch_index, 1,
                "failed recovery must leave the original Tip in place"
            );
        }

        #[test]
        fn recovery_redrains_direct_inputs_and_replay_sees_them_once() {
            let db = temp_db("recovery-redrain-e2e");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            let deposits = vec![
                StoredSafeInput {
                    sender: Address::ZERO,
                    payload: vec![0xd1],
                    block_number: 10,
                },
                StoredSafeInput {
                    sender: Address::ZERO,
                    payload: vec![0xd2],
                    block_number: 10,
                },
            ];
            storage
                .append_safe_inputs(10, deposits.as_slice(), &default_protocol_config())
                .expect("append deposits");
            storage
                .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
                .expect("close frame with deposits");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            let before = load_all_ordered_l2_txs(&mut storage);
            assert_eq!(before.len(), 2, "both deposits should be visible");

            let batch_submitter = Address::repeat_byte(0xAA);
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append stale batch submission");
            let invalidated = storage
                .detect_and_recover(1200)
                .expect("detect and recover");
            assert!(!invalidated.is_empty(), "should have invalidated batches");

            let after = load_all_ordered_l2_txs(&mut storage);
            let direct_payloads: Vec<&[u8]> = after
                .iter()
                .filter_map(|tx| match tx {
                    SequencedL2Tx::Direct(d) if d.sender != batch_submitter => {
                        Some(d.payload.as_slice())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                direct_payloads,
                vec![&[0xd1][..], &[0xd2][..]],
                "deposits must appear exactly once in replay after recovery"
            );

            let recovery_batch = storage.load_open_state().expect("load").unwrap();
            let recovery_txs = storage
                .load_ordered_l2_txs_for_batch(recovery_batch.batch_index)
                .expect("load recovery batch txs");
            let recovery_direct_count = recovery_txs
                .iter()
                .filter(|tx| matches!(tx, SequencedL2Tx::Direct(d) if d.sender != batch_submitter))
                .count();
            assert_eq!(
                recovery_direct_count, 2,
                "both deposits should be in the recovery batch"
            );
        }

        #[test]
        fn undrained_safe_input_appears_in_recovery_batch_first_frame() {
            // §7.4.2: a deposit ingested into safe_inputs but not yet drained
            // into any frame must be sequenced into the recovery batch's first
            // frame after cascade. Complements §7.4.1 (re-drain from
            // invalidated) with the never-drained case.
            let db = temp_db("recovery-includes-undrained");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0 with no deposits");

            let non_submitter = Address::repeat_byte(0xCC);
            storage
                .append_safe_inputs(
                    20,
                    &[StoredSafeInput {
                        sender: non_submitter,
                        payload: vec![0xde, 0xad],
                        block_number: 20,
                    }],
                    &default_protocol_config(),
                )
                .expect("append undrained deposit");
            let before = load_all_ordered_l2_txs(&mut storage);
            assert!(
                before.iter().all(|tx| !matches!(
                    tx,
                    SequencedL2Tx::Direct(d) if d.sender == non_submitter
                )),
                "undrained deposit must not be sequenced before drain",
            );

            let batch_submitter = SENDER_A;
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append stale batch submission");
            let invalidated = storage.detect_and_recover(1200).expect("recover");
            assert!(!invalidated.is_empty(), "stale batch must cascade");

            let recovery = storage.load_open_state().expect("load").unwrap();
            let recovery_txs = storage
                .load_ordered_l2_txs_for_batch(recovery.batch_index)
                .expect("load recovery batch txs");
            let deposit_payloads: Vec<&[u8]> = recovery_txs
                .iter()
                .filter_map(|tx| match tx {
                    SequencedL2Tx::Direct(d) if d.sender == non_submitter => {
                        Some(d.payload.as_slice())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                deposit_payloads,
                vec![&[0xde, 0xad][..]],
                "undrained deposit must land in the recovery batch's first frame",
            );
        }

        #[test]
        fn recovery_batch_opens_empty_when_no_direct_inputs_pending() {
            // §7.4.3: no drained-into-invalidated inputs AND no undrained safe
            // inputs → recovery batch opens with an empty first frame (aside
            // from the batch-submitter's own self-submission, which is drained
            // but carries no user-visible payload).
            let db = temp_db("recovery-empty-first-frame");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            let batch_submitter = SENDER_A;
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append stale batch submission");
            let invalidated = storage.detect_and_recover(1200).expect("recover");
            assert_eq!(invalidated, vec![0, 1]);

            let recovery = storage.load_open_state().expect("load").unwrap();
            let recovery_txs = storage
                .load_ordered_l2_txs_for_batch(recovery.batch_index)
                .expect("load recovery batch txs");
            let user_visible: Vec<_> = recovery_txs
                .iter()
                .filter(|tx| match tx {
                    SequencedL2Tx::Direct(d) => d.sender != batch_submitter,
                    SequencedL2Tx::UserOp(_) => true,
                })
                .collect();
            assert!(
                user_visible.is_empty(),
                "recovery batch must have no deposits or user-ops when none were pending: {user_visible:?}",
            );
        }

        #[test]
        fn first_batch_stale_recovery_reuses_nonce_zero() {
            // §7.5.1: first-ever batch (nonce 0) goes stale before reaching
            // Gold. Cascade invalidates it; recovery opens a fresh batch that
            // reuses nonce 0 (no valid ancestor exists to advance the nonce).
            let db = temp_db("first-batch-stale-nonce-zero");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0 (nonce 0)");

            let batch_submitter = SENDER_A;
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append stale batch submission");
            let invalidated = storage.detect_and_recover(1200).expect("recover");
            assert_eq!(
                invalidated,
                vec![0, 1],
                "closed batch 0 and open batch 1 must both invalidate",
            );

            let recovery = storage.load_open_state().expect("load").unwrap();
            assert_eq!(recovery.batch_index, 2, "batch_index is monotonic (PK)");
            drop(storage);

            // Read the new Tip's nonce and parent pointer via raw SQL — no
            // public accessor surfaces them.
            let conn =
                Storage::open_connection(db.path.as_str(), "NORMAL").expect("open read conn");
            let recovery_i64 = recovery.batch_index as i64;
            let nonce: i64 = conn
                .query_row(
                    "SELECT nonce FROM batches WHERE batch_index = ?1",
                    [recovery_i64],
                    |row| row.get(0),
                )
                .expect("query nonce");
            assert_eq!(
                nonce, 0,
                "recovery batch must reuse nonce 0 after torn cascade",
            );
            let parent: Option<i64> = conn
                .query_row(
                    "SELECT parent_batch_index FROM batches WHERE batch_index = ?1",
                    [recovery_i64],
                    |row| row.get(0),
                )
                .expect("query parent");
            assert_eq!(
                parent, None,
                "torn recovery has no valid ancestor; parent_batch_index is NULL",
            );
        }

        #[test]
        fn detect_and_recover_after_post_recovery_crash_is_no_op() {
            // §7.6.3: simulate a crash AFTER open_recovery_batch has run. On
            // restart, the state contains a valid open recovery batch (no stale
            // tail remains). A fresh `detect_and_recover` call must be a no-op:
            // no new invalidations, and the same recovery batch remains the Tip.
            //
            // Distinct from §7.6.1 (idempotent back-to-back call on the same
            // Storage handle) — this test drops and reopens Storage to model a
            // full restart over the persisted DB.
            let db = temp_db("post-recovery-crash-idempotent");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");

            let batch_submitter = SENDER_A;
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append stale submission");
            // First call: full recovery runs to completion and opens a new Tip.
            let invalidated = storage.detect_and_recover(1200).expect("recover");
            assert_eq!(invalidated, vec![0, 1]);
            let recovery_index = storage
                .load_open_state()
                .expect("load open")
                .expect("recovery batch exists")
                .batch_index;

            // Simulate "crash immediately after open_recovery_batch" by
            // dropping Storage (mimics process exit) and reopening against the
            // same on-disk DB.
            drop(storage);
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("reopen storage");

            let second = storage.detect_and_recover(1200).expect("second detect");
            assert!(
                second.is_empty(),
                "post-recovery restart must be a no-op, got invalidations: {second:?}",
            );
            let after = storage
                .load_open_state()
                .expect("load after restart")
                .expect("recovery batch still Tip after restart");
            assert_eq!(
                after.batch_index, recovery_index,
                "the same recovery batch must remain the Tip after restart",
            );
        }
    }

    mod check_danger_zone {
        use super::*;

        // ── check_danger_zone ──────────────────────────────────────────────

        #[test]
        fn check_danger_zone_ignores_old_gold_batches() {
            // Batch 0 is Gold (accepted, first_frame_safe_block=10). Batch 1 is
            // the open tip at first_frame_safe_block=100. Advance safe head to
            // 1200 so batch 0 is age=1190 > 1125 (past threshold, but it's Gold
            // and therefore excluded) while batch 1 is age=1100 < 1125 (fresh).
            //
            // `check_danger_zone` must return None: no unresolved batch is in
            // danger. Gold batches (accepted past the frontier) never participate,
            // and the open tip isn't old enough to trip the threshold.
            let db = temp_db("danger-zone-gold");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let batch_submitter = Address::repeat_byte(0xAA);

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close batch 0");

            storage
                .append_safe_inputs(
                    20,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 20,
                    }],
                    &default_protocol_config(),
                )
                .expect("append safe input");
            // Advance to a current safe block where batch 0 (safe_block=10) is
            // past threshold (1200-10=1190>=1125) but batch 1 (safe_block=100)
            // is still fresh (1200-100=1100<1125).
            storage
                .append_safe_inputs(1200, &[], &default_protocol_config())
                .expect("advance safe block");

            let result = storage.check_danger_zone(1125).expect("check danger zone");
            assert!(
                result.is_none(),
                "old Gold batches should not trigger danger zone; got batch_index={result:?}"
            );
        }

        #[test]
        fn check_danger_zone_does_not_flag_open_batch_zombie() {
            // `check_danger_zone` is for zombie detection: it must NOT flag the
            // open batch (which has no L1 tx to become a zombie). Flagging open
            // batches here would put the live submitter into a shutdown/restart
            // loop when an open batch ages into the danger zone without any
            // pending wallet-nonce slots to flush.
            //
            // Scenario: only an open batch exists, aged past the danger
            // threshold. `check_danger_zone` returns None.
            let db = temp_db("danger-zone-open-no-zombie");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize open batch at safe_block=10");

            storage
                .append_safe_inputs(1200, &[], &default_protocol_config())
                .expect("advance safe head past danger threshold");

            let result = storage.check_danger_zone(1125).expect("check danger zone");
            assert!(
                result.is_none(),
                "open batch (no zombie) must not trigger check_danger_zone; got batch_index={result:?}"
            );
        }
    }

    mod check_any_unresolved {
        use super::*;

        // ── check_any_unresolved_batch_in_danger ───────────────────────────────

        #[test]
        fn check_any_unresolved_flags_stale_open_batch() {
            // Wall-clock fallback regression: `check_any_unresolved_batch_in_danger`
            // MUST flag a stale open batch. This is the semantic the wall-clock
            // fallback relies on — if L1 is unreachable and an open batch may be
            // past the threshold, refuse to boot rather than accept user ops
            // into a batch that can't land.
            let db = temp_db("any-unresolved-open-stale");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize open batch at safe_block=10");

            storage
                .append_safe_inputs(1200, &[], &default_protocol_config())
                .expect("advance safe head past threshold");

            let result = storage
                .check_any_unresolved_batch_in_danger(1125)
                .expect("check any unresolved in danger");
            assert_eq!(
                result,
                Some(0),
                "stale open batch (batch 0) must be flagged by the unified check"
            );
        }

        #[test]
        fn check_any_unresolved_does_not_flag_fresh_open_batch() {
            // Negative counterpart. Fresh open batch below threshold must not
            // trigger false positives in the unified check.
            let db = temp_db("any-unresolved-open-fresh");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

            storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize open batch at safe_block=10");

            storage
                .append_safe_inputs(1100, &[], &default_protocol_config())
                .expect("advance safe head below threshold");

            let result = storage
                .check_any_unresolved_batch_in_danger(1125)
                .expect("check any unresolved in danger");
            assert!(
                result.is_none(),
                "fresh open batch must not trigger the unified check; got batch_index={result:?}"
            );
        }

        #[test]
        fn check_danger_zone_triggers_on_frontier_batch() {
            let db = temp_db("danger-zone-frontier");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let batch_submitter = Address::repeat_byte(0xAA);

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close batch 1");

            storage
                .append_safe_inputs(
                    20,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 20,
                    }],
                    &default_protocol_config(),
                )
                .expect("append safe input");
            storage
                .append_safe_inputs(1200, &[], &default_protocol_config())
                .expect("advance safe block");

            let result = storage.check_danger_zone(1125).expect("check danger zone");
            assert_eq!(result, Some(1), "frontier batch should trigger danger zone");
        }

        #[test]
        fn check_danger_zone_does_not_trigger_below_threshold() {
            let db = temp_db("danger-zone-below");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let batch_submitter = Address::repeat_byte(0xAA);

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch 0");
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close batch 1");

            storage
                .append_safe_inputs(
                    20,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 20,
                    }],
                    &default_protocol_config(),
                )
                .expect("append safe input");
            storage
                .append_safe_inputs(1134, &[], &default_protocol_config())
                .expect("advance safe block");

            let result = storage.check_danger_zone(1125).expect("check danger zone");
            assert!(
                result.is_none(),
                "should not trigger below threshold; got batch_index={result:?}"
            );
        }
    }

    mod boundary {
        use super::*;

        // ── boundary tests ─────────────────────────────────────────────────

        #[test]
        fn detect_and_recover_boundary_exactly_max_wait_is_stale() {
            let db = temp_db("detect-boundary-exact");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let max_wait: u64 = 1200;

            let mut head = storage
                .initialize_open_state(100, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close batch");

            storage
                .append_safe_inputs(
                    1300,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 100),
                        block_number: 1300,
                    }],
                    &default_protocol_config(),
                )
                .expect("append safe input");
            let invalidated = storage.detect_and_recover(max_wait).expect("detect");
            assert_eq!(invalidated, vec![0, 1], "exactly at max_wait must be stale");
            assert_eq!(
                storage
                    .load_open_state()
                    .expect("load")
                    .unwrap()
                    .batch_index,
                2
            );
        }

        #[test]
        fn detect_and_recover_boundary_one_below_max_wait_is_not_stale() {
            let db = temp_db("detect-boundary-one-below");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let max_wait: u64 = 1200;

            let mut head = storage
                .initialize_open_state(100, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close batch");

            storage
                .append_safe_inputs(
                    1299,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 100),
                        block_number: 1299,
                    }],
                    &default_protocol_config(),
                )
                .expect("append safe input");
            let invalidated = storage.detect_and_recover(max_wait).expect("detect");
            assert!(
                invalidated.is_empty(),
                "one below max_wait must not be stale"
            );
        }

        #[test]
        fn detect_and_recover_all_batches_invalidated_frontier_zero() {
            let db = temp_db("detect-frontier-zero");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let max_wait: u64 = 1200;

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            for _ in 0..3 {
                storage.close_frame_and_batch(&mut head, 10).expect("close");
            }

            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append");
            let inv = storage.detect_and_recover(max_wait).expect("detect");
            assert_eq!(inv, vec![0, 1, 2, 3]);
            assert!(storage.load_open_state().expect("open").is_some());
        }

        #[test]
        fn detect_and_recover_recovery_batch_itself_becomes_stale() {
            let db = temp_db("detect-recovery-stale");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let max_wait: u64 = 1200;

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage.close_frame_and_batch(&mut head, 10).expect("close");

            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append gen1");
            let inv1 = storage.detect_and_recover(max_wait).expect("recover gen1");
            assert_eq!(inv1, vec![0, 1]);

            let mut head2 = storage.load_open_state().expect("load").unwrap();
            storage
                .close_frame_and_batch(&mut head2, 1210)
                .expect("close gen2");

            storage
                .append_safe_inputs(
                    2410,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 1210),
                        block_number: 2410,
                    }],
                    &default_protocol_config(),
                )
                .expect("append gen2");
            let inv2 = storage.detect_and_recover(max_wait).expect("recover gen2");
            assert_eq!(inv2, vec![2, 3]);
            assert!(storage.load_open_state().expect("open").is_some());
        }

        #[test]
        fn detect_and_recover_multi_round_gen3_recovery() {
            let db = temp_db("detect-gen3");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let max_wait: u64 = 1200;

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("init");
            storage.close_frame_and_batch(&mut head, 10).expect("close");
            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append");
            storage.detect_and_recover(max_wait).expect("recover gen1");

            let mut head2 = storage.load_open_state().expect("load").unwrap();
            storage
                .close_frame_and_batch(&mut head2, 1210)
                .expect("close gen2");
            storage
                .append_safe_inputs(
                    2410,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 1210),
                        block_number: 2410,
                    }],
                    &default_protocol_config(),
                )
                .expect("append gen2");
            storage.detect_and_recover(max_wait).expect("recover gen2");

            let mut head3 = storage.load_open_state().expect("load").unwrap();
            storage
                .close_frame_and_batch(&mut head3, 2410)
                .expect("close gen3");
            storage
                .append_safe_inputs(
                    2420,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 2410),
                        block_number: 2420,
                    }],
                    &default_protocol_config(),
                )
                .expect("append gen3");
            let inv3 = storage.detect_and_recover(max_wait).expect("recover gen3");
            assert!(inv3.is_empty(), "gen3 should be healthy");
        }

        #[test]
        fn detect_and_recover_large_cascade_50_batches() {
            let db = temp_db("detect-large-cascade");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let max_wait: u64 = 1200;

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            for _ in 0..50 {
                storage.close_frame_and_batch(&mut head, 10).expect("close");
            }

            storage
                .append_safe_inputs(
                    1210,
                    &[StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 1210,
                    }],
                    &default_protocol_config(),
                )
                .expect("append");
            let inv = storage.detect_and_recover(max_wait).expect("detect");
            assert_eq!(inv.len(), 51);
        }
    }

    mod schema_invariants {
        use super::*;
        use rusqlite::params;

        // ── Schema-invariant regression tests ─────────────────────────────────
        //
        // These exercise the triggers + partial unique index in the schema
        // directly. Each one checks a specific invariant that previously lived
        // in writer discipline and now has a schema-level tripwire.
        //
        // They're here (rather than in a dedicated file) because they share the
        // recovery tests' setup: same helpers, same fixture. Failures here mean
        // the schema guard regressed, which is the whole point of making the
        // invariants declarative.

        #[test]
        fn schema_rejects_second_valid_tip() {
            // The partial unique index `ux_single_valid_tip` catches a writer that
            // opens a new Tip without sealing the old one first.
            let db = temp_db("schema-second-tip");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");

            // Try to bypass the lane and insert a second valid Tip directly.
            let err = storage.conn.execute(
                "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms) \
             VALUES (99, 0, 1, 1000)",
                [],
            );
            let msg = format!("{err:?}");
            assert!(
                msg.contains("UNIQUE constraint failed") && msg.contains("ux_single_valid_tip"),
                "expected ux_single_valid_tip violation, got: {msg}"
            );
        }

        #[test]
        fn schema_rejects_bad_nonce_contiguity() {
            // Nonce must equal parent.nonce + 1 — trigger enforces it.
            // Insert the bad-nonce batch as already-sealed so it doesn't collide
            // with the existing Tip on `ux_single_valid_tip`.
            let db = temp_db("schema-bad-nonce");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let mut head = storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 0)
                .expect("close batch 0; batch 1 is now Tip");
            // Batch 1 has nonce 1 (0 + 1). Insert child with nonce 99 (should be 2).
            let err = storage.conn.execute(
            "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms, sealed_at_ms) \
             VALUES (999, 1, 99, \
                     (SELECT created_at_ms FROM batches WHERE batch_index = 1), \
                     (SELECT created_at_ms FROM batches WHERE batch_index = 1))",
            [],
        );
            assert!(
                format!("{err:?}").contains("batch nonce must equal parent.nonce + 1"),
                "expected nonce trigger, got: {err:?}"
            );
        }

        #[test]
        fn schema_rejects_genesis_with_nonzero_nonce() {
            let db = temp_db("schema-genesis-nonzero");
            let storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let err = storage.conn.execute(
                "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms) \
             VALUES (0, NULL, 7, 100)",
                [],
            );
            assert!(
                format!("{err:?}").contains("genesis batch must have nonce 0"),
                "expected genesis-nonce trigger, got: {err:?}"
            );
        }

        #[test]
        fn schema_rejects_re_seal() {
            let db = temp_db("schema-re-seal");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let mut head = storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 0)
                .expect("close batch 0 (seals it)");
            // Batch 0 is sealed. Attempt to re-seal with a different timestamp.
            let err = storage.conn.execute(
                "UPDATE batches SET sealed_at_ms = sealed_at_ms + 1 WHERE batch_index = 0",
                [],
            );
            assert!(
                format!("{err:?}").contains("sealed_at_ms is write-once"),
                "expected write-once trigger, got: {err:?}"
            );
        }

        #[test]
        fn schema_rejects_re_invalidate() {
            let db = temp_db("schema-re-invalidate");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            // Seed via test helper (uses now_unix_ms internally).
            storage.insert_invalid_batch(0).expect("first invalidate");
            let err = storage.conn.execute(
                "UPDATE batches SET invalidated_at_ms = invalidated_at_ms + 1 \
             WHERE batch_index = 0",
                [],
            );
            assert!(
                format!("{err:?}").contains("invalidated_at_ms is write-once"),
                "expected write-once trigger, got: {err:?}"
            );
        }

        #[test]
        fn schema_rejects_frame_insert_into_sealed_batch() {
            // This is the bug class we've been fighting: writer holds a stale
            // WriteHead and writes to a batch that's no longer the Tip.
            let db = temp_db("schema-frame-into-sealed");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let mut head = storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 0)
                .expect("close batch 0; batch 0 is now sealed");
            // Batch 0 is sealed. Any direct insert into its frames must fail.
            let err = storage.conn.execute(
                "INSERT INTO frames (batch_index, frame_in_batch, created_at_ms, fee, safe_block) \
             VALUES (0, 1, 100, 1060, 0)",
                [],
            );
            assert!(
                format!("{err:?}").contains("frames can only be inserted into the current Tip"),
                "expected tip-only-frames trigger, got: {err:?}"
            );
        }

        #[test]
        fn schema_rejects_frame_insert_into_invalidated_batch() {
            let db = temp_db("schema-frame-into-invalid");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            // Invalidate (without sealing) — Tip that never closed, now dead.
            storage.insert_invalid_batch(0).expect("invalidate tip");
            let err = storage.conn.execute(
                "INSERT INTO frames (batch_index, frame_in_batch, created_at_ms, fee, safe_block) \
             VALUES (0, 1, 100, 1060, 0)",
                [],
            );
            assert!(
                format!("{err:?}").contains("frames can only be inserted into the current Tip"),
                "expected tip-only-frames trigger, got: {err:?}"
            );
        }

        #[test]
        fn schema_rejects_parent_batch_index_mutation() {
            let db = temp_db("schema-parent-immutable");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let mut head = storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            storage
                .close_frame_and_batch(&mut head, 0)
                .expect("close batch 0");
            // Try to change parent of batch 1 — should be rejected.
            let err = storage.conn.execute(
                "UPDATE batches SET parent_batch_index = NULL WHERE batch_index = 1",
                [],
            );
            assert!(
                format!("{err:?}").contains("parent_batch_index is immutable"),
                "expected parent-immutable trigger, got: {err:?}"
            );
        }

        #[test]
        fn nonce_reuse_after_cascade_with_valid_ancestor() {
            // Beautiful part of parent-pointer + structural nonce: after a cascade
            // that invalidates only the suffix (keeping an ancestor valid), the
            // new Tip's parent is the last valid ancestor, so its nonce is
            // `ancestor.nonce + 1` — the same nonce the invalidated suffix's
            // first batch had. Nonce reuse is automatic.
            //
            // Scenario: batch 0 is accepted (safe_accepted_batches advances past
            // nonce 0). Batch 1 is stale and triggers cascade. Batches 1, 2, 3
            // invalidated; batch 0 remains valid.
            let db = temp_db("nonce-reuse-with-ancestor");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let batch_submitter = SENDER_A;

            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize at safe_block=10");
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close batch 0 (nonce 0)");
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close batch 1 (nonce 1)");
            storage
                .close_frame_and_batch(&mut head, 100)
                .expect("close batch 2 (nonce 2)");
            // Head is now batch 3 (nonce 3, first_frame_safe_block=100).

            // Batch 0 lands on L1 (accepted): safe_input at block 20 with nonce 0.
            storage
                .append_safe_inputs(
                    20,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 20,
                    }],
                    &default_protocol_config(),
                )
                .expect("append batch 0 submission");
            // Advance safe head so batches 1, 2, 3 (first_frame=100) are stale.
            // current_safe=1400 → 1400-100=1300 >= 1200.
            storage
                .append_safe_inputs(1400, &[], &default_protocol_config())
                .expect("advance past threshold");

            let inv = storage.detect_and_recover(1200).expect("recover");
            // Batches 1, 2, 3 invalidated; batch 0 (accepted) stays valid.
            assert_eq!(inv, vec![1, 2, 3], "only the suffix cascades, got {inv:?}");

            // The NEW Tip has parent=0 (the last valid ancestor), nonce=1.
            // This is what nonce reuse looks like: the invalidated batch 1 had
            // nonce 1; the recovery batch gets the same nonce via +1-from-parent.
            let (tip_nonce, tip_parent): (i64, i64) = storage
                .conn
                .query_row(
                    "SELECT nonce, parent_batch_index FROM valid_open_batch",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("query recovery tip");
            assert_eq!(tip_nonce, 1, "recovery Tip reuses nonce 1");
            assert_eq!(tip_parent, 0, "recovery Tip's parent is batch 0");
        }

        // ── §12.1.1 CHECK-constraint regressions ──────────────────────────
        //
        // These differ from the trigger-based tests above: they exercise raw
        // `CHECK` clauses declared in `migrations/0001_schema.sql`. The
        // type-safe `Storage` API would reject these values Rust-side; we go
        // through `storage.conn.execute` to prove the schema itself refuses.

        #[test]
        fn schema_rejects_safe_input_with_wrong_sender_length() {
            // §12.1.1: `safe_inputs.sender` must be exactly 20 bytes (an
            // Ethereum address). A shorter or longer blob must be refused
            // by the schema even if it bypasses the Rust API.
            let db = temp_db("schema-safe-input-sender-len");
            let storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let err = storage.conn.execute(
                "INSERT INTO safe_inputs (safe_input_index, sender, payload, block_number) \
                 VALUES (0, X'DEADBEEF', X'00', 10)",
                [],
            );
            assert!(
                format!("{err:?}").contains("CHECK constraint failed"),
                "expected CHECK constraint error on safe_inputs.sender, got: {err:?}",
            );
        }

        #[test]
        fn schema_rejects_user_op_with_wrong_sender_length() {
            // §12.1.1: `user_ops.sender` must be 20 bytes.
            let db = temp_db("schema-user-op-sender-len");
            let storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            // Seed a frame to satisfy the composite FK — initialize_open_state
            // creates batch 0 frame 0 as the Tip.
            let mut storage = storage;
            storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            let err = storage.conn.execute(
                "INSERT INTO user_ops \
                 (batch_index, frame_in_batch, pos_in_frame, sender, nonce, max_fee, data, sig, received_at_ms) \
                 VALUES (0, 0, 0, X'010203', 0, 0, X'', ?1, 0)",
                params![vec![0u8; 65]],
            );
            assert!(
                format!("{err:?}").contains("CHECK constraint failed"),
                "expected CHECK constraint error on user_ops.sender length, got: {err:?}",
            );
        }

        #[test]
        fn schema_rejects_user_op_with_wrong_signature_length() {
            // §12.1.1: `user_ops.sig` must be exactly 65 bytes (secp256k1
            // r || s || v). Regression for "accidentally accepted a non-65
            // signature and crashed a downstream consumer."
            let db = temp_db("schema-user-op-sig-len");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            let valid_sender = vec![0u8; 20];
            let short_sig = vec![0u8; 32]; // Should be 65.
            let err = storage.conn.execute(
                "INSERT INTO user_ops \
                 (batch_index, frame_in_batch, pos_in_frame, sender, nonce, max_fee, data, sig, received_at_ms) \
                 VALUES (0, 0, 0, ?1, 0, 0, X'', ?2, 0)",
                params![valid_sender, short_sig],
            );
            assert!(
                format!("{err:?}").contains("CHECK constraint failed"),
                "expected CHECK constraint error on user_ops.sig length, got: {err:?}",
            );
        }

        #[test]
        fn schema_rejects_sequenced_l2_tx_with_neither_xor_branch() {
            // §12.1.1: `sequenced_l2_txs` must be either a user-op row
            // (user_op_pos_in_frame IS NOT NULL) or a direct-input row
            // (safe_input_index IS NOT NULL), never both and never neither.
            // Setting both to NULL is the clean XOR violation to test —
            // FKs are only triggered on non-NULL values so we isolate the
            // CHECK constraint.
            let db = temp_db("schema-sequenced-l2-tx-xor-neither");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            storage
                .initialize_open_state(0, SafeInputRange::empty_at(0))
                .expect("initialize");
            let err = storage.conn.execute(
                "INSERT INTO sequenced_l2_txs \
                 (offset, batch_index, frame_in_batch, user_op_pos_in_frame, safe_input_index) \
                 VALUES (0, 0, 0, NULL, NULL)",
                [],
            );
            assert!(
                format!("{err:?}").contains("CHECK constraint failed"),
                "expected CHECK constraint error on sequenced_l2_txs XOR, got: {err:?}",
            );
        }

        #[test]
        fn schema_rejects_l1_bootstrap_cache_with_zero_chain_id() {
            // §12.1.1: `l1_bootstrap_cache.chain_id > 0`. chain_id = 0 would
            // collide with the EIP-712 domain's unspecified-chain sentinel
            // and break signature recovery; the CHECK refuses to persist it
            // in the first place.
            let db = temp_db("schema-bootstrap-chain-id-zero");
            let storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let input_box = vec![0u8; 20];
            let err = storage.conn.execute(
                "INSERT INTO l1_bootstrap_cache \
                 (singleton_id, input_box_address, genesis_block, chain_id) \
                 VALUES (0, ?1, 0, 0)",
                params![input_box],
            );
            assert!(
                format!("{err:?}").contains("CHECK constraint failed"),
                "expected CHECK constraint error on chain_id > 0, got: {err:?}",
            );
        }

        #[test]
        fn schema_rejects_safe_input_with_negative_block_number() {
            // §12.1.1: `safe_inputs.block_number >= 0`. Catches a regression
            // that would let a negative block number slip through — the rest
            // of the system assumes non-negative and could panic on cast.
            let db = temp_db("schema-safe-input-neg-block");
            let storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let sender = vec![0u8; 20];
            let err = storage.conn.execute(
                "INSERT INTO safe_inputs (safe_input_index, sender, payload, block_number) \
                 VALUES (0, ?1, X'00', -1)",
                params![sender],
            );
            assert!(
                format!("{err:?}").contains("CHECK constraint failed"),
                "expected CHECK constraint error on block_number >= 0, got: {err:?}",
            );
        }
    }

    mod tree_invariants {
        use super::*;

        // ── §12.5 Parent-pointer tree invariants ──────────────────────────────
        use crate::storage::internals::{i64_to_u64, u64_to_i64};
        use rusqlite::params;

        /// Check the tree invariants that should hold at every quiescent state:
        ///   - Every valid batch has `nonce = parent.nonce + 1`, or `nonce = 0`
        ///     with `parent_batch_index IS NULL` (genesis/post-torn-cascade).
        ///   - Every `parent_batch_index` either is NULL or references an
        ///     existing batch (FK handles this, but we assert explicitly).
        ///   - Walking up `parent_batch_index` from any valid batch terminates
        ///     at a NULL-parent row within `batch_index` hops (no cycles).
        ///   - The valid path is strictly contiguous in `nonce`: the set of
        ///     nonces among valid batches is `{0, 1, ..., max_valid_nonce}`.
        ///   - At most one `valid_open_batch` row exists.
        fn assert_tree_invariants(storage: &mut Storage) {
            // 1. Nonce = parent.nonce + 1 (or nonce=0 for NULL parent).
            let mut stmt = storage
                .conn
                .prepare(
                    "SELECT b.batch_index, b.parent_batch_index, b.nonce, p.nonce \
                 FROM batches b LEFT JOIN batches p ON p.batch_index = b.parent_batch_index",
                )
                .expect("prepare");
            let rows: Vec<(i64, Option<i64>, i64, Option<i64>)> = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("collect");
            drop(stmt);
            for (bi, parent, nonce, parent_nonce) in &rows {
                match (parent, parent_nonce) {
                    (None, _) => assert_eq!(
                        *nonce, 0,
                        "batch {bi}: NULL parent must have nonce 0, got {nonce}"
                    ),
                    (Some(_), None) => panic!("batch {bi}: parent exists but parent row missing"),
                    (Some(_), Some(pn)) => assert_eq!(
                        *nonce,
                        pn + 1,
                        "batch {bi}: nonce={nonce}, expected parent.nonce+1 = {}",
                        pn + 1
                    ),
                }
            }

            // 2. At most one valid open batch.
            let open_count: i64 = storage
                .conn
                .query_row("SELECT COUNT(*) FROM valid_open_batch", [], |row| {
                    row.get(0)
                })
                .expect("count open");
            assert!(open_count <= 1, "more than one valid Tip: {open_count}");

            // 3. Valid-path nonce contiguity: nonces on the valid chain are 0..N.
            let mut valid_nonces: Vec<i64> = storage
                .conn
                .prepare("SELECT nonce FROM valid_batches ORDER BY nonce ASC")
                .expect("prepare")
                .query_map([], |row| row.get::<_, i64>(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("collect");
            // There can be multiple valid batches with the SAME nonce only if
            // they live on different branches — but we don't allow that; valid
            // batches form a strict chain. So dedup-and-equal means contiguous.
            valid_nonces.sort();
            valid_nonces.dedup();
            for (i, &n) in valid_nonces.iter().enumerate() {
                assert_eq!(
                    n, i as i64,
                    "valid nonces not contiguous: got {valid_nonces:?}"
                );
            }

            // 4. Parent walk terminates at NULL in ≤ batch_index hops for every valid row.
            for (bi, _, _, _) in &rows {
                let mut cur: i64 = *bi;
                let bi_u = i64_to_u64(*bi);
                for _ in 0..=bi_u {
                    let parent: Option<i64> = storage
                        .conn
                        .query_row(
                            "SELECT parent_batch_index FROM batches WHERE batch_index = ?1",
                            params![cur],
                            |row| row.get(0),
                        )
                        .expect("parent lookup");
                    match parent {
                        None => break,
                        Some(p) => {
                            assert!(
                                p < cur,
                                "batch {bi}: parent-walk went backward ({p} >= {cur}) — cycle?"
                            );
                            cur = p;
                        }
                    }
                }
            }
        }

        #[test]
        fn tree_invariants_hold_across_mixed_workload() {
            // Exercises every mutating code path: genesis, rotations, partial
            // cascades (ancestor survives), cascades across accepted frontier,
            // torn cascades (no valid ancestor), and back-to-back generations.
            // Asserts tree invariants after each step.
            let db = temp_db("tree-invariants-workload");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let batch_submitter = SENDER_A;

            // Phase 1: genesis + 4 rotations. Simple chain.
            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            assert_tree_invariants(&mut storage);
            for _ in 0..4 {
                storage
                    .close_frame_and_batch(&mut head, 100)
                    .expect("close");
                assert_tree_invariants(&mut storage);
            }
            // Tree: 0(Gold sentinel in concept)→1→2→3→4 (Tip)

            // Phase 2: cascade with a valid ancestor. Batch 0 is accepted first.
            storage
                .append_safe_inputs(
                    20,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 20,
                    }],
                    &default_protocol_config(),
                )
                .expect("append accepted");
            storage
                .append_safe_inputs(1400, &[], &default_protocol_config())
                .expect("advance past threshold");
            let inv = storage.detect_and_recover(1200).expect("recover");
            assert!(!inv.is_empty(), "partial cascade should invalidate");
            assert_tree_invariants(&mut storage);

            // Phase 3: more rotations after partial cascade.
            let mut head = storage.load_open_state().expect("load").unwrap();
            for _ in 0..3 {
                storage
                    .close_frame_and_batch(&mut head, 1500)
                    .expect("close gen2");
                assert_tree_invariants(&mut storage);
            }

            // Phase 4: torn cascade — invalidate everything including batch 0.
            let latest = storage.latest_batch_index().expect("latest").unwrap();
            for bi in 0..=latest {
                storage.insert_invalid_batch(bi).expect("invalidate");
            }
            storage.detect_and_recover(1200).expect("recover from torn");
            assert_tree_invariants(&mut storage);

            // Phase 5: rotations after torn cascade — new Tip has parent=NULL, nonce=0.
            let mut head = storage.load_open_state().expect("load").unwrap();
            for _ in 0..5 {
                storage
                    .close_frame_and_batch(&mut head, 2000)
                    .expect("close gen3");
                assert_tree_invariants(&mut storage);
            }
        }

        #[test]
        fn subtree_by_batch_index_equals_subtree_by_parent_walk() {
            // §12.5.2: cascade queries use `batch_index >= N` as a shortcut for
            // "subtree rooted at N". This test asserts the equivalence on a
            // realistic scenario with multiple cascade generations.
            let db = temp_db("subtree-equivalence");
            let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
            let batch_submitter = SENDER_A;

            // Build: 5 batches, cascade from 2 (partial), 3 more, cascade from 1 (torn-ish).
            let mut head = storage
                .initialize_open_state(10, SafeInputRange::empty_at(0))
                .expect("initialize");
            for _ in 0..4 {
                storage
                    .close_frame_and_batch(&mut head, 100)
                    .expect("close");
            }
            storage
                .append_safe_inputs(
                    20,
                    &[StoredSafeInput {
                        sender: batch_submitter,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 20,
                    }],
                    &default_protocol_config(),
                )
                .expect("append accepted");
            storage
                .append_safe_inputs(1400, &[], &default_protocol_config())
                .expect("advance");
            let _ = storage.detect_and_recover(1200).expect("cascade 1");

            let mut head = storage.load_open_state().expect("load").unwrap();
            for _ in 0..2 {
                storage
                    .close_frame_and_batch(&mut head, 1500)
                    .expect("close");
            }

            // Assert equivalence among VALID batches for every valid N.
            // Restricting both sides to `valid_batches` is the invariant cascade
            // relies on: its WHERE filters invalidated rows, so the two sets need
            // only agree on the valid subset.
            let valid_bi: Vec<u64> = {
                let mut stmt = storage
                    .conn
                    .prepare("SELECT batch_index FROM valid_batches ORDER BY batch_index")
                    .expect("prepare");
                stmt.query_map([], |row| row.get::<_, i64>(0).map(i64_to_u64))
                    .expect("query")
                    .collect::<rusqlite::Result<_>>()
                    .expect("collect")
            };
            for &n in &valid_bi {
                let by_index: Vec<u64> = {
                    let mut stmt = storage
                        .conn
                        .prepare(
                            "SELECT batch_index FROM valid_batches \
                         WHERE batch_index >= ?1 ORDER BY batch_index",
                        )
                        .expect("prepare");
                    stmt.query_map(params![u64_to_i64(n)], |row| {
                        row.get::<_, i64>(0).map(i64_to_u64)
                    })
                    .expect("query")
                    .collect::<rusqlite::Result<_>>()
                    .expect("collect")
                };
                let by_subtree: Vec<u64> = {
                    let mut stmt = storage
                        .conn
                        .prepare(
                            "WITH RECURSIVE subtree(batch_index) AS ( \
                           SELECT batch_index FROM valid_batches WHERE batch_index = ?1 \
                           UNION ALL \
                           SELECT b.batch_index FROM valid_batches b \
                           JOIN subtree s ON b.parent_batch_index = s.batch_index \
                         ) \
                         SELECT batch_index FROM subtree ORDER BY batch_index",
                        )
                        .expect("prepare");
                    stmt.query_map(params![u64_to_i64(n)], |row| {
                        row.get::<_, i64>(0).map(i64_to_u64)
                    })
                    .expect("query")
                    .collect::<rusqlite::Result<_>>()
                    .expect("collect")
                };
                assert_eq!(
                    by_index, by_subtree,
                    "cascade root {n}: valid batch_index >= N diverged from valid parent-walk subtree"
                );
            }
        }
    }
}
