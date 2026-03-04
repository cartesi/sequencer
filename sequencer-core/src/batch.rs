// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::user_op::UserOp;
use ssz_derive::{Decode, Encode};

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Batch {
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Frame {
    pub user_ops: Vec<WireUserOp>,
    pub safe_block: u64,
    pub fee_price: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct WireUserOp {
    pub nonce: u32,
    pub max_fee: u32,
    pub data: Vec<u8>,
    pub signature: Vec<u8>,
}

impl WireUserOp {
    pub const SIGNATURE_BYTES: usize = 65;

    pub fn to_user_op(&self) -> UserOp {
        UserOp {
            nonce: self.nonce,
            max_fee: self.max_fee,
            data: self.data.clone().into(),
        }
    }
}
