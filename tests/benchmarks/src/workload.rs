// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, Signature, U256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use app_core::application::{
    MAX_METHOD_PAYLOAD_BYTES, Method, Transfer, Withdrawal, default_private_keys,
};
use clap::ValueEnum;
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::hazmat::PrehashSigner;
use sequencer_core::api::TxRequest;
use sequencer_core::user_op::UserOp;
use serde::{Deserialize, Serialize};
use std::fs;

use crate::{BenchResult, support::err};

pub const DEFAULT_WORKLOAD_TRANSFER_AMOUNT: u64 = 1;

#[derive(Debug, Clone)]
pub struct SignedTxFixture {
    pub request: TxRequest,
    pub expected_sender: String,
    pub expected_data_hex: String,
}

#[derive(Debug, Clone)]
pub(crate) struct FundedAccountPlan {
    pub(crate) address: Address,
    pub(crate) required_balance: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    Synthetic,
    FundedTransfer,
}

impl WorkloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synthetic => "synthetic",
            Self::FundedTransfer => "funded-transfer",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadConfig {
    pub kind: WorkloadKind,
    pub accounts_file: Option<String>,
    pub transfer_amount: u64,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self {
            kind: WorkloadKind::Synthetic,
            accounts_file: None,
            transfer_amount: DEFAULT_WORKLOAD_TRANSFER_AMOUNT,
        }
    }
}

pub(crate) struct WorkloadState {
    inner: WorkloadStateInner,
}

enum WorkloadStateInner {
    Synthetic {
        next_seed: u64,
    },
    FundedTransfer {
        accounts: Vec<FundedAccount>,
        round_robin_index: usize,
        transfer_amount: u64,
    },
}

pub(crate) fn funded_account_plans(
    config: &WorkloadConfig,
    total_count: u64,
) -> BenchResult<Vec<FundedAccountPlan>> {
    if config.kind != WorkloadKind::FundedTransfer {
        return Ok(Vec::new());
    }

    let keys = match config.accounts_file.as_deref() {
        Some(path) => load_private_keys_from_file(path)?,
        None => default_private_keys().to_vec(),
    };
    if keys.is_empty() {
        return Err(err("no private keys available for funded workload"));
    }

    let account_count = keys.len() as u64;
    let mut plans = Vec::with_capacity(keys.len());
    for (index, private_key) in keys.into_iter().enumerate() {
        let signing_key = signing_key_from_hex(private_key.as_str())?;
        let address = address_from_signing_key(&signing_key);
        let send_count = round_robin_send_count(total_count, account_count, index as u64);
        let required_balance = required_funded_transfer_balance(send_count, config.transfer_amount);
        plans.push(FundedAccountPlan {
            address,
            required_balance,
        });
    }

    Ok(plans)
}

#[derive(Clone)]
struct FundedAccount {
    signing_key: SigningKey,
    sender: Address,
    next_nonce: u32,
}

impl WorkloadState {
    pub(crate) fn initialize(config: &WorkloadConfig, seed_offset: u64) -> BenchResult<Self> {
        match config.kind {
            WorkloadKind::Synthetic => Ok(Self {
                inner: WorkloadStateInner::Synthetic {
                    next_seed: seed_offset,
                },
            }),
            WorkloadKind::FundedTransfer => {
                let accounts = load_funded_accounts(config.accounts_file.as_deref())?;
                Ok(Self {
                    inner: WorkloadStateInner::FundedTransfer {
                        accounts,
                        round_robin_index: 0,
                        transfer_amount: config.transfer_amount,
                    },
                })
            }
        }
    }

