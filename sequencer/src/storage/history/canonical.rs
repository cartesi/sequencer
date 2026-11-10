// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Canonical application history: version-checked pages read with their
//! boundaries in one transaction. The era's history is setup's rebuilt rows,
//! every one executed, up to the rebuilt history's end, then the live rows; a
//! page may span the seam. Native restart catch-up and the WS feed read it.

use alloy_primitives::{Address, B256};
use rusqlite::{Connection, Row, Transaction, params};
use sequencer_core::history::{
    HistoryBounds, HistoryClaim, HistoryPolicyError, HistoryVersion, L2TxCount,
};
use sequencer_core::l2_tx::{Direct, UserOpTx};

use crate::storage::Storage;
use crate::storage::convert::{
    i64_to_u16, i64_to_u32, i64_to_u64, saturating_query_bound, u64_to_i64,
};
use crate::storage::history::{Era, era_in, history_version_in, next_l2_tx_count_of};

/// One executed history row: the record the executor ran, plus the batch
/// coordinates the feed reports beside it.
#[derive(Debug, Clone)]
pub(crate) enum L2TxContext {
    UserOp {
        tx: UserOpTx,
        batch_nonce: u64,
    },
    Direct {
        tx: Direct,
        batch_nonce: u64,
        transaction_hash: B256,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ApplicationInputRow {
    pub(crate) offset: L2TxCount,
    pub(crate) context: L2TxContext,
}

fn decode_application_input(row: &Row<'_>) -> rusqlite::Result<ApplicationInputRow> {
    let sender = Address::from_slice(row.get::<_, Vec<u8>>(2)?.as_slice());
    let batch_nonce = i64_to_u64(row.get(8)?);
    let context = match row.get::<_, i64>(1)? {
        0 => L2TxContext::UserOp {
            tx: UserOpTx {
                sender,
                nonce: i64_to_u32(row.get(10)?),
                fee: i64_to_u16(row.get(4)?),
                block: i64_to_u64(row.get(7)?),
                data: row.get(3)?,
            },
            batch_nonce,
        },
        1 => L2TxContext::Direct {
            tx: Direct {
                input_index: i64_to_u64(row.get(9)?),
                sender,
                block: i64_to_u64(row.get(6)?),
                timestamp: i64_to_u64(row.get(11)?),
                payload: row.get(5)?,
            },
            batch_nonce,
            transaction_hash: B256::from_slice(row.get::<_, Vec<u8>>(12)?.as_slice()),
        },
        kind => panic!("invalid application input kind {kind}"),
    };
    Ok(ApplicationInputRow {
        offset: L2TxCount::new(i64_to_u64(row.get(0)?)),
        context,
    })
}

#[derive(Debug)]
pub(crate) struct CanonicalHistoryPage {
    pub(crate) bounds: HistoryBounds,
    pub(crate) rows: Vec<ApplicationInputRow>,
    pub(crate) next_input: L2TxCount,
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
            let era = era_in(tx)?;
            let bounds = history_bounds_from(tx, &era, history_version_in(tx)?)?;
            match bounds.validate(claim) {
                Ok(()) => canonical_page_in(tx, &era, bounds, claim.next_input, limit).map(Ok),
                Err(error) => Ok(Err(error)),
            }
        })?
        .map_err(HistoryReadError::Policy)
    }
}

fn history_bounds_in(conn: &Connection) -> rusqlite::Result<HistoryBounds> {
    history_bounds_from(conn, &era_in(conn)?, history_version_in(conn)?)
}

fn history_bounds_from(
    conn: &Connection,
    era: &Era,
    version: HistoryVersion,
) -> rusqlite::Result<HistoryBounds> {
    Ok(HistoryBounds {
        version,
        available_from: era.base,
        head: next_l2_tx_count_of(conn, era)?,
    })
}

const REBUILT_PAGE_SQL: &str = "
    SELECT
        r.l2_tx_count,
        CASE WHEN r.safe_input_index IS NULL THEN 0 ELSE 1 END,
        CASE WHEN r.safe_input_index IS NULL THEN r.sender ELSE d.sender END,
        r.data,
        r.fee,
        d.payload,
        d.block_number,
        r.safe_block,
        r.batch_nonce,
        r.safe_input_index,
        r.nonce,
        d.block_timestamp,
        d.transaction_hash
    FROM rebuilt_history r
    LEFT JOIN safe_inputs d ON d.safe_input_index = r.safe_input_index
    WHERE r.l2_tx_count >= ?1
    ORDER BY r.l2_tx_count
    LIMIT ?2
";

const LIVE_PAGE_SQL: &str = "
    SELECT
        s.l2_tx_count,
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
    WHERE s.l2_tx_count >= ?1
    ORDER BY s.l2_tx_count
    LIMIT ?2
";

fn canonical_page_in(
    tx: &Transaction<'_>,
    era: &Era,
    bounds: HistoryBounds,
    from: L2TxCount,
    limit: usize,
) -> rusqlite::Result<CanonicalHistoryPage> {
    let limit = u64::try_from(limit).expect("page size fits u64");
    let expected_len = limit.min(bounds.head.get() - from.get());
    let rebuilt_end = era.rebuilt_end;
    let mut rows = Vec::new();
    let mut expected = from;
    // H can be i64::MAX + 1, the boundary after the last representable row.
    // An empty-at-head request must never clamp back onto that last input.
    if expected_len > 0 {
        // The rebuilt rows cover [base, rebuilt_end) without gaps,
        // and the live rows start at rebuilt_end, so one cursor spans both.
        let rebuilt_len = expected_len.min(rebuilt_end.get().saturating_sub(from.get()));
        for (sql, start, len) in [
            (REBUILT_PAGE_SQL, from, rebuilt_len),
            (
                LIVE_PAGE_SQL,
                from.max(rebuilt_end),
                expected_len - rebuilt_len,
            ),
        ] {
            if len == 0 {
                continue;
            }
            let mut stmt = tx.prepare_cached(sql)?;
            let mapped = stmt.query_map(
                params![u64_to_i64(start.get()), saturating_query_bound(len)],
                decode_application_input,
            )?;
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
