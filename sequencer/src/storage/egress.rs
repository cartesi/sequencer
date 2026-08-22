// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Egress reader: ordered-L2-tx queries used by the WS feed and catch-up replay.
//!
//! Read-only — every method here either pages the `valid_sequenced_l2_txs` view
//! or counts over it. The view encapsulates the exclusion of invalidated batches
//! so callers don't repeat the filter.

use alloy_primitives::{Address, B256};
use rusqlite::{Result, params};

use super::Storage;
use super::convert::{i64_to_u32, i64_to_u64, saturating_query_bound};
use super::queries::decode_l2_tx_row;
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};

/// One persisted L2 transaction and the ordering context of its covering frame.
#[derive(Debug, Clone)]
pub(crate) enum OrderedL2TxRow {
    UserOp {
        offset: u64,
        tx: ValidUserOp,
        nonce: u32,
        safe_block: u64,
        batch_nonce: u64,
        executed_input_offset: Option<ExecutedInputCount>,
    },
    DirectInput {
        offset: u64,
        tx: DirectInput,
        input_index: u64,
        safe_block: u64,
        batch_nonce: u64,
        block_timestamp: u64,
        transaction_hash: B256,
        executed_input_offset: Option<ExecutedInputCount>,
    },
}

impl OrderedL2TxRow {
    pub(crate) fn offset(&self) -> u64 {
        match self {
            Self::UserOp { offset, .. } | Self::DirectInput { offset, .. } => *offset,
        }
    }

    fn into_replay_row(self) -> ReplayL2TxRow {
        match self {
            Self::UserOp {
                offset,
                tx,
                safe_block,
                executed_input_offset,
                ..
            } => ReplayL2TxRow {
                db_offset: offset,
                tx: SequencedL2Tx::UserOp(tx),
                frame_safe_block: safe_block,
                executed_input_offset,
            },
            Self::DirectInput {
                offset,
                tx,
                safe_block,
                executed_input_offset,
                ..
            } => ReplayL2TxRow {
                db_offset: offset,
                tx: SequencedL2Tx::Direct(tx),
                frame_safe_block: safe_block,
                executed_input_offset,
            },
        }
    }
}

/// One valid physical replay row with its canonical application attribution.
///
/// The physical SQLite cursor and logical application offset are deliberately
/// named: they are different coordinates and callers must not infer their
/// meaning from tuple position.
#[derive(Debug, Clone)]
pub(crate) struct ReplayL2TxRow {
    pub(crate) db_offset: u64,
    pub(crate) tx: SequencedL2Tx,
    pub(crate) frame_safe_block: u64,
    pub(crate) executed_input_offset: Option<ExecutedInputCount>,
}

impl Storage {
    /// Load a page of ordered L2 transactions starting after the given offset.
    /// Each row names both its physical database cursor and optional logical
    /// application offset. `frame_safe_block` is fed to user-op replay so the
    /// app clock advances exactly as it did live (directs use their own block
    /// number). `executed_input_offset` is `None` only for a physical row that
    /// does not execute in the application. Callers advance with `db_offset`
    /// rather than incrementing either coordinate.
    pub(crate) fn ordered_l2_txs_page_from(
        &mut self,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<ReplayL2TxRow>> {
        self.ordered_l2_tx_rows_page_from(offset, limit)
            .map(|rows| {
                rows.into_iter()
                    .map(OrderedL2TxRow::into_replay_row)
                    .collect()
            })
    }

    /// Load feed rows with all persisted execution and settlement context.
    pub(crate) fn ordered_l2_tx_rows_page_from(
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
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.block_timestamp  ELSE NULL END AS block_timestamp,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.transaction_hash ELSE NULL END AS transaction_hash,
                e.executed_input_offset
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
            LEFT JOIN executed_inputs e
              ON e.sequenced_l2_tx_offset = s.offset
            WHERE s.offset > ?1
            ORDER BY s.offset ASC
            LIMIT ?2
        ";
        let mut stmt = self.conn.prepare_cached(SQL)?;
        let limit = u64::try_from(limit).unwrap_or(u64::MAX);
        // Query bounds saturate by design: `offset` can be a client-supplied
        // WS cursor and `limit` is config-sourced (see `saturating_query_bound`).
        let rows = stmt.query_map(
            params![
                saturating_query_bound(offset),
                saturating_query_bound(limit)
            ],
            |row| {
                let db_offset: i64 = row.get(0)?;
                let tx = decode_l2_tx_row(
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                );
                // Non-NULL for every sequenced row: batches and frames exist before
                // anything can be sequenced into them.
                let safe_block = i64_to_u64(row.get(7)?);
                let batch_nonce = i64_to_u64(row.get(8)?);
                let executed_input_offset = row
                    .get::<_, Option<i64>>(13)?
                    .map(|value| ExecutedInputCount::new(i64_to_u64(value)));
                match tx {
                    SequencedL2Tx::UserOp(tx) => Ok(OrderedL2TxRow::UserOp {
                        offset: i64_to_u64(db_offset),
                        tx,
                        nonce: i64_to_u32(row.get(10)?),
                        safe_block,
                        batch_nonce,
                        executed_input_offset,
                    }),
                    SequencedL2Tx::Direct(tx) => Ok(OrderedL2TxRow::DirectInput {
                        offset: i64_to_u64(db_offset),
                        tx,
                        input_index: i64_to_u64(row.get(9)?),
                        safe_block,
                        batch_nonce,
                        block_timestamp: i64_to_u64(row.get(11)?),
                        transaction_hash: B256::from_slice(row.get::<_, Vec<u8>>(12)?.as_slice()),
                        executed_input_offset,
                    }),
                }
            },
        )?;
        rows.collect::<Result<Vec<_>>>()
    }

    /// Returns the maximum offset in `valid_sequenced_l2_txs`, or 0 if empty.
    /// Used as the head cursor for feed subscribers. Shares the single
    /// `valid_ordered_l2_tx_head` reader with the snapshot batch-close path,
    /// so the feed cursor and the snapshot replay cursor can't drift.
    pub fn ordered_l2_tx_head_offset(&mut self) -> Result<u64> {
        super::queries::valid_ordered_l2_tx_head(&self.conn)
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
                    params![
                        saturating_query_bound(from_offset),
                        addr.as_slice(),
                        saturating_query_bound(limit)
                    ],
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
                    params![
                        saturating_query_bound(from_offset),
                        saturating_query_bound(limit)
                    ],
                    |row| row.get(0),
                )?
            }
        };
        Ok(i64_to_u64(value))
    }
}
