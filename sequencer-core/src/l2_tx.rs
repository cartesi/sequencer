// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::Address;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectInput {
    pub sender: Address,
    pub block_number: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ValidUserOp {
    pub sender: Address,
    /// Log-space fee exponent (base 129/128) committed by the sequencer for this frame.
    /// Use [`crate::fee::fee_to_linear`] to convert to a linear amount for charging.
    pub fee: u16,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum SequencedL2Tx {
    UserOp(ValidUserOp),
    Direct(DirectInput),
}
