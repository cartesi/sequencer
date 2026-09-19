// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Version-checked canonical pages, read with their boundaries in one transaction.

use rusqlite::{Connection, Transaction, params};
use sequencer_core::history::{
    ExecutedInputCount, HistoryBounds, HistoryClaim, HistoryPolicyError,
};

use super::{ApplicationInputRow, decode_application_input};
use crate::storage::Storage;
use crate::storage::convert::{saturating_query_bound, u64_to_i64};
use crate::storage::history::{next_executed_input_count_in, query_history_state};

#[derive(Debug)]
pub(crate) struct CanonicalHistoryPage {
    pub(crate) bounds: HistoryBounds,
    pub(crate) rows: Vec<ApplicationInputRow>,
    pub(crate) next_input: ExecutedInputCount,
}

impl CanonicalHistoryPage {
    pub(crate) fn next_claim(&self) -> HistoryClaim {
        HistoryClaim {
            version: self.bounds.version,
            next_input: self.next_input,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoryReadError {
    #[error(transparent)]
    Policy(#[from] HistoryPolicyError),
    #[error("reading canonical history: {0}")]
    Storage(#[from] rusqlite::Error),
}

impl Storage {
    pub(crate) fn history_bounds(&mut self) -> rusqlite::Result<HistoryBounds> {
        self.read(|tx| history_bounds_in(tx))
    }

    /// Validate before reading inputs, including for empty pages. An accepted
    /// count at the head waits; a missing row before that head is an invariant fault.
    pub(crate) fn canonical_history_page(
        &mut self,
        claim: HistoryClaim,
        limit: usize,
    ) -> Result<CanonicalHistoryPage, HistoryReadError> {
        self.read(|tx| {
            let bounds = history_bounds_in(tx)?;
            match bounds.validate(claim) {
                Ok(()) => canonical_page_in(tx, bounds, claim.next_input, limit).map(Ok),
                Err(error) => Ok(Err(error)),
            }
        })?
        .map_err(HistoryReadError::Policy)
    }
}

fn history_bounds_in(conn: &Connection) -> rusqlite::Result<HistoryBounds> {
    let state = query_history_state(conn)?;
    let available_from = ExecutedInputCount::new(state.base_executed_input_count);
    Ok(HistoryBounds {
        version: state.version,
        available_from,
        head: next_executed_input_count_in(conn)?,
    })
}

fn canonical_page_in(
    tx: &Transaction<'_>,
    bounds: HistoryBounds,
    from: ExecutedInputCount,
    limit: usize,
) -> rusqlite::Result<CanonicalHistoryPage> {
    let limit = u64::try_from(limit).expect("page size fits u64");
    let expected_len = limit.min(bounds.head.get() - from.get());
    let mut rows = Vec::new();
    // H can be i64::MAX + 1, the boundary after the last representable row.
    // An empty-at-head request must never clamp back onto that last input.
    if expected_len > 0 {
        const SQL: &str = "
            SELECT
                s.offset,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN 0 ELSE 1 END,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.sender ELSE d.sender END,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.data ELSE NULL END,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN f.fee ELSE NULL END,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.payload ELSE NULL END,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.block_number ELSE NULL END,
                f.safe_block,
                b.nonce,
                s.safe_input_index,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.nonce ELSE NULL END,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.block_timestamp ELSE NULL END,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.transaction_hash ELSE NULL END
            FROM application_inputs s
            LEFT JOIN user_ops u
              ON u.batch_index = s.batch_index
             AND u.frame_in_batch = s.frame_in_batch
             AND u.pos_in_frame = s.user_op_pos_in_frame
            LEFT JOIN frames f
              ON f.batch_index = s.batch_index AND f.frame_in_batch = s.frame_in_batch
            LEFT JOIN safe_inputs d ON d.safe_input_index = s.safe_input_index
            LEFT JOIN batches b ON b.batch_index = s.batch_index
            WHERE s.offset >= ?1
            ORDER BY s.offset
            LIMIT ?2
        ";
        let mut stmt = tx.prepare_cached(SQL)?;
        let mapped = stmt.query_map(
            params![u64_to_i64(from.get()), saturating_query_bound(expected_len)],
            decode_application_input,
        )?;
        let mut expected = from;
        for row in mapped {
            let row = row?;
            assert_eq!(
                row.offset, expected,
                "canonical history page has an attribution gap"
            );
            rows.push(row);
            expected = expected
                .checked_next()
                .expect("canonical input count overflow");
        }
        assert_eq!(
            rows.len() as u64,
            expected_len,
            "canonical history page ended before its recorded head"
        );
    }
    Ok(CanonicalHistoryPage {
        bounds,
        next_input: from
            .checked_add(expected_len)
            .expect("page ends at or before head"),
        rows,
    })
}

#[cfg(test)]
mod tests;
