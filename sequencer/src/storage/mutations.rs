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
use super::history::{
    ExecutedInputMapping, attach_executed_inputs_in, next_executed_input_count_in,
};
use super::l1_inputs::query_deployment_identity;
use super::{DirectInputExecution, SafeInputRange};

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
        // A parentless root carries the deployment's batch-tree anchor nonce:
        // 0 for a genesis deployment, N' for a cockroach-recovered one. This
        // generalizes the old hard-coded 0; the `batch_tree_anchor` row defaults
        // to 0, so genesis and post-cascade re-roots are unchanged. Mirrored by
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
/// landings against (review R2, hash-at-seal).
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

/// Insert one `sequenced_l2_txs` row per safe-input index in `range` for the
/// given (batch, frame). Used by ingress (frame close) and recovery (re-drain
/// after cascade invalidation).
pub(super) fn persist_frame_direct_sequence(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    range: SafeInputRange,
    executions: &[DirectInputExecution],
) -> Result<()> {
    let expected = derive_direct_input_executions_in(tx, range)?;
    assert_eq!(
        executions, expected,
        "direct execution attributions must cover exactly the non-submitter drained rows from the canonical next count"
    );
    persist_frame_direct_sequence_inner(tx, batch_index, frame_in_batch, range, executions)
}

/// Sequence a production startup/recovery Tip's leading direct range, deriving
/// its complete application attribution from the persisted deployment identity
/// and current canonical application boundary.
pub(super) fn persist_frame_direct_sequence_derived(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    range: SafeInputRange,
) -> Result<()> {
    let executions = derive_direct_input_executions_in(tx, range)?;
    persist_frame_direct_sequence_inner(tx, batch_index, frame_in_batch, range, &executions)
}

/// Sequence physical cursor rows that intentionally represent no newly
/// executed application inputs. Cockroach-root padding uses this because the
/// recovered snapshot already contains their effects; test fixtures use it
/// when exercising only the physical ordering layer.
pub(super) fn persist_frame_direct_sequence_physical_only(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    range: SafeInputRange,
) -> Result<()> {
    persist_frame_direct_sequence_inner(tx, batch_index, frame_in_batch, range, &[])
}

fn persist_frame_direct_sequence_inner(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    range: SafeInputRange,
    executions: &[DirectInputExecution],
) -> Result<()> {
    if range.is_empty() {
        assert!(
            executions.is_empty(),
            "empty safe-input range cannot carry execution attributions"
        );
        return Ok(());
    }

    let mut previous_safe_input_index = None;
    for execution in executions {
        assert!(
            execution.safe_input_index >= range.start() && execution.safe_input_index < range.end(),
            "direct execution attribution lies outside its drained range"
        );
        if let Some(previous) = previous_safe_input_index {
            assert!(
                execution.safe_input_index > previous,
                "direct execution attributions must be strictly ordered by safe-input index"
            );
        }
        previous_safe_input_index = Some(execution.safe_input_index);
    }

    let mut stmt = tx.prepare_cached(
        "INSERT INTO sequenced_l2_txs (batch_index, frame_in_batch, user_op_pos_in_frame, safe_input_index) \
         VALUES (?1, ?2, NULL, ?3)",
    )?;
    let mut executions = executions.iter().peekable();
    let mut mappings = Vec::with_capacity(executions.len());
    for safe_input_index in range.start()..range.end() {
        stmt.execute(params![
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            u64_to_i64(safe_input_index),
        ])?;
        if executions
            .peek()
            .is_some_and(|execution| execution.safe_input_index == safe_input_index)
        {
            let execution = executions.next().expect("peeked direct execution");
            mappings.push(ExecutedInputMapping {
                sequenced_l2_tx_offset: i64_to_u64(tx.last_insert_rowid()),
                executed_input_offset: execution.executed_input_offset,
            });
        }
    }
    assert!(
        executions.next().is_none(),
        "not every direct execution attribution was persisted"
    );
    drop(stmt);
    attach_executed_inputs_in(tx, &mappings)?;
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
