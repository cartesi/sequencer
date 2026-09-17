// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Write-side helpers shared across writer-role files.
//!
//! Like [`super::queries`] these take `&Transaction` so they compose inside a
//! larger atomic unit. The two consumers today are ingress (batch/frame close
//! + re-drain) and recovery (opening a recovery batch after cascade).

use alloy_primitives::Address;
use rusqlite::{Connection, Result, Transaction, params};

use super::convert::{i64_to_u64, u64_to_i64};
use super::history::next_executed_input_count_in;
use super::l1_inputs::query_deployment_identity;
use super::{DirectInputExecution, SafeInputRange};

/// Insert a new batch. Nonce is derived from `parent_batch_index`:
/// `parent.nonce + 1`, or the deployment's anchor for a parentless root
/// (genesis, cockroach recovery, or a fully invalidated branch).
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
        // A parentless root carries the deployment's batch-tree anchor nonce:
        // 0 for a genesis deployment, N' for a cockroach-recovered one. Mirrored by
        // `trg_enforce_nonce_contiguity`'s parentless arm.
        None => batch_tree_anchor_in(tx),
        Some(parent_bi) => {
            let parent_nonce: i64 = tx.query_row(
                "SELECT nonce FROM batches WHERE batch_index = ?1",
                params![u64_to_i64(parent_bi)],
                |row| row.get(0),
            )?;
            Ok(i64_to_u64(parent_nonce)
                .checked_add(1)
                .expect("batch nonce overflow: contract-impossible"))
        }
    }
}

/// Mark a batch as sealed (inclusion lane closed it). Write-once per the
/// `trg_sealed_at_ms_write_once` / `trg_payload_hash_write_once` triggers.
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

/// Set the batch-tree anchor nonce — the nonce the single parentless root
/// carries (0 for genesis, `N'` for a cockroach-recovered deployment). Composes
/// inside the recovery-fill transaction. `trg_batch_tree_anchor_write_once`
/// rejects this once `setup_complete` exists, so it is callable only during
/// setup, before its atomic completion.
pub(super) fn set_batch_tree_anchor_in(tx: &Transaction<'_>, nonce: u64) -> Result<()> {
    let changed = tx.execute(
        "UPDATE batch_tree_anchor SET nonce = ?1 WHERE singleton_id = 0",
        params![u64_to_i64(nonce)],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

/// Read the batch-tree anchor nonce (default 0). Mirrors
/// [`set_batch_tree_anchor_in`]. The single home for the anchor-read query:
/// takes `&Connection` (not `&Transaction`) so both the in-transaction writer
/// ([`compute_next_nonce`]) and the connection-level frontier readers
/// (`frontier_nonce`, `populate_safe_accepted_batches`) share it. A
/// `&Transaction` coerces to `&Connection` at the call site.
pub(super) fn batch_tree_anchor_in(conn: &Connection) -> Result<u64> {
    let anchor: i64 = conn.query_row(
        "SELECT nonce FROM batch_tree_anchor WHERE singleton_id = 0",
        [],
        |row| row.get(0),
    )?;
    Ok(i64_to_u64(anchor))
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
        "INSERT INTO application_inputs (offset, batch_index, frame_in_batch, safe_input_index)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for execution in executions {
        stmt.execute(params![
            u64_to_i64(execution.executed_input_offset.get()),
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            u64_to_i64(execution.safe_input_index)
        ])?;
    }
    Ok(())
}

fn derive_direct_input_executions_in(
    tx: &Transaction<'_>,
    range: SafeInputRange,
) -> Result<Vec<DirectInputExecution>> {
    if range.is_empty() {
        return Ok(Vec::new());
    }

    let identity = query_deployment_identity(tx)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let mut next = next_executed_input_count_in(tx)?;
    let mut stmt = tx.prepare_cached(
        "SELECT safe_input_index, sender \
         FROM safe_inputs \
         WHERE safe_input_index >= ?1 AND safe_input_index < ?2 \
         ORDER BY safe_input_index ASC",
    )?;
    let rows = stmt.query_map(
        params![u64_to_i64(range.start()), u64_to_i64(range.end())],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;

    let mut executions = Vec::new();
    let mut fetched = 0_u64;
    for row in rows {
        let (safe_input_index, sender) = row?;
        let safe_input_index = i64_to_u64(safe_input_index);
        let expected_index = range
            .start()
            .checked_add(fetched)
            .expect("safe-input classification index overflow: contract-impossible");
        assert_eq!(
            safe_input_index, expected_index,
            "non-contiguous safe-input range while assigning execution offsets"
        );
        if Address::from_slice(sender.as_slice()) != identity.batch_submitter_address {
            executions.push(DirectInputExecution {
                safe_input_index,
                executed_input_offset: next,
            });
            next = next
                .checked_next()
                .expect("executed input count overflow: contract-impossible");
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
