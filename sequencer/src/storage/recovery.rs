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
//! against arbitrarily malformed self-submissions:
//! [`populate_safe_accepted_batches_inner`] trusts that on-chain batches from
//! the sequencer's own address are structurally valid. The sequencer controls
//! its own submissions — this is a deliberate system assumption, not a gap.

use alloy_primitives::Address;
use rusqlite::{Connection, OptionalExtension, Result, Transaction, TransactionBehavior, params};

use super::Storage;
use super::internals::{
    batch_age_is_stale, i64_to_u64, insert_open_batch_with_index, insert_open_frame, now_unix_ms,
    persist_frame_direct_sequence, query_batch_policy, query_current_safe_block,
    query_latest_safe_input_index_exclusive, u64_to_i64,
};

impl Storage {
    /// Mark a single batch as invalid. Test-only seeder — production code goes
    /// through [`Storage::detect_and_recover`] / [`Storage::run_startup_recovery`].
    #[cfg(test)]
    pub(crate) fn insert_invalid_batch(&mut self, batch_index: u64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO invalid_batches (batch_index) VALUES (?1)",
            params![u64_to_i64(batch_index)],
        )?;
        Ok(())
    }

    /// Detect stale batches and cascade-invalidate, then restore the open-batch invariant.
    ///
    /// Runs detection, cascade invalidation, and recovery-batch opening inside a single
    /// `Immediate` transaction so the operation is crash-safe and atomic.
    ///
    /// Also handles the edge case where a previous boot invalidated the suffix but crashed
    /// before opening the fresh batch: if no new invalidations are found but no valid open
    /// batch exists, a recovery batch is opened.
    ///
    /// Returns the list of newly invalidated batch indices (empty if no stale batches found).
    pub fn detect_and_recover(&mut self, max_wait_blocks: u64) -> Result<Vec<u64>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let to_invalidate = detect_and_recover_inner(&tx, max_wait_blocks)?;
        tx.commit()?;
        Ok(to_invalidate)
    }

    /// Refresh the recovery-side metadata in one atomic transaction:
    /// 1. Populate `safe_accepted_batches` from L1 safe inputs (the gold frontier).
    /// 2. Assign nonces to any un-nonced valid batches.
    ///
    /// Called by the batch submitter each tick and by the recovery startup sequence
    /// before checking the danger zone. Both `populate` and `assign` are idempotent,
    /// so re-running this is safe.
    pub fn refresh_recovery_metadata(
        &mut self,
        batch_submitter_address: Address,
        max_wait_blocks: u64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        populate_safe_accepted_batches_inner(&tx, batch_submitter_address, max_wait_blocks)?;
        assign_batch_nonces_inner(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Full startup-recovery pipeline (refresh + detect_and_recover) wrapped
    /// in one atomic transaction. Returns the newly invalidated batch indices.
    pub fn run_startup_recovery(
        &mut self,
        batch_submitter_address: Address,
        max_wait_blocks: u64,
    ) -> Result<Vec<u64>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        populate_safe_accepted_batches_inner(&tx, batch_submitter_address, max_wait_blocks)?;
        assign_batch_nonces_inner(&tx)?;
        let invalidated = detect_and_recover_inner(&tx, max_wait_blocks)?;
        tx.commit()?;
        Ok(invalidated)
    }
}

// ── Free functions used by both recovery and the batch submitter ──────────

#[derive(Debug, Clone, Copy)]
pub(super) struct SafeAcceptedBatchRow {
    pub safe_input_index: i64,
    pub nonce: i64,
}

pub(super) fn query_latest_safe_accepted_batch(
    conn: &Connection,
) -> Result<Option<SafeAcceptedBatchRow>> {
    conn.query_row(
        "SELECT safe_input_index, nonce FROM safe_accepted_batches \
         ORDER BY safe_input_index DESC LIMIT 1",
        [],
        |row| {
            Ok(SafeAcceptedBatchRow {
                safe_input_index: row.get(0)?,
                nonce: row.get(1)?,
            })
        },
    )
    .optional()
}

