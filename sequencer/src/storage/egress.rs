// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Egress reader: ordered-L2-tx queries used by the WS feed and catch-up replay.
//!
//! Read-only — every method here either pages the `valid_sequenced_l2_txs` view
//! or counts over it. The view encapsulates the exclusion of invalidated batches
//! so callers don't repeat the filter.

use alloy_primitives::Address;
use rusqlite::{Result, params};

use super::Storage;
use super::convert::{i64_to_u64, u64_to_i64, usize_to_i64};
use super::queries::decode_l2_tx_row;
use sequencer_core::l2_tx::SequencedL2Tx;

impl Storage {
    /// Load a page of ordered L2 transactions starting after the given offset.
    /// Returns `(db_offset, tx)` pairs. Callers should track `db_offset` of the
    /// last item as their cursor, not increment a counter.
    pub fn ordered_l2_txs_page_from(
        &mut self,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<(u64, SequencedL2Tx)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        const SQL: &str = "
            SELECT
                s.offset,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN 0 ELSE 1 END AS kind,
                CASE
                    WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.sender
                    WHEN s.safe_input_index IS NOT NULL THEN d.sender
                    ELSE NULL
                END AS sender,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.data ELSE NULL END AS data,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN f.fee  ELSE NULL END AS fee,
                CASE WHEN s.safe_input_index   IS NOT NULL THEN d.payload      ELSE NULL END AS payload,
                CASE WHEN s.safe_input_index   IS NOT NULL THEN d.block_number ELSE NULL END AS block_number
            FROM valid_sequenced_l2_txs s
            LEFT JOIN user_ops u
              ON u.batch_index    = s.batch_index
             AND u.frame_in_batch = s.frame_in_batch
             AND u.pos_in_frame   = s.user_op_pos_in_frame
            LEFT JOIN frames f
              ON f.batch_index    = s.batch_index
             AND f.frame_in_batch = s.frame_in_batch
            LEFT JOIN safe_inputs d
              ON d.safe_input_index = s.safe_input_index
            WHERE s.offset > ?1
            ORDER BY s.offset ASC
            LIMIT ?2
        ";
        let mut stmt = self.conn.prepare_cached(SQL)?;
        let rows = stmt.query_map(params![u64_to_i64(offset), usize_to_i64(limit)], |row| {
            let db_offset: i64 = row.get(0)?;
            let tx = decode_l2_tx_row(
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            );
            Ok((i64_to_u64(db_offset), tx))
        })?;
        rows.collect::<Result<Vec<_>>>()
    }

    /// Returns the maximum offset in `valid_sequenced_l2_txs`, or 0 if empty.
    /// Used as the head cursor for feed subscribers.
    pub fn ordered_l2_tx_head_offset(&mut self) -> Result<u64> {
        let value: Option<i64> = self.conn.query_row(
            "SELECT MAX(offset) FROM valid_sequenced_l2_txs",
            [],
            |row| row.get(0),
        )?;
        Ok(value.map(i64_to_u64).unwrap_or(0))
    }

    /// Count broadcastable events with offset > `from_offset`, capped at `limit`.
    ///
    /// Used for catch-up window checks. Excludes batch-submitter direct inputs
    /// (which are filtered before WS delivery) so the count reflects what the
    /// client actually receives.
    pub fn count_broadcastable_events_after(
        &mut self,
        from_offset: u64,
        limit: u64,
        batch_submitter_address: Option<Address>,
    ) -> Result<u64> {
        if limit == 0 {
            return Ok(0);
        }

        let value: i64 = match batch_submitter_address {
            Some(addr) => {
                const SQL: &str = "
                    SELECT COUNT(*) FROM (
                        SELECT 1 FROM valid_sequenced_l2_txs s
                        WHERE s.offset > ?1
                          AND NOT (s.safe_input_index IS NOT NULL
                              AND EXISTS (SELECT 1 FROM safe_inputs si
                                  WHERE si.safe_input_index = s.safe_input_index
                                    AND si.sender = ?2))
                        LIMIT ?3
                    )";
                self.conn.query_row(
                    SQL,
                    params![u64_to_i64(from_offset), addr.as_slice(), u64_to_i64(limit)],
                    |row| row.get(0),
                )?
            }
            None => {
                const SQL: &str = "
                    SELECT COUNT(*) FROM (
                        SELECT 1 FROM valid_sequenced_l2_txs
                        WHERE offset > ?1
                        LIMIT ?2
                    )";
                self.conn.query_row(
                    SQL,
                    params![u64_to_i64(from_offset), u64_to_i64(limit)],
                    |row| row.get(0),
                )?
            }
        };
        Ok(i64_to_u64(value))
    }
}