    pub(crate) fn next_fixture(
        &mut self,
        max_fee: u32,
        domain: &Eip712Domain,
    ) -> BenchResult<SignedTxFixture> {
        match &mut self.inner {
            WorkloadStateInner::Synthetic { next_seed } => {
                let fixture = make_signed_fixture(*next_seed, max_fee, domain)?;
                *next_seed = next_seed.wrapping_add(1);
                Ok(fixture)
            }
            WorkloadStateInner::FundedTransfer {
                accounts,
                round_robin_index,
                transfer_amount,
            } => {
                if accounts.is_empty() {
                    return Err(err("funded workload has zero accounts"));
                }
                let sender_index = *round_robin_index % accounts.len();
                let recipient_index = (sender_index + 1) % accounts.len();
                let recipient = accounts[recipient_index].sender;
                let sender = &mut accounts[sender_index];

                let amount =
                    U256::from((*transfer_amount).saturating_add(u64::from(sender.next_nonce)));
                let method = Method::Transfer(Transfer {
                    amount,
                    to: recipient,
                });
                let data = ssz::Encode::as_ssz_bytes(&method);
                if data.len() > MAX_METHOD_PAYLOAD_BYTES {
                    return Err(err(format!(
                        "funded transfer payload too large: {} > {}",
                        data.len(),
                        MAX_METHOD_PAYLOAD_BYTES
                    )));
                }

                let user_op = UserOp {
                    nonce: sender.next_nonce,
                    max_fee,
                    data: data.into(),
                };
                let fixture =
                    make_signed_fixture_from_signing_key(&sender.signing_key, user_op, domain)?;
                sender.next_nonce = sender.next_nonce.wrapping_add(1);
                *round_robin_index = (*round_robin_index + 1) % accounts.len();
                Ok(fixture)
            }
        }
    }

    pub(crate) fn concurrency_cap(&self) -> Option<usize> {
        match &self.inner {
            WorkloadStateInner::Synthetic { .. } => None,
            WorkloadStateInner::FundedTransfer { accounts, .. } => Some(accounts.len().max(1)),
        }
    }
}

pub fn make_signed_fixture(
    seed: u64,
    max_fee: u32,
    domain: &Eip712Domain,
) -> BenchResult<SignedTxFixture> {
    let signing_key = signing_key_for_seed(seed)?;
    let sender = address_from_signing_key(&signing_key);
    let method = Method::Withdrawal(Withdrawal {
        amount: U256::from(seed.saturating_add(1)),
    });
    let data = ssz::Encode::as_ssz_bytes(&method);
    if data.len() > MAX_METHOD_PAYLOAD_BYTES {
        return Err(err(format!(
            "benchmark payload too large: {} > {}",
            data.len(),
            MAX_METHOD_PAYLOAD_BYTES
        )));
    }
    let message = UserOp {
        nonce: 0,
        max_fee,
        data: data.clone().into(),
    };

    let fixture = make_signed_fixture_from_signing_key(&signing_key, message, domain)?;
    if fixture.expected_sender != sender.to_string() {
        return Err(err("unexpected synthetic sender mismatch"));
    }
    Ok(fixture)
}

fn load_funded_accounts(accounts_file: Option<&str>) -> BenchResult<Vec<FundedAccount>> {
    let keys = match accounts_file {
        Some(path) => load_private_keys_from_file(path)?,
        None => default_private_keys().to_vec(),
    };
    if keys.is_empty() {
        return Err(err("no private keys available for funded workload"));
    }

    let mut accounts = Vec::with_capacity(keys.len());
    for key_hex in keys {
        let signing_key = signing_key_from_hex(key_hex.as_str())?;
        let sender = address_from_signing_key(&signing_key);
        accounts.push(FundedAccount {
            signing_key,
            sender,
            next_nonce: 0,
        });
    }
    Ok(accounts)
}

fn round_robin_send_count(total_count: u64, account_count: u64, account_index: u64) -> u64 {
    if account_index >= total_count {
        return 0;
    }
    1 + (total_count - 1 - account_index) / account_count
}

fn required_funded_transfer_balance(send_count: u64, transfer_amount: u64) -> U256 {
    if send_count == 0 {
        return U256::ZERO;
    }

    let send_count_u256 = U256::from(send_count);
    let transfer_amount_u256 = U256::from(transfer_amount);
    let arithmetic_sum = send_count_u256 * U256::from(send_count - 1) / U256::from(2_u64);
    (send_count_u256 * transfer_amount_u256) + arithmetic_sum
}

