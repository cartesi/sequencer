// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::broadcast::BroadcastTxMessage;
use crate::user_op::{SignedUserOp, UserOp};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRequest {
    pub message: UserOp,
    pub signature: String,
    pub sender: String,
}

impl TxRequest {
    pub const HEX_PREFIX_LEN: usize = 2;
    pub const ADDRESS_BYTES: usize = 20;
    pub const SIGNATURE_HEX_LEN: usize = Self::HEX_PREFIX_LEN + (SignedUserOp::SIGNATURE_BYTES * 2);
    pub const ADDRESS_HEX_LEN: usize = Self::HEX_PREFIX_LEN + (Self::ADDRESS_BYTES * 2);
    // Conservative wire-level cap for TxRequest JSON. It intentionally leaves headroom for field
    // names, quotes, separators, and decimal nonce/max_fee rendering.
    pub const MAX_JSON_BYTES_RECOMMENDED: usize = 4 * 1024;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxResponse {
    pub ok: bool,
    pub sender: String,
    pub nonce: u32,
}

pub type WsTxMessage = BroadcastTxMessage;
