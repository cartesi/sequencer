// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::l2_tx::{DirectInput, ValidUserOp};
use serde::{Deserialize, Serialize};

/// One transaction from the sequencer's replay-then-live feed.
///
/// In addition to application inputs, each variant carries enough persisted
/// ordering context for a subscriber to rebuild the soft suffix and reconcile
/// it with L1 settlement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BroadcastTxMessage {
    UserOp {
        offset: u64,
        sender: String,
        /// Signed replay-protection nonce of the user operation.
        nonce: u32,
        /// Log-space fee exponent (base 129/128). See [`crate::fee`].
        fee: u16,
        data: String,
        /// Safe L1 block committed by the covering frame.
        safe_block: u64,
        /// Nonce of the batch containing the covering frame.
        batch_nonce: u64,
    },
    DirectInput {
        offset: u64,
        sender: String,
        block_number: u64,
        payload: String,
        /// Per-application InputBox index of this direct input.
        input_index: u64,
        /// Nonce of the batch that drained this direct input.
        batch_nonce: u64,
        /// Unix timestamp, in seconds, of the L1 block containing this direct input.
        block_timestamp: u64,
        /// Hash of the L1 transaction that carried this direct input.
        transaction_hash: String,
    },
}

impl BroadcastTxMessage {
    pub fn offset(&self) -> u64 {
        match self {
            Self::UserOp { offset, .. } => *offset,
            Self::DirectInput { offset, .. } => *offset,
        }
    }

    pub fn from_user_op(
        offset: u64,
        user_op: ValidUserOp,
        nonce: u32,
        safe_block: u64,
        batch_nonce: u64,
    ) -> Self {
        Self::UserOp {
            offset,
            sender: user_op.sender.to_string(),
            nonce,
            fee: user_op.fee,
            data: alloy_primitives::hex::encode_prefixed(user_op.data.as_slice()),
            safe_block,
            batch_nonce,
        }
    }

    pub fn from_direct_input(
        offset: u64,
        direct: DirectInput,
        input_index: u64,
        batch_nonce: u64,
        block_timestamp: u64,
        transaction_hash: alloy_primitives::B256,
    ) -> Self {
        Self::DirectInput {
            offset,
            sender: direct.sender.to_string(),
            block_number: direct.block_number,
            payload: alloy_primitives::hex::encode_prefixed(direct.payload.as_slice()),
            input_index,
            batch_nonce,
            block_timestamp,
            transaction_hash: alloy_primitives::hex::encode_prefixed(transaction_hash),
        }
    }
}