/// Simulate the scheduler's acceptance logic over new safe inputs from
/// `batch_submitter_address` and append matches to `safe_accepted_batches`.
///
/// For each safe input newer than the cursor (the latest accepted row), in
/// `safe_input_index` order:
/// - SSZ-decode the payload as a [`sequencer_core::batch::Batch`]; on decode
///   failure, skip (we trust our own submissions, but defend against garbage).
/// - If the batch is stale by inclusion
///   (`inclusion_block - first_frame_safe_block >= max_wait_blocks`), skip —
///   the scheduler skips it too.
/// - If `batch.nonce == expected_nonce`, append and bump `expected_nonce`;
///   otherwise skip (out-of-order, duplicate, or post-recovery old submission).
///
/// Paginated to bound memory; the cursor advances with the scan.
pub(super) fn populate_safe_accepted_batches_inner(
    conn: &Connection,
    batch_submitter_address: Address,
    max_wait_blocks: u64,
) -> Result<()> {
    const PAGE_SIZE: i64 = 256;

    let latest_accepted = query_latest_safe_accepted_batch(conn)?;
    let mut cursor = latest_accepted
        .map(|row| row.safe_input_index)
        .unwrap_or(-1);
    let mut expected = latest_accepted
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(0);

    // Scan new safe_inputs from batch_submitter in order, paginated.
    const SQL: &str = "SELECT si.safe_input_index, si.payload, si.block_number \
                       FROM safe_inputs si \
                       WHERE si.sender = ?1 \
                         AND si.safe_input_index > ?2 \
                       ORDER BY si.safe_input_index ASC LIMIT ?3";
    loop {
        let mut stmt = conn.prepare_cached(SQL)?;
        let mut rows = stmt.query(rusqlite::params![
            batch_submitter_address.as_slice(),
            cursor,
            PAGE_SIZE,
        ])?;
        let mut page_count: i64 = 0;
        let mut to_insert = Vec::new();
        while let Some(row) = rows.next()? {
            page_count += 1;
            let safe_input_index: i64 = row.get(0)?;
            cursor = safe_input_index;
            let payload: Vec<u8> = row.get(1)?;
            let block_number: i64 = row.get(2)?;
            let Ok(batch) = <sequencer_core::batch::Batch as ssz::Decode>::from_ssz_bytes(&payload)
            else {
                continue;
            };

            // Skip stale batches — the scheduler skips them too.
            let first_frame_safe_block = batch.frames.first().map(|f| f.safe_block).unwrap_or(0);
            let inclusion_block = i64_to_u64(block_number);
            if !batch.frames.is_empty()
                && batch_age_is_stale(inclusion_block, first_frame_safe_block, max_wait_blocks)
            {
                continue;
            }

            // Only accept if nonce matches the expected sequence.
            if batch.nonce == expected {
                to_insert.push((
                    safe_input_index,
                    i64::try_from(batch.nonce).unwrap_or(i64::MAX),
                    i64::try_from(first_frame_safe_block).unwrap_or(i64::MAX),
                    block_number,
                ));
                expected = expected.saturating_add(1);
            }
        }
        drop(rows);
        drop(stmt);
        for (si_idx, nonce, first_frame_sb, inc_block) in to_insert {
            conn.execute(
                "INSERT OR IGNORE INTO safe_accepted_batches \
                 (safe_input_index, nonce, first_frame_safe_block, inclusion_block) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![si_idx, nonce, first_frame_sb, inc_block],
            )?;
        }
        if page_count < PAGE_SIZE {
            break;
        }
    }

    Ok(())
}

