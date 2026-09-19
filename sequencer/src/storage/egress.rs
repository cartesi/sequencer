// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Application replay entries shared by catch-up and consumer history reads.

use alloy_primitives::{Address, B256};
use rusqlite::{Result, Row};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};

use super::convert::{i64_to_u16, i64_to_u32, i64_to_u64};

mod canonical;
pub(crate) use canonical::HistoryReadError;

#[derive(Debug, Clone)]
pub(crate) enum L2TxContext {
    UserOp {
        tx: ValidUserOp,
        nonce: u32,
        safe_block: u64,
        batch_nonce: u64,
    },
    DirectInput {
        tx: DirectInput,
        input_index: u64,
        batch_nonce: u64,
        block_timestamp: u64,
        transaction_hash: B256,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ApplicationInputRow {
    pub(crate) offset: ExecutedInputCount,
    pub(crate) context: L2TxContext,
}

fn decode_application_input(row: &Row<'_>) -> Result<ApplicationInputRow> {
    let sender = Address::from_slice(row.get::<_, Vec<u8>>(2)?.as_slice());
    let safe_block = i64_to_u64(row.get(7)?);
    let batch_nonce = i64_to_u64(row.get(8)?);
    let context = match row.get::<_, i64>(1)? {
        0 => L2TxContext::UserOp {
            tx: ValidUserOp {
                sender,
                data: row.get(3)?,
                fee: i64_to_u16(row.get(4)?),
            },
            nonce: i64_to_u32(row.get(10)?),
            safe_block,
            batch_nonce,
        },
        1 => L2TxContext::DirectInput {
            tx: DirectInput {
                sender,
                payload: row.get(5)?,
                block_number: i64_to_u64(row.get(6)?),
            },
            input_index: i64_to_u64(row.get(9)?),
            batch_nonce,
            block_timestamp: i64_to_u64(row.get(11)?),
            transaction_hash: B256::from_slice(row.get::<_, Vec<u8>>(12)?.as_slice()),
        },
        kind => panic!("invalid application input kind {kind}"),
    };
    Ok(ApplicationInputRow {
        offset: ExecutedInputCount::new(i64_to_u64(row.get(0)?)),
        context,
    })
}