fn load_private_keys_from_file(path: &str) -> BenchResult<Vec<String>> {
    let contents = fs::read_to_string(path)
        .map_err(|e| err(format!("failed reading accounts file '{path}': {e}")))?;
    let mut keys = Vec::new();
    for (line_no, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let address = parts.next().ok_or_else(|| {
            err(format!(
                "accounts file '{path}' line {}: missing address",
                line_no + 1
            ))
        })?;
        let private_key = parts.next().ok_or_else(|| {
            err(format!(
                "accounts file '{path}' line {}: missing private key",
                line_no + 1
            ))
        })?;
        if parts.next().is_some() {
            return Err(err(format!(
                "accounts file '{path}' line {}: expected exactly two fields: <address> <private_key>",
                line_no + 1
            )));
        }

        validate_hex_token(path, line_no + 1, "address", address, 42)?;
        validate_hex_token(path, line_no + 1, "private_key", private_key, 66)?;
        keys.push(private_key.to_string());
    }
    if keys.is_empty() {
        return Err(err(format!(
            "accounts file '{path}' did not contain any account records"
        )));
    }
    Ok(keys)
}

fn validate_hex_token(
    path: &str,
    line_no: usize,
    field: &str,
    value: &str,
    expected_len: usize,
) -> BenchResult<()> {
    if value.len() != expected_len {
        return Err(err(format!(
            "accounts file '{path}' line {}: invalid {field} length: expected {}, got {}",
            line_no,
            expected_len,
            value.len()
        )));
    }
    if !value.starts_with("0x") {
        return Err(err(format!(
            "accounts file '{path}' line {}: {field} must start with 0x",
            line_no
        )));
    }
    if !value.as_bytes().iter().skip(2).all(u8::is_ascii_hexdigit) {
        return Err(err(format!(
            "accounts file '{path}' line {}: {field} must be hex",
            line_no
        )));
    }
    Ok(())
}

