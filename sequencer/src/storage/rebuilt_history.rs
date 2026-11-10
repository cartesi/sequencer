// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Setup's rebuilt history: the transactions the scheduler hands out from the
//! base through the stop block, stored in order before anything executes, then
//! read back in chunks for execution, which records each executed row's count.
//! Setup is the only writer, and only before it publishes the era; the feed
//! reads the executed rows through `history::canonical`.

use alloy_primitives::Address;
use rusqlite::{OptionalExtension, Result, Transaction, params};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
use sequencer_core::scheduler::Scheduled;

use super::Storage;
use super::convert::{i64_to_u16, i64_to_u32, i64_to_u64, saturating_query_bound, u64_to_i64};

/// Where a rebuilt row sits: the input whose processing scheduled it, and its
/// place among that input's transactions. Rows order by this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RebuiltKey {
    pub l1_input_index: u64,
    pub position: u64,
}

/// One stored transaction, as execution reads it back: a direct carries the
/// input-table fields the scheduler handed out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuiltRow {
    pub key: RebuiltKey,
    pub batch_nonce: u64,
    pub tx: L2Tx,
}

const DIRECT_KIND: &str = "direct";
const USER_OP_KIND: &str = "user_op";

impl Storage {
    /// Append scheduled transactions in the order the scheduler handed them
    /// out, each with the index of the input whose processing scheduled it,
    /// in one transaction. Storage numbers the positions within each input,
    /// so one input's transactions may span several calls.
    pub fn append_rebuilt_history(&mut self, rows: &[(u64, Scheduled)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            let mut last = last_rebuilt_key_in(tx)?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO rebuilt_history \
                 (l1_input_index, position, kind, safe_input_index, sender, nonce, fee, \
                  data, safe_block, batch_nonce) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for (l1_input_index, scheduled) in rows {
                let key = next_key(last, *l1_input_index);
                let batch_nonce = u64_to_i64(scheduled.batch_nonce);
                match &scheduled.tx {
                    L2Tx::Direct(direct) => insert.execute(params![
                        u64_to_i64(key.l1_input_index),
                        u64_to_i64(key.position),
                        DIRECT_KIND,
                        u64_to_i64(direct.input_index),
                        None::<Vec<u8>>,
                        None::<i64>,
                        None::<i64>,
                        None::<Vec<u8>>,
                        None::<i64>,
                        batch_nonce,
                    ])?,
                    L2Tx::UserOp(op) => insert.execute(params![
                        u64_to_i64(key.l1_input_index),
                        u64_to_i64(key.position),
                        USER_OP_KIND,
                        None::<i64>,
                        op.sender.as_slice(),
                        i64::from(op.nonce),
                        i64::from(op.fee),
                        op.data.as_slice(),
                        u64_to_i64(op.block),
                        batch_nonce,
                    ])?,
                };
                last = Some(key);
            }
            Ok(())
        })
    }

    /// Up to `limit` rows after `after` (from the first row when `None`), in
    /// order, for execution. A direct is read back from the input table.
    pub fn rebuilt_history_after(
        &mut self,
        after: Option<RebuiltKey>,
        limit: usize,
    ) -> Result<Vec<RebuiltRow>> {
        let (input, position) = after.map_or((-1, -1), |key| {
            (u64_to_i64(key.l1_input_index), u64_to_i64(key.position))
        });
        let limit = saturating_query_bound(u64::try_from(limit).expect("limit fits u64"));
        self.read(|tx| {
            let mut statement = tx.prepare_cached(
                "SELECT r.l1_input_index, r.position, r.kind, r.batch_nonce, \
                        r.sender, r.nonce, r.fee, r.data, r.safe_block, \
                        d.safe_input_index, d.sender, d.payload, d.block_number, d.block_timestamp \
                 FROM rebuilt_history r \
                 LEFT JOIN safe_inputs d ON d.safe_input_index = r.safe_input_index \
                 WHERE (r.l1_input_index, r.position) > (?1, ?2) \
                 ORDER BY r.l1_input_index, r.position LIMIT ?3",
            )?;
            let rows = statement.query_map(params![input, position, limit], |row| {
                let key = RebuiltKey {
                    l1_input_index: i64_to_u64(row.get(0)?),
                    position: i64_to_u64(row.get(1)?),
                };
                let tx = match row.get::<_, String>(2)?.as_str() {
                    DIRECT_KIND => L2Tx::Direct(Direct {
                        input_index: i64_to_u64(row.get(9)?),
                        sender: Address::from_slice(&row.get::<_, Vec<u8>>(10)?),
                        block: i64_to_u64(row.get(12)?),
                        timestamp: i64_to_u64(row.get(13)?),
                        payload: row.get(11)?,
                    }),
                    USER_OP_KIND => L2Tx::UserOp(UserOpTx {
                        sender: Address::from_slice(&row.get::<_, Vec<u8>>(4)?),
                        nonce: i64_to_u32(row.get(5)?),
                        fee: i64_to_u16(row.get(6)?),
                        block: i64_to_u64(row.get(8)?),
                        data: row.get(7)?,
                    }),
                    kind => panic!("invalid rebuilt history kind {kind}"),
                };
                Ok(RebuiltRow {
                    key,
                    batch_nonce: i64_to_u64(row.get(3)?),
                    tx,
                })
            })?;
            rows.collect()
        })
    }

    /// Record the counts of executed rows, in execution order, in one
    /// transaction. A skipped row gets none; the schema refuses a count out
    /// of row order, a gap, or a second count for a row.
    pub fn record_rebuilt_counts(
        &mut self,
        counts: &[(RebuiltKey, ExecutedInputCount)],
    ) -> Result<()> {
        if counts.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            let mut update = tx.prepare_cached(
                "UPDATE rebuilt_history SET l2_tx_count = ?3 \
                 WHERE l1_input_index = ?1 AND position = ?2",
            )?;
            for (key, count) in counts {
                let changed = update.execute(params![
                    u64_to_i64(key.l1_input_index),
                    u64_to_i64(key.position),
                    u64_to_i64(count.get()),
                ])?;
                if changed != 1 {
                    return Err(rusqlite::Error::StatementChangedRows(changed));
                }
            }
            Ok(())
        })
    }
}

fn last_rebuilt_key_in(tx: &Transaction<'_>) -> Result<Option<RebuiltKey>> {
    tx.query_row(
        "SELECT l1_input_index, position FROM rebuilt_history \
         ORDER BY l1_input_index DESC, position DESC LIMIT 1",
        [],
        |row| {
            Ok(RebuiltKey {
                l1_input_index: i64_to_u64(row.get(0)?),
                position: i64_to_u64(row.get(1)?),
            })
        },
    )
    .optional()
}

/// The key after `last` for a row of `l1_input_index`: the next position of
/// the same input, or position 0 of a later one. An earlier input is left to
/// the schema, which refuses it.
fn next_key(last: Option<RebuiltKey>, l1_input_index: u64) -> RebuiltKey {
    let position = match last {
        Some(last) if last.l1_input_index == l1_input_index => last
            .position
            .checked_add(1)
            .expect("rebuilt position overflow"),
        _ => 0,
    };
    RebuiltKey {
        l1_input_index,
        position,
    }
}

#[cfg(test)]
mod tests;
