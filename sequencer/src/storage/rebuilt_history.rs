// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Setup's rebuilt history: the transactions that executed from the base
//! through the stop block, each written once with its L2 tx count. Setup is
//! the only writer, and only before it publishes the era; the feed reads the
//! rows through `history::canonical`.

use rusqlite::{Result, named_params, params};
use sequencer_core::history::L2TxCount;
use sequencer_core::l2_tx::L2Tx;
use sequencer_core::scheduler::Scheduled;

use super::convert::{i64_to_u64, saturating_query_bound, u64_to_i64};
use super::queries::{DIRECT_EXECUTES_SQL, DirectOperands};
use super::{SafeInputRange, Storage};

/// The rebuilt directs, counted against the input table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuiltDirects {
    /// Every direct the rebuilt history holds.
    pub stored: u64,
    /// Those that are inputs of the range and execute as directs.
    pub admitted_in_range: u64,
}

impl Storage {
    /// Insert executed transactions with their counts, in execution order,
    /// in one transaction. A direct stores only its input index.
    pub fn append_rebuilt_history(&mut self, rows: &[(L2TxCount, Scheduled)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            let mut insert = tx.prepare_cached(
                "INSERT INTO rebuilt_history \
                 (l2_tx_count, batch_nonce, safe_input_index, sender, nonce, fee, safe_block, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (count, scheduled) in rows {
                let count = u64_to_i64(count.get());
                let batch_nonce = u64_to_i64(scheduled.batch_nonce);
                match &scheduled.tx {
                    L2Tx::Direct(direct) => insert.execute(params![
                        count,
                        batch_nonce,
                        u64_to_i64(direct.input_index),
                        None::<Vec<u8>>,
                        None::<i64>,
                        None::<i64>,
                        None::<i64>,
                        None::<Vec<u8>>,
                    ])?,
                    L2Tx::UserOp(op) => insert.execute(params![
                        count,
                        batch_nonce,
                        None::<i64>,
                        op.sender.as_slice(),
                        i64::from(op.nonce),
                        i64::from(op.fee),
                        u64_to_i64(op.block),
                        op.data.as_slice(),
                    ])?,
                };
            }
            Ok(())
        })
    }

    /// The directs the rebuilt history holds, and how many of them are inputs
    /// of `range` that execute as directs by the predicate every drain uses.
    /// Setup checks both against the admitted inputs of the range it drained.
    pub fn rebuilt_directs(&mut self, range: SafeInputRange) -> Result<RebuiltDirects> {
        self.read(|tx| {
            let operands = DirectOperands::load(tx)?;
            let stored: i64 = tx.query_row(
                "SELECT COUNT(*) FROM rebuilt_history WHERE safe_input_index IS NOT NULL",
                [],
                |row| row.get(0),
            )?;
            let admitted_in_range: i64 = tx
                .prepare_cached(&format!(
                    "SELECT COUNT(*) FROM safe_inputs \
                     WHERE safe_input_index >= :start AND safe_input_index < :end \
                       AND {DIRECT_EXECUTES_SQL} \
                       AND safe_input_index IN (SELECT safe_input_index FROM rebuilt_history \
                                                 WHERE safe_input_index IS NOT NULL)"
                ))?
                .query_row(
                    operands
                        .bind(named_params! {
                            ":start": saturating_query_bound(range.start()),
                            ":end": saturating_query_bound(range.end()),
                        })
                        .as_slice(),
                    |row| row.get(0),
                )?;
            Ok(RebuiltDirects {
                stored: i64_to_u64(stored),
                admitted_in_range: i64_to_u64(admitted_in_range),
            })
        })
    }
}

#[cfg(test)]
mod tests;
