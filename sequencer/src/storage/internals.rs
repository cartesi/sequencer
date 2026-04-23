// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Cross-writer helpers — anything used by more than one of the writer-role
//! files lives here. Single-caller SQL stays inline in the writer that owns it.
//!
//! Visibility is `pub(super)` throughout so all `impl Storage` files in
//! `storage/` can reach it. Nothing here is part of the public API.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::Address;
use rusqlite::{Connection, Result, Transaction, params};

use super::{BatchPolicy, SafeInputRange, WriteHead};
use sequencer_core::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};

// ── Write-head loading and validation ─────────────────────────────────────
//
// Used by ingress (initialize/append/close) and recovery (open recovery batch
// after cascade). The WriteHead is the in-memory mirror of the latest open
// batch/frame and must always match what's persisted in `batches` and `frames`.

pub(super) fn load_current_write_head(tx: &Transaction<'_>) -> Result<Option<WriteHead>> {
    // The Tip is the single row in `valid_open_batch` (enforced by
    // `ux_single_valid_tip`). Returns None if there's no Tip (fresh DB,
    // or torn state between cascade and recovery-batch open).
    let latest_batch = match tx.query_row(
        "SELECT
            b.batch_index,
            b.created_at_ms,
            (SELECT COUNT(*) FROM user_ops u WHERE u.batch_index = b.batch_index) AS user_op_count
         FROM valid_open_batch b",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    ) {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(other) => return Err(other),
    };
    let (batch_index_i64, batch_created_at_ms, batch_user_op_count_i64) = latest_batch;

    let (frame_in_batch_i64, frame_fee_i64, safe_block_i64): (i64, i64, i64) = tx.query_row(
        "SELECT frame_in_batch, fee, safe_block FROM frames \
         WHERE batch_index = ?1 ORDER BY frame_in_batch DESC LIMIT 1",
        params![batch_index_i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    let open_frame_user_op_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM user_ops WHERE batch_index = ?1 AND frame_in_batch = ?2",
        params![batch_index_i64, frame_in_batch_i64],
        |row| row.get(0),
    )?;

    let policy = query_batch_policy(tx)?;
    Ok(Some(WriteHead {
        batch_index: i64_to_u64(batch_index_i64),
        batch_created_at: from_unix_ms(batch_created_at_ms),
        frame_fee: i64_to_u16(frame_fee_i64),
        safe_block: i64_to_u64(safe_block_i64),
        batch_user_op_count: i64_to_u64(batch_user_op_count_i64),
        open_frame_user_op_count: i64_to_u32(open_frame_user_op_count),
        frame_in_batch: i64_to_u32(frame_in_batch_i64),
        max_batch_user_op_bytes: super::batch_size_target_bytes(policy),
    }))
}

// ── Cross-writer reads (no `&mut self` needed) ───────────────────────────

pub(super) fn query_latest_safe_input_index_exclusive(conn: &Connection) -> Result<u64> {
    let value: Option<i64> =
        conn.query_row("SELECT MAX(safe_input_index) FROM safe_inputs", [], |row| {
            row.get(0)
        })?;
    Ok(match value {
        Some(last_index) => i64_to_u64(last_index).saturating_add(1),
        None => 0,
    })
}

pub(super) fn query_current_safe_block(conn: &Connection) -> Result<u64> {
    let value: i64 = conn.query_row(
        "SELECT block_number FROM l1_safe_head WHERE singleton_id = 0 LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    Ok(i64_to_u64(value))
}

pub(super) fn query_batch_policy(conn: &Connection) -> Result<BatchPolicy> {
    let (log_recommended_fee, log_batch_size_target): (i64, i64) = conn.query_row(
        "SELECT log_recommended_fee, log_batch_size_target FROM batch_policy_derived \
         WHERE singleton_id = 0 LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let max_exp = sequencer_core::fee::MAX_EXPONENT;
    Ok(BatchPolicy {
        // Clamp to MAX_EXPONENT to prevent panics in fee_to_linear.
        recommended_fee: i64_to_u16(log_recommended_fee).min(max_exp),
        batch_size_target: i64_to_u16(log_batch_size_target).min(max_exp),
    })
}

// ── Batch / frame insert helpers (used by ingress and recovery) ───────────

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

// ── L2-tx row decoding (shared between egress page reads and per-batch loads) ─

/// Decode a single ordered-L2-tx row into a `SequencedL2Tx`.
///
/// Callers materialize the row fields directly inside their `query_map` closure
/// and pass them here. This avoids defining an intermediate row struct just to
/// destructure it immediately.
pub(super) fn decode_l2_tx_row(
    kind: i64,
    sender: Option<Vec<u8>>,
    data: Option<Vec<u8>>,
    fee: Option<i64>,
    payload: Option<Vec<u8>>,
    block_number: Option<i64>,
) -> SequencedL2Tx {
    let sender_bytes = sender.expect("ordered replay row: missing sender");
    assert_eq!(
        sender_bytes.len(),
        20,
        "ordered replay row: sender must be 20 bytes"
    );
    if kind == 0 {
        SequencedL2Tx::UserOp(ValidUserOp {
            sender: Address::from_slice(sender_bytes.as_slice()),
            // Replay uses the persisted frame fee (log-space exponent) to mirror canonical execution.
            fee: i64_to_u16(fee.expect("ordered replay row: missing fee")),
            data: data.expect("ordered replay row: missing data"),
        })
    } else {
        SequencedL2Tx::Direct(DirectInput {
            sender: Address::from_slice(sender_bytes.as_slice()),
            block_number: i64_to_u64(
                block_number.expect("ordered replay row: missing block_number"),
            ),
            payload: payload.expect("ordered replay row: missing payload"),
        })
    }
}

// ── Time helpers ──────────────────────────────────────────────────────────

pub(super) fn to_unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub(super) fn from_unix_ms(ms: i64) -> SystemTime {
    let clamped_ms = ms.max(0) as u64;
    UNIX_EPOCH + Duration::from_millis(clamped_ms)
}

pub(super) fn now_unix_ms() -> i64 {
    to_unix_ms(SystemTime::now())
}

// ── Width conversions (saturating; SQLite ↔ Rust integer widths) ──────────

pub(super) fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn i64_to_u64(value: i64) -> u64 {
    value.max(0) as u64
}

pub(super) fn i64_to_u16(value: i64) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

pub(super) fn i64_to_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}