/// Assign nonces to all valid batches that don't yet have a nonce in `batch_nonces`.
/// See `Storage::assign_batch_nonces` for full doc.
pub(super) fn assign_batch_nonces_inner(conn: &Connection) -> Result<u64> {
    const SQL_LATEST_VALID_NONCE: &str = "SELECT nonce FROM valid_batch_nonces \
                                          ORDER BY batch_index DESC LIMIT 1";
    let latest_valid_nonce: Option<i64> = conn
        .query_row(SQL_LATEST_VALID_NONCE, [], |row| row.get(0))
        .optional()?;
    let mut next_nonce = latest_valid_nonce
        .map(|nonce| i64_to_u64(nonce).saturating_add(1))
        .unwrap_or(0);

    // The open batch (MAX(batch_index)) reads from `batches` directly because we
    // explicitly want to skip whichever row is currently the open one — including
    // it when it's invalid would be a no-op; including it when it's valid is wrong
    // because we don't assign nonces to open batches.
    let open_batch_index: Option<i64> =
        conn.query_row("SELECT MAX(batch_index) FROM batches", [], |row| row.get(0))?;
    let Some(open_batch_index) = open_batch_index else {
        return Ok(0);
    };

    const SQL_UNNONCED: &str = "SELECT batch_index FROM valid_batches \
                                WHERE batch_index NOT IN (SELECT batch_index FROM batch_nonces) \
                                  AND batch_index < ?1 \
                                ORDER BY batch_index ASC";
    let mut stmt = conn.prepare(SQL_UNNONCED)?;
    let mut rows = stmt.query(rusqlite::params![open_batch_index])?;
    let mut to_assign = Vec::new();
    while let Some(row) = rows.next()? {
        let bi: i64 = row.get(0)?;
        to_assign.push(i64_to_u64(bi));
    }
    drop(rows);
    drop(stmt);

    let count = to_assign.len() as u64;
    for bi in to_assign {
        conn.execute(
            "INSERT OR IGNORE INTO batch_nonces (batch_index, nonce) VALUES (?1, ?2)",
            params![u64_to_i64(bi), u64_to_i64(next_nonce)],
        )?;
        next_nonce = next_nonce.saturating_add(1);
    }

    Ok(count)
}

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
///   (a) a closed batch past the accepted frontier (visible via
///       `valid_batch_nonces`), or
///   (b) the currently-open batch (has no nonce, so invisible to (a) but
///       still at risk of aging into danger).
///
/// Closed-unaccepted batches are strictly older than the open batch (the
/// sequencer opens new batches at monotonically non-decreasing `safe_block`),
/// so the closed-frontier check takes precedence. Cascading from that batch
/// covers the open batch automatically via `batch_index >= N`.
///
/// Used by:
///   - `Storage::check_danger_zone` — preemptive danger check (submitter
///     worker tick + startup wall-clock fallback).
///   - `detect_and_recover_inner` — atomic cascade-invalidation path.
///
/// Keeping both call sites behind this single helper keeps them symmetric:
/// the preemptive and reactive paths can never diverge on what counts as "in
/// danger."
///
/// Requires `safe_accepted_batches` and `batch_nonces` to be populated (via
/// `refresh_recovery_metadata`) for the closed-frontier arm to function.
pub(super) fn find_first_batch_in_danger(conn: &Connection, threshold: u64) -> Result<Option<u64>> {
    if let Some(bi) = find_closed_frontier_batch_in_danger(conn, threshold)? {
        return Ok(Some(bi));
    }
    find_open_batch_in_danger(conn, threshold)
}

/// First closed batch past the accepted frontier whose `first_frame_safe_block`
/// is older than `current_safe_block - threshold`. Returns `None` if no closed
/// batch at the frontier matches.
///
/// Does not consider the open batch — `assign_batch_nonces` never nonces
/// `MAX(batch_index)`, so open batches are invisible to `valid_batch_nonces`.
/// The unified entrypoint `find_first_batch_in_danger` falls through to
/// `find_open_batch_in_danger` for that case.
///
/// Exposed to `l1_submission` so `Storage::check_danger_zone` can use this
/// directly — the submitter's zombie-detection check must NOT flag open
/// batches (they have no L1 tx to become a zombie).
pub(super) fn find_closed_frontier_batch_in_danger(
    conn: &Connection,
    threshold: u64,
) -> Result<Option<u64>> {
    let frontier_nonce = query_latest_safe_accepted_batch(conn)?
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(0);

    let batch_ref: Option<(i64, i64)> = conn
        .query_row(
            "SELECT batch_index, nonce FROM valid_batch_nonces \
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
    if batch_age_is_stale(safe_block, first_frame_safe_block, threshold) {
        Ok(Some(i64_to_u64(batch_index)))
    } else {
        Ok(None)
    }
}

/// Open batch (MAX `batch_index`, if valid) whose `first_frame_safe_block` is
/// older than `current_safe_block - threshold`. Returns `None` if no valid
/// open batch exists or it is not yet in danger.
///
/// The open batch has no `batch_nonces` row because `assign_batch_nonces`
/// explicitly skips `MAX(batch_index)`. It's therefore invisible to
/// `find_closed_frontier_batch_in_danger` and must be checked separately.
fn find_open_batch_in_danger(conn: &Connection, threshold: u64) -> Result<Option<u64>> {
    let max_bi: Option<i64> =
        conn.query_row("SELECT MAX(batch_index) FROM batches", [], |row| row.get(0))?;
    let Some(max_bi) = max_bi else {
        return Ok(None);
    };

    // A previous cascade may have invalidated everything up to and including
    // the latest batch (torn-invalidation case, handled by the caller re-
    // opening a fresh batch). In that state, there's no valid open batch —
    // don't double-invalidate.
    let is_invalid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM invalid_batches WHERE batch_index = ?1)",
        rusqlite::params![max_bi],
        |row| row.get(0),
    )?;
    if is_invalid {
        return Ok(None);
    }

    let first_frame_safe_block = first_frame_safe_block_of(conn, max_bi)?;
    let safe_block = query_current_safe_block(conn)?;
    if batch_age_is_stale(safe_block, first_frame_safe_block, threshold) {
        Ok(Some(i64_to_u64(max_bi)))
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
/// Reads the cascade list BEFORE inserting into `invalid_batches` — the SELECT
/// must see the rows the INSERT will then mark invalid (the view re-evaluates
/// per statement).
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
        .collect::<Result<_>>()?
    };

    if !invalidated.is_empty() {
        tx.execute(
            "INSERT INTO invalid_batches (batch_index) \
             SELECT batch_index FROM valid_batches WHERE batch_index >= ?1",
            params![from_i64],
        )?;
    }

    Ok(invalidated)
}

