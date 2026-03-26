// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::l2_tx::SequencedL2Tx;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BroadcastTxMessage {
    UserOp {
        offset: u64,
        sender: String,
        /// Log-space fee exponent (base 129/128). See [`crate::fee`].
        fee: u16,
        data: String,
    },
    DirectInput {
        offset: u64,
        sender: String,
        block_number: u64,
        payload: String,
    },
}

impl BroadcastTxMessage {
    pub fn offset(&self) -> u64 {
        match self {
            Self::UserOp { offset, .. } => *offset,
            Self::DirectInput { offset, .. } => *offset,
        }
    }

    pub fn from_offset_and_tx(offset: u64, tx: SequencedL2Tx) -> Self {
        match tx {
            SequencedL2Tx::UserOp(user_op) => Self::UserOp {
                offset,
                sender: user_op.sender.to_string(),
                fee: user_op.fee,
                data: alloy_primitives::hex::encode_prefixed(user_op.data.as_slice()),
            },
            SequencedL2Tx::Direct(direct) => Self::DirectInput {
                offset,
                sender: direct.sender.to_string(),
                block_number: direct.block_number,
                payload: alloy_primitives::hex::encode_prefixed(direct.payload.as_slice()),
            },
        }
    }
}
