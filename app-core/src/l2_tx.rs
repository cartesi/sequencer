// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::Address;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectInput {
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ValidUserOp {
    pub sender: Address,
    // Fee committed by the sequencer for the batch that contains this user-op.
    pub fee: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum SequencedL2Tx {
    UserOp(ValidUserOp),
    Direct(DirectInput),
}
