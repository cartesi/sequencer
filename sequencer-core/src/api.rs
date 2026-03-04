// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, Signature};
use alloy_sol_types::{Eip712Domain, SolStruct};
use thiserror::Error;

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

    pub fn into_signed_user_op(
        self,
        domain: &Eip712Domain,
        max_user_op_data_bytes: usize,
    ) -> Result<SignedUserOp, TxRequestError> {
        self.validate_hex_lengths()?;
        self.validate_payload_size(max_user_op_data_bytes)?;

        let signature = self.decode_signature()?;
        let expected_sender = self.decode_address()?;
        let recovered_sender = recover_sender(&self.message, &signature, domain)?;

        if expected_sender != recovered_sender {
            return Err(TxRequestError::invalid_signature("sender mismatch"));
        }

        Ok(SignedUserOp {
            sender: recovered_sender,
            signature,
            user_op: self.message,
        })
    }

    fn validate_hex_lengths(&self) -> Result<(), TxRequestError> {
        if self.signature.len() != Self::SIGNATURE_HEX_LEN {
            return Err(TxRequestError::bad_request(format!(
                "signature must be {} hex chars (0x + 65 bytes)",
                Self::SIGNATURE_HEX_LEN
            )));
        }
        if self.sender.len() != Self::ADDRESS_HEX_LEN {
            return Err(TxRequestError::bad_request(format!(
                "sender must be {} hex chars (0x + 20 bytes)",
                Self::ADDRESS_HEX_LEN
            )));
        }
        Ok(())
    }

    fn validate_payload_size(&self, max_user_op_data_bytes: usize) -> Result<(), TxRequestError> {
        let user_op_data_len = self.message.data.len();
        if user_op_data_len > max_user_op_data_bytes {
            return Err(TxRequestError::bad_request(format!(
                "user op payload too large: max {} bytes, got {} bytes",
                max_user_op_data_bytes, user_op_data_len
            )));
        }
        Ok(())
    }

    fn decode_signature(&self) -> Result<Signature, TxRequestError> {
        let signature_bytes =
            decode_hex_0x(self.signature.as_str()).map_err(TxRequestError::bad_request)?;
        if signature_bytes.len() != SignedUserOp::SIGNATURE_BYTES {
            return Err(TxRequestError::bad_request("signature must be 65 bytes"));
        }
        parse_signature(&signature_bytes)
    }

    fn decode_address(&self) -> Result<Address, TxRequestError> {
        let bytes = decode_hex_0x(self.sender.as_str()).map_err(TxRequestError::bad_request)?;
        if bytes.len() != Self::ADDRESS_BYTES {
            return Err(TxRequestError::bad_request("address must be 20 bytes"));
        }
        Ok(Address::from_slice(&bytes))
    }
}

#[derive(Debug, Error, Clone)]
pub enum TxRequestError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    InvalidSignature(String),
}

impl TxRequestError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn invalid_signature(message: impl Into<String>) -> Self {
        Self::InvalidSignature(message.into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxResponse {
    pub ok: bool,
    pub sender: String,
    pub nonce: u32,
}

pub type WsTxMessage = BroadcastTxMessage;

fn decode_hex_0x(value: &str) -> Result<Vec<u8>, String> {
    if !value.starts_with("0x") {
        return Err("hex string must start with 0x".to_string());
    }
    alloy_primitives::hex::decode(value).map_err(|err| format!("invalid hex: {err}"))
}

fn recover_sender(
    user_op: &UserOp,
    signature: &Signature,
    domain: &Eip712Domain,
) -> Result<Address, TxRequestError> {
    let signing_hash = user_op.eip712_signing_hash(domain);
    signature
        .recover_address_from_prehash(&signing_hash)
        .map_err(|_| TxRequestError::invalid_signature("cannot recover sender"))
}

fn parse_signature(bytes: &[u8]) -> Result<Signature, TxRequestError> {
    Signature::from_raw(bytes).map_err(|err| match err {
        alloy_primitives::SignatureError::FromBytes(_) => {
            TxRequestError::bad_request("signature must be 65 bytes")
        }
        alloy_primitives::SignatureError::FromHex(_) => {
            TxRequestError::bad_request("invalid signature hex")
        }
        alloy_primitives::SignatureError::InvalidParity(_) => {
            TxRequestError::invalid_signature("invalid signature parity")
        }
        alloy_primitives::SignatureError::K256(_) => {
            TxRequestError::invalid_signature("invalid signature")
        }
    })
}
