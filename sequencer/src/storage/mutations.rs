// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Write helpers shared across writer roles. Like [`super::queries`], they take a
//! `&Transaction` so they compose inside a larger atomic unit.

use rusqlite::{Result, Transaction, named_params, params};

use super::convert::{i64_to_u64, u64_to_i64};
use super::history::{era_in, next_l2_tx_count_in};
use super::queries::{DIRECT_EXECUTES_SQL, DirectOperands};
use super::{DirectInputExecution, SafeInputRange};

/// Insert a new batch after the last one. Its nonce is the last nonce + 1, or
/// the era's anchor for the era's first batch.
///
/// If `batch_index_opt` is None, SQLite auto-assigns (highest existing +1).
/// The explicit form pins the very first genesis batch at `batch_index = 0`.
/// `trg_batch_insert` checks the same rule, so caller and schema agree.
/// Returns the new batch's index and nonce.
pub(super) fn insert_new_batch(
    tx: &Transaction<'_>,
    batch_index_opt: Option<u64>,
    created_at_ms: i64,
) -> Result<(u64, u64)> {
    let nonce = compute_next_nonce(tx)?;
    let batch_index = match batch_index_opt {
        Some(bi) => {
            tx.execute(
                "INSERT INTO batches (batch_index, nonce, created_at_ms) VALUES (?1, ?2, ?3)",
                params![u64_to_i64(bi), u64_to_i64(nonce), created_at_ms],
            )?;
            bi
        }
        None => {
            tx.execute(
                "INSERT INTO batches (nonce, created_at_ms) VALUES (?1, ?2)",
                params![u64_to_i64(nonce), created_at_ms],
            )?;
            i64_to_u64(tx.last_insert_rowid())
        }
    };
    Ok((batch_index, nonce))
}

/// The last nonce + 1, or the era's anchor when there is no batch: the rule of
/// `trg_batch_insert`. With neither, reading the era fails.
fn compute_next_nonce(tx: &Transaction<'_>) -> Result<u64> {
    let last: Option<i64> = tx.query_row("SELECT MAX(nonce) FROM batches", [], |row| row.get(0))?;
    match last {
        Some(nonce) => Ok(i64_to_u64(nonce)
            .checked_add(1)
            .expect("batch nonce overflow: contract-impossible")),
        None => Ok(era_in(tx)?.anchor_batch_nonce),
    }
}

/// Mark a batch as sealed (inclusion lane closed it). Write-once per
/// `trg_batch_lifecycle`.
/// The payload hash is stamped in the same UPDATE: a sealed batch always
/// carries the hash the content-identity check compares accepted L1
/// landings against (hash-at-seal).
pub(super) fn seal_batch(
    tx: &Transaction<'_>,
    batch_index: u64,
    sealed_at_ms: i64,
    payload_hash: &[u8; 32],
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE batches SET sealed_at_ms = ?1, payload_hash = ?2 WHERE batch_index = ?3",
        params![
            sealed_at_ms,
            payload_hash.as_slice(),
            u64_to_i64(batch_index)
        ],
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

/// Commit the exact direct inputs executed while opening a frame.
pub(super) fn persist_frame_direct_sequence(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    range: SafeInputRange,
    executions: &[DirectInputExecution],
) -> Result<()> {
    assert_eq!(
        executions,
        derive_direct_input_executions_in(tx, range)?,
        "direct executions must cover the complete external-input range at the next application count"
    );
    persist_direct_executions(tx, batch_index, frame_in_batch, executions)
}

/// Startup fixes the replay sequence before the engine is restored. Admission
/// succeeds only after catch-up applies every committed application input.
pub(super) fn persist_frame_direct_sequence_derived(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    range: SafeInputRange,
) -> Result<()> {
    let executions = derive_direct_input_executions_in(tx, range)?;
    persist_direct_executions(tx, batch_index, frame_in_batch, &executions)
}

fn persist_direct_executions(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    executions: &[DirectInputExecution],
) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO application_inputs (l2_tx_count, batch_index, frame_in_batch, safe_input_index)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for execution in executions {
        stmt.execute(params![
            u64_to_i64(execution.l2_tx_index.get()),
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            u64_to_i64(execution.safe_input_index)
        ])?;
    }
    Ok(())
}

/// The executions a drained range owes: one per direct that
/// [`DIRECT_EXECUTES_SQL`] admits, at consecutive counts. A submitter input
/// or an over-cap direct takes no count and gets no row.
fn derive_direct_input_executions_in(
    tx: &Transaction<'_>,
    range: SafeInputRange,
) -> Result<Vec<DirectInputExecution>> {
    if range.is_empty() {
        return Ok(Vec::new());
    }

    let operands = DirectOperands::load(tx)?;
    let mut next = next_l2_tx_count_in(tx)?;
    // Every input in the range is read, so the range's completeness is
    // checked even where none of its inputs executes.
    let mut stmt = tx.prepare_cached(&format!(
        "SELECT safe_input_index, {DIRECT_EXECUTES_SQL} \
         FROM safe_inputs \
         WHERE safe_input_index >= :start AND safe_input_index < :end \
         ORDER BY safe_input_index ASC"
    ))?;
    let rows = stmt.query_map(
        operands
            .bind(named_params! {
                ":start": u64_to_i64(range.start()),
                ":end": u64_to_i64(range.end()),
            })
            .as_slice(),
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
    )?;

    let mut executions = Vec::new();
    let mut fetched = 0_u64;
    for row in rows {
        let (safe_input_index, executes) = row?;
        let safe_input_index = i64_to_u64(safe_input_index);
        let expected_index = range
            .start()
            .checked_add(fetched)
            .expect("safe-input classification index overflow: contract-impossible");
        assert_eq!(
            safe_input_index, expected_index,
            "non-contiguous safe-input range while assigning execution offsets"
        );
        if executes {
            executions.push(DirectInputExecution {
                safe_input_index,
                l2_tx_index: next,
            });
            next = next
                .checked_next()
                .expect("L2 tx count overflow: contract-impossible");
        }
        fetched = fetched
            .checked_add(1)
            .expect("safe-input classification count overflow: contract-impossible");
    }
    assert_eq!(
        range.start().checked_add(fetched),
        Some(range.end()),
        "safe-input range was not fully populated while assigning execution offsets"
    );
    Ok(executions)
}
