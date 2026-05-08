// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Materialized view of the scheduler-accepted batches.
//!
//! `safe_accepted_batches` caches the prefix of submitted batches that the
//! on-chain scheduler would accept, based on an off-chain simulation of its
//! acceptance rules (see [`sequencer_core::protocol::ProtocolConfig`]).
//!
//! Maintenance contract: the view is advanced atomically with each
//! [`super::Storage::append_safe_inputs`] write, so any reader that sees
//! `l1_safe_head` at block B also sees every acceptance decision up to B. No
//! caller should populate this view directly.
//!
//! Readers:
//! - batch submitter frontier / danger reads (`submitter_frontier`,
//!   `check_danger`)
//! - recovery cascade (`find_closed_frontier_batch_in_danger`)
//! - wall-clock and stalled-safe-head danger estimates
//!
//! The only writer is [`populate_safe_accepted_batches`], invoked from
//! `append_safe_inputs` inside its transaction.

use rusqlite::{Connection, OptionalExtension, Result, params};

use super::convert::{i64_to_u64, u64_to_i64};
use sequencer_core::protocol::{ProtocolConfig, SafeInputView};

/// One row of `safe_accepted_batches`, exposing just the columns the
/// frontier-read code paths need.
#[derive(Debug, Clone, Copy)]
pub(super) struct SafeAcceptedBatchRow {
    pub safe_input_index: i64,
    pub nonce: i64,
}

/// The most recently accepted row, or `None` if the view is empty.
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

/// Next nonce the scheduler is expected to accept — the gold frontier's
/// "expected next" cursor.
///
/// Returns `latest_accepted.nonce + 1` if any batch has been accepted, else `0`.
/// Equivalently, the nonce that the very next valid closed batch (the cascade
/// pivot, when one exists) will carry, by the contiguity invariant on the
/// valid path (`trg_enforce_nonce_contiguity`).
pub(super) fn frontier_nonce(conn: &Connection) -> Result<u64> {
    Ok(query_latest_safe_accepted_batch(conn)?
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(0))
}

/// Simulate the scheduler's acceptance logic over new safe inputs and append
/// matches to `safe_accepted_batches`.
///
/// Paginates through `safe_inputs` rows newer than the latest accepted row,
/// pre-filtered at SQL to the batch-submitter's sender. For each row,
/// delegates to [`ProtocolConfig::scheduler_accepts`] with the
/// currently-expected nonce — on `Some`, inserts the accepted row and advances
/// expected; on `None`, moves on. The SQL sender filter is an optimization;
/// `scheduler_accepts` re-checks defensively, so the filter is
/// correctness-neutral.
///
/// The scan cursor is local to one invocation. Persistently, the only cursor
/// is the latest accepted row in `safe_accepted_batches`, not the latest row
/// scanned. That is intentional for now: a recovery batch may reuse the same
/// scheduler nonce after earlier rejected rows, and a too-eager persistent
/// scan cursor would risk skipping it. The tradeoff is that rejected
/// batch-submitter inputs after the gold frontier can be rescanned on later
/// safe-head syncs until a later batch is accepted and moves the accepted
/// cursor forward.
pub(super) fn populate_safe_accepted_batches(
    conn: &Connection,
    protocol: &ProtocolConfig,
) -> Result<()> {
    const PAGE_SIZE: i64 = 256;
    const SELECT_SQL: &str = "SELECT safe_input_index, payload, block_number \
                              FROM safe_inputs \
                              WHERE sender = ?1 AND safe_input_index > ?2 \
                              ORDER BY safe_input_index ASC LIMIT ?3";
    const INSERT_SQL: &str = "INSERT OR IGNORE INTO safe_accepted_batches \
                              (safe_input_index, nonce, first_frame_safe_block, inclusion_block) \
                              VALUES (?1, ?2, ?3, ?4)";

    let latest_accepted = query_latest_safe_accepted_batch(conn)?;
    let mut cursor = latest_accepted
        .map(|row| row.safe_input_index)
        .unwrap_or(-1);
    let mut expected = latest_accepted
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(0);

    loop {
        // Materialize one page before executing any INSERTs. rusqlite's row
        // iterator borrows the prepared statement, so we can't INSERT on the
        // same connection while iterating. Once the page is collected and the
        // statement is dropped, the connection is free for inserts.
        let page: Vec<(i64, Vec<u8>, i64)> = {
            let mut stmt = conn.prepare_cached(SELECT_SQL)?;
            stmt.query_map(
                params![protocol.batch_submitter.as_slice(), cursor, PAGE_SIZE,],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect::<Result<_>>()?
        };

        if page.is_empty() {
            break;
        }
        let page_len = page.len() as i64;

        let mut insert_stmt = conn.prepare_cached(INSERT_SQL)?;
        for (safe_input_index, payload, block_number) in &page {
            cursor = *safe_input_index;
            let input = SafeInputView {
                safe_input_index: i64_to_u64(*safe_input_index),
                sender: protocol.batch_submitter,
                payload: payload.as_slice(),
                inclusion_block: i64_to_u64(*block_number),
            };
            let Some(accepted) = protocol.scheduler_accepts(input, expected) else {
                continue;
            };
            insert_stmt.execute(params![
                u64_to_i64(accepted.safe_input_index),
                u64_to_i64(accepted.nonce),
                u64_to_i64(accepted.first_frame_safe_block),
                u64_to_i64(accepted.inclusion_block),
            ])?;
            expected = expected.saturating_add(1);
        }

        if page_len < PAGE_SIZE {
            break;
        }
    }

    Ok(())
}
