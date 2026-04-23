// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Write-side helpers shared across writer-role files.
//!
//! Like [`super::queries`] these take `&Transaction` so they compose inside a
//! larger atomic unit. The two consumers today are ingress (batch/frame close
//! + re-drain) and recovery (opening a recovery batch after cascade).

use rusqlite::{Result, Transaction, params};

use super::SafeInputRange;
use super::convert::{i64_to_u64, u64_to_i64};

/// Insert a new batch. Nonce is derived from `parent_batch_index`:
/// `parent.nonce + 1`, or 0 if `parent_batch_index` is None (genesis or
/// post-cascade torn-state new Tip).
///
/// If `batch_index_opt` is None, SQLite auto-assigns (highest existing +1).
/// The explicit form is used only by `initialize_open_state` to pin the
/// very first genesis batch at `batch_index = 0`.
///
/// The `trg_enforce_nonce_contiguity` trigger verifies the nonce matches
/// `parent.nonce + 1`, so caller and schema agree.
pub(super) fn insert_new_batch(
    tx: &Transaction<'_>,
    batch_index_opt: Option<u64>,
    parent_batch_index: Option<u64>,
    created_at_ms: i64,
) -> Result<u64> {
    let nonce = compute_next_nonce(tx, parent_batch_index)?;
    match batch_index_opt {
        Some(bi) => {
            tx.execute(
                "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    u64_to_i64(bi),
                    parent_batch_index.map(u64_to_i64),
                    u64_to_i64(nonce),
                    created_at_ms
                ],
            )?;
            Ok(bi)
        }
        None => {
            tx.execute(
                "INSERT INTO batches (parent_batch_index, nonce, created_at_ms) \
                 VALUES (?1, ?2, ?3)",
                params![
                    parent_batch_index.map(u64_to_i64),
                    u64_to_i64(nonce),
                    created_at_ms
                ],
            )?;
            Ok(i64_to_u64(tx.last_insert_rowid()))
        }
    }
}

fn compute_next_nonce(tx: &Transaction<'_>, parent_batch_index: Option<u64>) -> Result<u64> {
    match parent_batch_index {
        None => Ok(0),
        Some(parent_bi) => {
            let parent_nonce: i64 = tx.query_row(
                "SELECT nonce FROM batches WHERE batch_index = ?1",
                params![u64_to_i64(parent_bi)],
                |row| row.get(0),
            )?;
            Ok(i64_to_u64(parent_nonce).saturating_add(1))
        }
    }
}

/// Mark a batch as sealed (inclusion lane closed it). Write-once per the
/// `trg_sealed_at_ms_write_once` trigger.
pub(super) fn seal_batch(tx: &Transaction<'_>, batch_index: u64, sealed_at_ms: i64) -> Result<()> {
    let changed = tx.execute(
        "UPDATE batches SET sealed_at_ms = ?1 WHERE batch_index = ?2",
        params![sealed_at_ms, u64_to_i64(batch_index)],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

pub(super) fn insert_open_frame(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    created_at_ms: i64,
    frame_fee: u16,
    safe_block: u64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO frames (batch_index, frame_in_batch, created_at_ms, fee, safe_block) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            created_at_ms,
            i64::from(frame_fee),
            u64_to_i64(safe_block),
        ],
    )?;
    Ok(())
}

/// Insert one `sequenced_l2_txs` row per safe-input index in `range` for the
/// given (batch, frame). Used by ingress (frame close) and recovery (re-drain
/// after cascade invalidation).
pub(super) fn persist_frame_direct_sequence(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    range: SafeInputRange,
) -> Result<()> {
    if range.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO sequenced_l2_txs (batch_index, frame_in_batch, user_op_pos_in_frame, safe_input_index) \
         VALUES (?1, ?2, NULL, ?3)",
    )?;
    for safe_input_index in range.start()..range.end() {
        stmt.execute(params![
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            u64_to_i64(safe_input_index),
        ])?;
    }
    Ok(())
}
