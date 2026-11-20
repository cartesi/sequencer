// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::user_op::UserOp;
use alloy_primitives::Address;

/// What the scheduler hands out and the application executes. The same record
/// serves the canonical machine, a rebuild, the lane and its replay, so every
/// caller runs the same executor checks on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L2Tx {
    Direct(Direct),
    UserOp(UserOpTx),
}

/// A direct input the scheduler drained from its fridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Direct {
    /// Per-application InputBox index of the input that carried it.
    pub input_index: u64,
    pub sender: Address,
    /// Its L1 inclusion block.
    pub block: u64,
    /// Unix timestamp, in seconds, of its L1 inclusion block.
    pub timestamp: u64,
    pub payload: Vec<u8>,
}

/// A user op whose signature the scheduler recovered and whose fee cap covers
/// the frame's price. It keeps its signed nonce so the executor, not the
/// application, decides whether it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserOpTx {
    /// The signer the scheduler recovered.
    pub sender: Address,
    /// The nonce the sender signed.
    pub nonce: u32,
    /// The frame's log-space fee price (base 129/128), which the op's cap
    /// covers. Use [`crate::fee::fee_to_linear`] for a linear amount.
    pub fee: u16,
    /// The frame's safe block.
    pub block: u64,
    pub data: Vec<u8>,
}

impl UserOpTx {
    /// Build the record from a signed op and its frame, so the lane, the
    /// scheduler and replays all build it the same way.
    pub fn new(sender: Address, op: &UserOp, fee: u16, block: u64) -> Self {
        Self {
            sender,
            nonce: op.nonce,
            fee,
            block,
            data: op.data.to_vec(),
        }
    }
}