fn signing_key_from_hex(hex: &str) -> BenchResult<SigningKey> {
    let bytes = alloy_primitives::hex::decode(hex)
        .map_err(|e| err(format!("invalid private key hex '{hex}': {e}")))?;
    if bytes.len() != 32 {
        return Err(err(format!(
            "invalid private key length: expected 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut key_bytes = [0_u8; 32];
    key_bytes.copy_from_slice(&bytes);
    SigningKey::from_bytes((&key_bytes).into())
        .map_err(|e| err(format!("invalid private key material: {e}")))
}

fn make_signed_fixture_from_signing_key(
    signing_key: &SigningKey,
    user_op: UserOp,
    domain: &Eip712Domain,
) -> BenchResult<SignedTxFixture> {
    let sender = address_from_signing_key(signing_key);
    let signature = sign_user_op(domain, &user_op, signing_key)?;
    let data = user_op.data.to_vec();
    Ok(SignedTxFixture {
        request: TxRequest {
            message: user_op,
            signature,
            sender: sender.to_string(),
        },
        expected_sender: sender.to_string(),
        expected_data_hex: alloy_primitives::hex::encode_prefixed(data),
    })
}

fn signing_key_for_seed(seed: u64) -> BenchResult<SigningKey> {
    let mut bytes = [0_u8; 32];
    bytes[24..32].copy_from_slice(&seed.saturating_add(1).to_be_bytes());
    SigningKey::from_bytes((&bytes).into())
        .map_err(|e| err(format!("build signing key failed: {e}")))
}

fn sign_user_op(
    domain: &Eip712Domain,
    user_op: &UserOp,
    signing_key: &SigningKey,
) -> BenchResult<String> {
    let hash = user_op.eip712_signing_hash(domain);
    let k256_sig = signing_key
        .sign_prehash(hash.as_slice())
        .map_err(|e| err(format!("sign user op prehash failed: {e}")))?;

    let expected_sender = address_from_signing_key(signing_key);
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|candidate| {
            candidate
                .recover_address_from_prehash(&hash)
                .ok()
                .map(|sender| sender == expected_sender)
                .unwrap_or(false)
        })
        .ok_or_else(|| err("could not recover parity for signature"))?;

    Ok(alloy_primitives::hex::encode_prefixed(signature.as_bytes()))
}

fn address_from_signing_key(signing_key: &SigningKey) -> Address {
    let verifying = signing_key.verifying_key().to_encoded_point(false);
    Address::from_raw_public_key(&verifying.as_bytes()[1..])
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        FundedAccount, WorkloadConfig, WorkloadKind, WorkloadState, WorkloadStateInner,
        address_from_signing_key, default_private_keys, funded_account_plans,
        required_funded_transfer_balance, round_robin_send_count, signing_key_from_hex,
    };
    use crate::self_contained_domain;
    use alloy_primitives::U256;

    #[test]
    fn funded_transfer_round_robin_nonce_progression() {
        let mut accounts = Vec::new();
        for key in default_private_keys().iter().take(2) {
            let signing_key = signing_key_from_hex(key.as_str()).expect("signing key");
            accounts.push(FundedAccount {
                sender: address_from_signing_key(&signing_key),
                signing_key,
                next_nonce: 1,
            });
        }

        let domain = self_contained_domain().eip712_domain();
        let mut state = WorkloadState {
            inner: WorkloadStateInner::FundedTransfer {
                accounts,
                round_robin_index: 0,
                transfer_amount: 1,
            },
        };

        let one = state.next_fixture(0, &domain).expect("fixture 1");
        let two = state.next_fixture(0, &domain).expect("fixture 2");
        let three = state.next_fixture(0, &domain).expect("fixture 3");

        assert_ne!(one.expected_sender, two.expected_sender);
        assert_eq!(one.expected_sender, three.expected_sender);
        assert_eq!(one.request.message.nonce, 1);
        assert_eq!(two.request.message.nonce, 1);
        assert_eq!(three.request.message.nonce, 2);
    }

    #[test]
    fn funding_plan_matches_round_robin_workload() {
        let temp_path =
            std::env::temp_dir().join(format!("funded-account-plans-{}.txt", std::process::id()));
        let mut contents = String::new();
        for key in default_private_keys().iter().take(3) {
            let signing_key = signing_key_from_hex(key.as_str()).expect("signing key");
            let address = address_from_signing_key(&signing_key);
            contents.push_str(address.to_string().as_str());
            contents.push(' ');
            contents.push_str(key);
            contents.push('\n');
        }
        fs::write(&temp_path, contents).expect("accounts file");

        let config = WorkloadConfig {
            kind: WorkloadKind::FundedTransfer,
            accounts_file: Some(temp_path.to_string_lossy().to_string()),
            transfer_amount: 5,
        };

        let plans = funded_account_plans(&config, 5).expect("funding plan");
        let _ = fs::remove_file(&temp_path);
        assert!(plans.len() >= 3);
        assert_eq!(plans[0].required_balance, U256::from(11_u64));
        assert_eq!(plans[1].required_balance, U256::from(11_u64));
        assert_eq!(plans[2].required_balance, U256::from(5_u64));
    }

    #[test]
    fn round_robin_count_and_required_balance_helpers_work() {
        assert_eq!(round_robin_send_count(0, 3, 0), 0);
        assert_eq!(round_robin_send_count(5, 3, 0), 2);
        assert_eq!(round_robin_send_count(5, 3, 1), 2);
        assert_eq!(round_robin_send_count(5, 3, 2), 1);
        assert_eq!(required_funded_transfer_balance(0, 5), U256::ZERO);
        assert_eq!(required_funded_transfer_balance(3, 5), U256::from(18_u64));
    }
}
