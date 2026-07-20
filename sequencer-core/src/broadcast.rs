// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::l2_tx::SequencedL2Tx;
use serde::{Deserialize, Serialize};

// Execution-context fields (`safe_block`, `batch_nonce`, `input_index`, the user op's own
// `nonce`) are the minimal extension a downstream L1-first mirror needs to execute the soft
// tip faithfully and to dedup against L1 settlement; all were already persisted per
// sequenced row. Deliberately
// NOT added (deferred until a consumer needs them): frame/op position identity within the
// batch (batch granularity suffices for handoff), and an explicit recovery-generation
// handshake (mirrors treat any socket drop as a potential discontinuity and rebuild their
// soft suffix, which is always safe).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BroadcastTxMessage {
    UserOp {
        offset: u64,
        sender: String,
        /// The op's own signed replay-protection nonce. A mirror must apply
        /// the op under exactly this nonce; synthesizing it from mirror state
        /// mis-executes ops across feed gaps.
        nonce: u32,
        /// Log-space fee exponent (base 129/128). See [`crate::fee`].
        fee: u16,
        data: String,
        /// L1 safe block of the covering frame, the block the op executes under.
        safe_block: u64,
        /// Nonce of the batch the covering frame belongs to.
        batch_nonce: u64,
    },
    DirectInput {
        offset: u64,
        sender: String,
        block_number: u64,
        payload: String,
        /// L1 input index of the direct input.
        input_index: u64,
        /// Nonce of the batch whose frame drained this direct. Lets a mirror
        /// distinguish a direct already executed by the settled chain from one
        /// still parked in its scheduler fridge (both have settled L1 inputs).
        batch_nonce: u64,
    },
}

impl BroadcastTxMessage {
    pub fn offset(&self) -> u64 {
        match self {
            Self::UserOp { offset, .. } => *offset,
            Self::DirectInput { offset, .. } => *offset,
        }
    }

    /// Build a broadcast message from a sequenced row and its execution context.
    ///
    /// `input_index` is the direct input's L1 input index; it must be `Some` for direct
    /// rows (the storage row always carries it) and is ignored for user ops.
    pub fn from_offset_and_tx(
        offset: u64,
        tx: SequencedL2Tx,
        safe_block: u64,
        batch_nonce: u64,
        input_index: Option<u64>,
        op_nonce: Option<u32>,
    ) -> Self {
        match tx {
            SequencedL2Tx::UserOp(user_op) => Self::UserOp {
                offset,
                sender: user_op.sender.to_string(),
                nonce: op_nonce.expect("user op sequenced rows always carry their op nonce"),
                fee: user_op.fee,
                data: alloy_primitives::hex::encode_prefixed(user_op.data.as_slice()),
                safe_block,
                batch_nonce,
            },
            SequencedL2Tx::Direct(direct) => Self::DirectInput {
                offset,
                sender: direct.sender.to_string(),
                block_number: direct.block_number,
                payload: alloy_primitives::hex::encode_prefixed(direct.payload.as_slice()),
                input_index: input_index
                    .expect("direct sequenced rows always carry their L1 input index"),
                batch_nonce,
            },
        }
    }
}