/// Check whether the DB has a valid (non-invalidated) open batch.
///
/// The open batch is always the absolute latest batch (MAX batch_index).
/// If the latest batch is in `invalid_batches`, there is no valid open batch.
fn has_valid_open_batch(tx: &Connection) -> Result<bool> {
    let max_bi: Option<i64> =
        tx.query_row("SELECT MAX(batch_index) FROM batches", [], |row| row.get(0))?;
    let Some(max_bi) = max_bi else {
        return Ok(false);
    };
    let is_invalid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM invalid_batches WHERE batch_index = ?1)",
        rusqlite::params![max_bi],
        |row| row.get(0),
    )?;
    Ok(!is_invalid)
}

/// Open a fresh recovery batch inside an existing transaction.
fn open_recovery_batch_in_tx(tx: &Transaction<'_>) -> Result<()> {
    let now_ms = now_unix_ms();
    let safe_block = query_current_safe_block(tx)?;

    let max_bi: Option<i64> =
        tx.query_row("SELECT MAX(batch_index) FROM batches", [], |row| row.get(0))?;
    let next_bi = i64_to_u64(max_bi.map(|b| b.saturating_add(1)).unwrap_or(0));

    let policy = query_batch_policy(tx)?;

    insert_open_batch_with_index(tx, next_bi, now_ms)?;
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
        SENDER_A, load_all_ordered_l2_txs, make_stale_batch_payload, seed_closed_batches, temp_db,
    };
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
    use alloy_primitives::Address;
    use sequencer_core::l2_tx::SequencedL2Tx;

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
            .append_safe_inputs(10, directs_0.as_slice())
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
            .append_safe_inputs(20, directs_1.as_slice())
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
            .append_safe_inputs(10, directs.as_slice())
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
            .append_safe_inputs(10, directs.as_slice())
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

        storage.assign_batch_nonces().expect("assign nonces");

        let batch_submitter = Address::repeat_byte(0xAA);
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

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

        storage.assign_batch_nonces().expect("assign nonces");
        let batch_submitter = Address::repeat_byte(0xAA);
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

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

        storage.assign_batch_nonces().expect("assign nonces gen1");

        let batch_submitter = Address::repeat_byte(0xAA);
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append stale safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab gen1");

        let first = storage.detect_and_recover(1200).expect("first recovery");
        assert_eq!(first, vec![0, 1]);

        let mut head = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close recovery batch");

        storage.assign_batch_nonces().expect("assign nonces gen2");

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
        storage.assign_batch_nonces().expect("assign nonces gen1");

        let batch_submitter = Address::repeat_byte(0xAA);
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append gen1 stale safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab gen1");

        let first = storage.detect_and_recover(1200).expect("gen1 recovery");
        assert_eq!(first, vec![0, 1]);

        let mut head = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close gen2 batch");
        storage.assign_batch_nonces().expect("assign nonces gen2");

        storage
            .append_safe_inputs(
                2410,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 100),
                    block_number: 2410,
                }],
            )
            .expect("append gen2 stale safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab gen2");

        let second = storage.detect_and_recover(1200).expect("gen2 recovery");
        assert_eq!(
            second,
            vec![2, 3],
            "stale reused nonce in gen2 must still be detected"
        );
    }

    // ── §7.3 — open-batch staleness regression (post-unification) ──────────
    //
    // Original bug: an open (unclosed, not-yet-nonced) batch whose first
    // frame was pinned to an old safe_block escaped detection, because the
    // frontier lookup only queries `valid_batch_nonces` (which `assign_batch_nonces`
    // never populates for the max batch_index).
    //
    // After the unification refactor, both the preemptive danger check and
    // the reactive cascade path go through `find_first_batch_in_danger`,
    // which falls through to `find_open_batch_in_danger` when no closed
    // frontier batch matches. These tests verify the reactive path
    // (`detect_and_recover`); parallel tests for the preemptive path
    // (`check_danger_zone`) live under the `check_danger_zone` header below.
    //
    // Below covers four cases:
    //   - positive: open batch IS stale → invalidated
    //   - negative: open batch is fresh → NOT invalidated (no false positives)
    //   - combined: closed+stale AND open+stale → both invalidated in one cascade
    //   - no-batch: empty DB with no open batch → no-op, no panic

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
            .append_safe_inputs(1500, &[])
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
            .append_safe_inputs(1100, &[])
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
            .append_safe_inputs(1210, &[])
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
            .append_safe_inputs(1209, &[])
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
        storage.assign_batch_nonces().expect("assign nonces");

        // Advance safe head so batch 0's first frame (safe_block=10) is stale.
        storage
            .append_safe_inputs(1500, &[])
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
            .append_safe_inputs(10, deposits.as_slice())
            .expect("append deposits");
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
            .expect("close frame with deposits");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        let before = load_all_ordered_l2_txs(&mut storage);
        assert_eq!(before.len(), 2, "both deposits should be visible");

        storage.assign_batch_nonces().expect("assign nonces");
        let batch_submitter = Address::repeat_byte(0xAA);
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append stale batch submission");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

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
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 20,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        // Advance to a current safe block where batch 0 (safe_block=10) is
        // past threshold (1200-10=1190>=1125) but batch 1 (safe_block=100)
        // is still fresh (1200-100=1100<1125).
        storage
            .append_safe_inputs(1200, &[])
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
            .append_safe_inputs(1200, &[])
            .expect("advance safe head past danger threshold");

        let result = storage.check_danger_zone(1125).expect("check danger zone");
        assert!(
            result.is_none(),
            "open batch (no zombie) must not trigger check_danger_zone; got batch_index={result:?}"
        );
    }

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
            .append_safe_inputs(1200, &[])
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
            .append_safe_inputs(1100, &[])
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
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 20,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        storage
            .append_safe_inputs(1200, &[])
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
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 20,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        storage
            .append_safe_inputs(1134, &[])
            .expect("advance safe block");

        let result = storage.check_danger_zone(1125).expect("check danger zone");
        assert!(
            result.is_none(),
            "should not trigger below threshold; got batch_index={result:?}"
        );
    }

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
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                1300,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 100),
                    block_number: 1300,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate sab");

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
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                1299,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 100),
                    block_number: 1299,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate sab");

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
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate");

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
        storage.assign_batch_nonces().expect("nonces gen1");

        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append gen1");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate gen1");
        let inv1 = storage.detect_and_recover(max_wait).expect("recover gen1");
        assert_eq!(inv1, vec![0, 1]);

        let mut head2 = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head2, 1210)
            .expect("close gen2");
        storage.assign_batch_nonces().expect("nonces gen2");

        storage
            .append_safe_inputs(
                2410,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 1210),
                    block_number: 2410,
                }],
            )
            .expect("append gen2");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate gen2");
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
        storage.assign_batch_nonces().expect("nonces");
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate");
        storage.detect_and_recover(max_wait).expect("recover gen1");

        let mut head2 = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head2, 1210)
            .expect("close gen2");
        storage.assign_batch_nonces().expect("nonces gen2");
        storage
            .append_safe_inputs(
                2410,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 1210),
                    block_number: 2410,
                }],
            )
            .expect("append gen2");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate gen2");
        storage.detect_and_recover(max_wait).expect("recover gen2");

        let mut head3 = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head3, 2410)
            .expect("close gen3");
        storage.assign_batch_nonces().expect("nonces gen3");
        storage
            .append_safe_inputs(
                2420,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 2410),
                    block_number: 2420,
                }],
            )
            .expect("append gen3");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate gen3");
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
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate");

        let inv = storage.detect_and_recover(max_wait).expect("detect");
        assert_eq!(inv.len(), 51);
    }
}
