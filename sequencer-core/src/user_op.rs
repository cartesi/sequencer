// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, Signature};
use alloy_sol_types::sol;
use serde::{Deserialize, Serialize};

sol! {
    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct UserOp {
        uint32 nonce;
        /// Log-space fee exponent (base 129/128). See [`crate::fee`].
        uint16 max_fee;
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
    pub const MAX_FEE_BYTES: usize = 2;

    pub const MAX_BATCH_METADATA_BYTES: usize =
        Self::SIGNATURE_BYTES + Self::NONCE_BYTES + Self::MAX_FEE_BYTES;

    pub const fn max_batch_metadata() -> usize {
        Self::MAX_BATCH_METADATA_BYTES
    }
}
