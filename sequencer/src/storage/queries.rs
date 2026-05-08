// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Read-side helpers shared across writer-role files.
//!
//! These take a `&Connection` (or `&Transaction`, which derefs) rather than
//! `&mut Storage`, so they can compose inside a larger transaction built by
//! any writer role. Single-caller reads stay inline in the writer that owns
//! them; only the reads reused by two or more roles live here.

use alloy_primitives::Address;
use rusqlite::{Connection, OptionalExtension, Result, Transaction, params};

use super::convert::{from_unix_ms, i64_to_u16, i64_to_u32, i64_to_u64};
use super::{BatchPolicy, WriteHead};
use sequencer_core::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};

// ── Write-head loading ───────────────────────────────────────────────────
//
// Used by ingress (initialize/resume open state) and recovery (open recovery
// batch after cascade). The WriteHead is the in-memory mirror of the latest
// open batch/frame and must always match what's persisted in `batches` and
// `frames`.

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

// ── Cross-writer scalar reads ─────────────────────────────────────────────

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

pub(super) fn current_safe_block(conn: &Connection) -> Result<Option<u64>> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT block_number FROM l1_safe_head WHERE singleton_id = 0 LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.map(i64_to_u64))
}

/// Current safe block, or `QueryReturnedNoRows` if no observation has been
/// recorded yet. Use from code paths that only run after preemptive recovery
/// has produced a safe-head observation (submitter, lane, post-recovery
/// open-batch helpers); callers that legitimately handle the "never synced"
/// case should use [`current_safe_block`] instead.
pub(super) fn current_safe_block_required(conn: &Connection) -> Result<u64> {
    current_safe_block(conn)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// L1 timestamp (Unix seconds) of the current safe block, or `None` if no
/// real safe-head observation has recorded one yet.
pub(super) fn current_safe_block_timestamp(conn: &Connection) -> Result<Option<u64>> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT block_timestamp FROM l1_safe_head WHERE singleton_id = 0 LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.map(i64_to_u64))
}

/// Wall-clock timestamp (Unix ms) of the last observed safe-head advance, or
/// `None` if no real safe-head observation has occurred yet.
pub(super) fn last_safe_progress_ms(conn: &Connection) -> Result<Option<u64>> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT synced_at_ms FROM l1_safe_head WHERE singleton_id = 0",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.map(i64_to_u64))
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

// ── Ordered L2-tx row decoding ───────────────────────────────────────────
//
// Used by egress paging and the per-batch replay reader. Each caller builds
// the row shape inside its own `query_map` closure and hands the fields to
// this decoder rather than defining an intermediate struct.

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
