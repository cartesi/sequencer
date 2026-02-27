// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, Signature};
use alloy_sol_types::sol;
use serde::{Deserialize, Serialize};

sol! {
    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct UserOp {
        uint32 nonce;
        uint32 max_fee;
        bytes data;
    }
}

#[derive(Debug, Clone)]
pub struct SignedUserOp {
    pub sender: Address,
    pub signature: Signature,
    pub user_op: UserOp,
}

impl SignedUserOp {
    pub const SIGNATURE_BYTES: usize = 65;
    pub const NONCE_BYTES: usize = 4;
    pub const MAX_FEE_BYTES: usize = 4;
    // Method is SSZ enum-union encoded; Transfer includes a 1-byte union tag + 32-byte amount + 20-byte recipient.
    pub const MAX_METHOD_PAYLOAD_BYTES: usize = 1 + 32 + 20;
    pub const MAX_BATCH_BYTES_UPPER_BOUND: usize = Self::SIGNATURE_BYTES
        + Self::NONCE_BYTES
        + Self::MAX_FEE_BYTES
        + Self::MAX_METHOD_PAYLOAD_BYTES;

    pub const fn max_batch_bytes_upper_bound() -> usize {
        Self::MAX_BATCH_BYTES_UPPER_BOUND
    }

    pub const fn batch_bytes_upper_bound_for_data_len(data_len: usize) -> usize {
        Self::SIGNATURE_BYTES + Self::NONCE_BYTES + Self::MAX_FEE_BYTES + data_len
    }
}
