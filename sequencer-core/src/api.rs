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
        let expected_sender = parse_sender_address(&self.sender)?;
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
}

/// Parse a sender the way `POST /tx` does: `0x` plus 40 hex digits in any
/// letter case. EIP-55 checksums are not enforced; the signature, not the
/// casing, binds a sender.
pub fn parse_sender_address(value: &str) -> Result<Address, TxRequestError> {
    if value.len() != TxRequest::ADDRESS_HEX_LEN {
        return Err(TxRequestError::bad_request(format!(
            "sender must be {} hex chars (0x + 20 bytes)",
            TxRequest::ADDRESS_HEX_LEN
        )));
    }
    let bytes = decode_hex_0x(value).map_err(TxRequestError::bad_request)?;
    Ok(Address::from_slice(&bytes))
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

/// Fee quote for wallets that need to set signed `max_fee` before `POST /tx`.
///
/// `fee` is frozen on the open frame (the live inclusion check).
/// `recommended_fee` is what the next frame will sample.
/// `suggested_max_fee` is `max(fee, recommended_fee)` plus 1.5× log-space
/// slack — the value a wallet can copy into `max_fee` so the signature
/// survives a frame rotation. The user pays the frame fee, not this cap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeeResponse {
    pub fee: u16,
    pub recommended_fee: u16,
    pub suggested_max_fee: u16,
}

impl FeeResponse {
    pub fn quote(fee: u16, recommended_fee: u16) -> Self {
        Self {
            fee,
            recommended_fee,
            suggested_max_fee: crate::fee::suggested_signing_max_fee(fee, recommended_fee),
        }
    }
}

/// `GET /nonce` body. `next_nonce` is the value to sign into `UserOp.nonce`;
/// [`TxResponse::nonce`] is instead the nonce an included op consumed.
/// `sender` is echoed in EIP-55 casing, like `POST /tx` responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NonceResponse {
    pub sender: String,
    pub next_nonce: u32,
}

/// `GET /domain` body: the EIP-712 domain `POST /tx` verifies signatures
/// against, keyed as `eth_signTypedData_v4` expects. Clients pin their own
/// domain and assert it matches, as a wallet checks `eth_chainId`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DomainResponse {
    pub name: String,
    pub version: String,
    pub chain_id: u64,
    /// EIP-55 casing.
    pub verifying_contract: String,
}

impl DomainResponse {
    /// `None` unless the domain has exactly the shape
    /// [`crate::build_input_domain`] produces: all four fields, no salt.
    pub fn from_domain(domain: &Eip712Domain) -> Option<Self> {
        if domain.salt.is_some() {
            return None;
        }
        Some(Self {
            name: domain.name.as_deref()?.to_owned(),
            version: domain.version.as_deref()?.to_owned(),
            chain_id: u64::try_from(domain.chain_id?).ok()?,
            verifying_contract: domain.verifying_contract?.to_string(),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_parser_accepts_any_case_and_nothing_else() {
        let address = Address::repeat_byte(0xab);
        let lower = format!("{address:#x}");
        for accepted in [lower.clone(), lower.to_uppercase().replacen("0X", "0x", 1)] {
            assert_eq!(parse_sender_address(&accepted).unwrap(), address);
        }
        assert_eq!(
            parse_sender_address(&address.to_checksum(None)).unwrap(),
            address
        );
        for rejected in [
            lower.trim_start_matches("0x"),
            &lower[..lower.len() - 2],
            &format!("{lower}00"),
            &lower.replacen('a', "g", 1),
            &lower.replacen("0x", "0X", 1),
        ] {
            assert!(
                parse_sender_address(rejected).is_err(),
                "accepted {rejected:?}"
            );
        }
    }

    #[test]
    fn domain_response_uses_eip712_keys_and_requires_the_full_domain() {
        let app = Address::repeat_byte(0xab);
        let domain = crate::build_input_domain(31337, app);
        let served = DomainResponse::from_domain(&domain).unwrap();
        assert_eq!(
            serde_json::to_value(&served).unwrap(),
            serde_json::json!({
                "name": crate::DOMAIN_NAME,
                "version": crate::DOMAIN_VERSION,
                "chainId": 31337,
                "verifyingContract": app.to_checksum(None),
            })
        );

        let mut partial = domain.clone();
        partial.verifying_contract = None;
        assert_eq!(DomainResponse::from_domain(&partial), None);
        let mut salted = domain;
        salted.salt = Some(alloy_primitives::B256::ZERO);
        assert_eq!(DomainResponse::from_domain(&salted), None);
    }
}
