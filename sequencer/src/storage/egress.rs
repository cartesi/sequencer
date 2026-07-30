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

/// One ordered L2 tx with its execution context, see
/// [`Storage::ordered_l2_txs_page_from`].
pub struct OrderedL2TxRow {
    pub offset: u64,
    pub tx: SequencedL2Tx,
    /// The covering frame's `safe_block`, what the lane passed live to user-op
    /// execution (directs use their own `block_number` instead).
    pub safe_block: u64,
    /// Nonce of the batch the covering frame belongs to.
    pub batch_nonce: u64,
    /// The direct input's L1 input index, `None` for user ops.
    pub input_index: Option<u64>,
    /// The user op's own signed nonce, `None` for directs.
    pub op_nonce: Option<u32>,
    /// The direct input's L1 block timestamp, `None` for user ops.
    pub block_timestamp: Option<u64>,
    /// The direct input's L1 transaction hash, `None` for user ops.
    pub transaction_hash: Option<alloy_primitives::B256>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutedInputCountPosition {
    pub minimum_executed_input_count: u64,
    pub from_offset: u64,
    pub remaining_to_skip: u64,
}

impl Storage {
    /// Load a page of ordered L2 transactions starting after the given offset.
    /// Catch-up replay feeds `safe_block` to `execute_valid_user_op` so the
    /// app's safe-block clock advances exactly as it did live; the feed
    /// forwards the full execution context to subscribers. Callers should
    /// track `offset` of the last item as their cursor, not increment a
    /// counter.
    pub fn ordered_l2_txs_page_from(
        &mut self,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<OrderedL2TxRow>> {
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
                CASE WHEN s.safe_input_index   IS NOT NULL THEN d.block_number ELSE NULL END AS block_number,
                f.safe_block,
                b.nonce,
                s.safe_input_index,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.nonce ELSE NULL END AS op_nonce,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.block_timestamp   ELSE NULL END AS block_timestamp,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.transaction_hash  ELSE NULL END AS transaction_hash
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
            LEFT JOIN batches b
              ON b.batch_index = s.batch_index
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
            // Non-NULL for every sequenced row: the frames row is inserted
            // when the frame opens, before anything is sequenced into it,
            // and the batches row exists before its frames.
            let frame_safe_block: i64 = row.get(7)?;
            let batch_nonce: i64 = row.get(8)?;
            let input_index: Option<i64> = row.get(9)?;
            let op_nonce: Option<i64> = row.get(10)?;
            let block_timestamp: Option<i64> = row.get(11)?;
            let transaction_hash: Option<Vec<u8>> = row.get(12)?;
            Ok(OrderedL2TxRow {
                offset: i64_to_u64(db_offset),
                tx,
                safe_block: i64_to_u64(frame_safe_block),
                batch_nonce: i64_to_u64(batch_nonce),
                input_index: input_index.map(i64_to_u64),
                op_nonce: op_nonce.map(|value| {
                    u32::try_from(value).expect("user op nonces are u32-checked at insert")
                }),
                block_timestamp: block_timestamp.map(i64_to_u64),
                transaction_hash: transaction_hash
                    .map(|bytes| alloy_primitives::B256::from_slice(bytes.as_slice())),
            })
        })?;
        rows.collect::<Result<Vec<_>>>()
    }

    /// Returns the maximum offset in `valid_sequenced_l2_txs`, or 0 if empty.
    /// Used as the head cursor for feed subscribers. Shares the single
    /// `valid_ordered_l2_tx_head` reader with the snapshot batch-close path,
    /// so the feed cursor and the snapshot replay cursor can't drift.
    pub fn ordered_l2_tx_head_offset(&mut self) -> Result<u64> {
        super::queries::valid_ordered_l2_tx_head(&self.conn)
    }

    /// Resolve an application's already-executed input count to the feed's
    /// sparse exclusive cursor.
    ///
    /// Batch boundaries carry absolute logical counts, so lookup touches only
    /// the containing batch. Invalidated branches retain their rows but vanish
    /// from `valid_*`; a recovery branch structurally reuses the same counts.
    /// A future count resolves to the current head plus a number of future
    /// application events for the subscriber worker to skip.
    pub fn resolve_executed_input_count(
        &mut self,
        requested: u64,
    ) -> Result<ExecutedInputCountPosition> {
        self.read(|tx| {
            let (minimum, root_start, anchor_offset): (i64, i64, i64) = tx.query_row(
                "SELECT minimum_executed_input_count, \
                        root_executed_input_count_before, \
                        l2_tx_offset \
                 FROM l2_feed_anchor WHERE singleton_id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let minimum = i64_to_u64(minimum);
            let root_start = i64_to_u64(root_start);
            let anchor_offset = i64_to_u64(anchor_offset);
            if requested < minimum {
                return Ok(ExecutedInputCountPosition {
                    minimum_executed_input_count: minimum,
                    from_offset: anchor_offset,
                    remaining_to_skip: 0,
                });
            }

            let (tip_start, tip_inputs): (i64, i64) = tx.query_row(
                "SELECT b.executed_input_count_before, COUNT(app_tx.offset) \
                 FROM valid_open_batch b \
                 LEFT JOIN valid_application_l2_txs app_tx \
                   ON app_tx.batch_index = b.batch_index",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let current = i64_to_u64(tip_start)
                .checked_add(i64_to_u64(tip_inputs))
                .expect("executed input count overflow");
            let head = super::queries::valid_ordered_l2_tx_head(tx)?;

            if requested >= current {
                return Ok(ExecutedInputCountPosition {
                    minimum_executed_input_count: minimum,
                    from_offset: head,
                    remaining_to_skip: requested - current,
                });
            }
            if requested == minimum && root_start == minimum {
                return Ok(ExecutedInputCountPosition {
                    minimum_executed_input_count: minimum,
                    from_offset: anchor_offset,
                    remaining_to_skip: 0,
                });
            }

            let (batch_index, batch_start): (i64, i64) = tx.query_row(
                "SELECT batch_index, executed_input_count_before \
                 FROM valid_batches \
                 WHERE executed_input_count_before < ?1 \
                 ORDER BY executed_input_count_before DESC, batch_index DESC \
                 LIMIT 1",
                params![u64_to_i64(requested)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let ordinal_in_batch = requested
                .checked_sub(i64_to_u64(batch_start))
                .expect("selected batch starts before requested input count");
            let offset: i64 = tx.query_row(
                "SELECT offset \
                 FROM valid_application_l2_txs \
                 WHERE batch_index = ?1 \
                 ORDER BY offset ASC \
                 LIMIT 1 OFFSET ?2",
                params![
                    batch_index,
                    u64_to_i64(
                        ordinal_in_batch
                            .checked_sub(1)
                            .expect("batch-local input ordinal is one-based")
                    )
                ],
                |row| row.get(0),
            )?;
            Ok(ExecutedInputCountPosition {
                minimum_executed_input_count: minimum,
                from_offset: i64_to_u64(offset),
                remaining_to_skip: 0,
            })
        })
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
