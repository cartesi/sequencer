// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;
use app_core::application::{MAX_METHOD_PAYLOAD_BYTES, Method, Transfer, default_private_keys};
use clap::ValueEnum;
use k256::ecdsa::SigningKey;
use rollups_harness::{address_from_signing_key, sign_user_op_hex};
use sequencer_core::api::TxRequest;
use sequencer_core::fee::fee_to_linear;
use sequencer_core::user_op::UserOp;
use serde::{Deserialize, Serialize};
use std::fs;

use crate::{BenchResult, support::io_err};

pub const DEFAULT_WORKLOAD_TRANSFER_AMOUNT: u64 = 1;

#[derive(Debug, Clone)]
pub(crate) struct FundedAccountPlan {
    pub(crate) address: Address,
    pub(crate) required_balance: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    FundedTransfer,
}

impl WorkloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
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
            kind: WorkloadKind::FundedTransfer,
            accounts_file: None,
            transfer_amount: DEFAULT_WORKLOAD_TRANSFER_AMOUNT,
        }
    }
}

/// Build funding plans for the worker-per-wallet model.
///
/// `per_worker_counts[i]` is the number of transactions worker `i` will send.
/// Only the first `per_worker_counts.len()` accounts are funded; the rest get
/// zero balance requirements (but are still listed so the harness can iterate).
pub(crate) fn funded_account_plans(
    config: &WorkloadConfig,
    per_worker_counts: &[u64],
    max_fee: u16,
) -> BenchResult<Vec<FundedAccountPlan>> {
    let keys = match config.accounts_file.as_deref() {
        Some(path) => load_private_keys_from_file(path)?,
        None => default_private_keys().to_vec(),
    };
    if keys.is_empty() {
        return Err(io_err("no private keys available for funded workload"));
    }

    let mut plans = Vec::with_capacity(keys.len());
    for (index, private_key) in keys.into_iter().enumerate() {
        let signing_key = signing_key_from_hex(private_key.as_str())?;
        let address = address_from_signing_key(&signing_key);
        let send_count = per_worker_counts.get(index).copied().unwrap_or(0);
        let required_balance =
            required_funded_transfer_balance(send_count, config.transfer_amount, max_fee);
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
}

/// Lightweight per-worker context for just-in-time signing.
///
/// Instead of pre-generating all signed fixtures, each worker holds a
/// `WorkerContext` and derives + signs each `UserOp` on the fly from its
/// loop index. This keeps memory usage constant per worker regardless of
/// how many transactions it sends.
#[derive(Clone)]
pub(crate) struct WorkerContext {
    pub signing_key: SigningKey,
    pub sender: Address,
    pub recipient: Address,
    pub start_nonce: u32,
    pub max_fee: u16,
    pub transfer_amount: u64,
    pub domain: Eip712Domain,
}

impl WorkerContext {
    /// Build and sign a single `TxRequest` for the given loop index.
    pub fn build_request(&self, index: u64) -> BenchResult<TxRequest> {
        let nonce = self.start_nonce + index as u32;
        let amount = U256::from(self.transfer_amount.saturating_add(u64::from(nonce)));
        let method = Method::Transfer(Transfer {
            amount,
            to: self.recipient,
        });
        let data = ssz::Encode::as_ssz_bytes(&method);
        if data.len() > MAX_METHOD_PAYLOAD_BYTES {
            return Err(io_err(format!(
                "funded transfer payload too large: {} > {}",
                data.len(),
                MAX_METHOD_PAYLOAD_BYTES
            )));
        }
        let user_op = UserOp {
            nonce,
            max_fee: self.max_fee,
            data: data.into(),
        };
        let signature = sign_user_op_hex(&self.signing_key, &self.domain, &user_op)?;
        Ok(TxRequest {
            message: user_op,
            signature,
            sender: self.sender.to_string(),
        })
    }
}

/// Build lightweight worker contexts for just-in-time signing.
///
/// Worker `i` gets a context with account `i`'s signing key, a recipient
/// (account `(i+1) % len`), and the nonce offset for that worker.
pub(crate) fn build_worker_contexts(
    config: &WorkloadConfig,
    workers: usize,
    max_fee: u16,
    domain: &Eip712Domain,
    nonce_offsets: &[u64],
) -> BenchResult<Vec<WorkerContext>> {
    let accounts = load_funded_accounts(config.accounts_file.as_deref())?;
    if workers > accounts.len() {
        return Err(io_err(format!(
            "requested {} workers but only {} funded accounts available",
            workers,
            accounts.len()
        )));
    }

    let mut contexts = Vec::with_capacity(workers);
    for worker_idx in 0..workers {
        let account = &accounts[worker_idx];
        let recipient = &accounts[(worker_idx + 1) % accounts.len()];
        let offset = nonce_offsets.get(worker_idx).copied().unwrap_or(0);
        contexts.push(WorkerContext {
            signing_key: account.signing_key.clone(),
            sender: account.sender,
            recipient: recipient.sender,
            start_nonce: offset as u32,
            max_fee,
            transfer_amount: config.transfer_amount,
            domain: domain.clone(),
        });
    }

    Ok(contexts)
}

fn load_funded_accounts(accounts_file: Option<&str>) -> BenchResult<Vec<FundedAccount>> {
    let keys = match accounts_file {
        Some(path) => load_private_keys_from_file(path)?,
        None => default_private_keys().to_vec(),
    };
    if keys.is_empty() {
        return Err(io_err("no private keys available for funded workload"));
    }

    let mut accounts = Vec::with_capacity(keys.len());
    for key_hex in keys {
        let signing_key = signing_key_from_hex(key_hex.as_str())?;
        let sender = address_from_signing_key(&signing_key);
        accounts.push(FundedAccount {
            signing_key,
            sender,
        });
    }
    Ok(accounts)
}

fn required_funded_transfer_balance(send_count: u64, transfer_amount: u64, max_fee: u16) -> U256 {
    if send_count == 0 {
        return U256::ZERO;
    }

    let send_count_u256 = U256::from(send_count);
    let transfer_amount_u256 = U256::from(transfer_amount);
    let arithmetic_sum = send_count_u256 * U256::from(send_count - 1) / U256::from(2_u64);
    // Each send also costs fee_to_linear(frame_fee) gas. Use max_fee as an upper
    // bound so accounts are funded even if frame_fee == max_fee.
    let gas_per_op = fee_to_linear(max_fee);
    (send_count_u256 * transfer_amount_u256) + arithmetic_sum + (send_count_u256 * gas_per_op)
}

fn load_private_keys_from_file(path: &str) -> BenchResult<Vec<String>> {
    let contents = fs::read_to_string(path)
        .map_err(|e| io_err(format!("failed reading accounts file '{path}': {e}")))?;
    let mut keys = Vec::new();
    for (line_no, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let address = parts.next().ok_or_else(|| {
            io_err(format!(
                "accounts file '{path}' line {}: missing address",
                line_no + 1
            ))
        })?;
        let private_key = parts.next().ok_or_else(|| {
            io_err(format!(
                "accounts file '{path}' line {}: missing private key",
                line_no + 1
            ))
        })?;
        if parts.next().is_some() {
            return Err(io_err(format!(
                "accounts file '{path}' line {}: expected exactly two fields: <address> <private_key>",
                line_no + 1
            )));
        }

        validate_hex_token(path, line_no + 1, "address", address, 42)?;
        validate_hex_token(path, line_no + 1, "private_key", private_key, 66)?;
        keys.push(private_key.to_string());
    }
    if keys.is_empty() {
        return Err(io_err(format!(
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
        return Err(io_err(format!(
            "accounts file '{path}' line {}: invalid {field} length: expected {}, got {}",
            line_no,
            expected_len,
            value.len()
        )));
    }
    if !value.starts_with("0x") {
        return Err(io_err(format!(
            "accounts file '{path}' line {}: {field} must start with 0x",
            line_no
        )));
    }
    if !value.as_bytes().iter().skip(2).all(u8::is_ascii_hexdigit) {
        return Err(io_err(format!(
            "accounts file '{path}' line {}: {field} must be hex",
            line_no
        )));
    }
    Ok(())
}

fn signing_key_from_hex(hex: &str) -> BenchResult<SigningKey> {
    let bytes = alloy_primitives::hex::decode(hex)
        .map_err(|e| io_err(format!("invalid private key hex '{hex}': {e}")))?;
    if bytes.len() != 32 {
        return Err(io_err(format!(
            "invalid private key length: expected 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut key_bytes = [0_u8; 32];
    key_bytes.copy_from_slice(&bytes);
    SigningKey::from_bytes((&key_bytes).into())
        .map_err(|e| io_err(format!("invalid private key material: {e}")))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        WorkloadConfig, WorkloadKind, build_worker_contexts, default_private_keys,
        funded_account_plans, required_funded_transfer_balance, signing_key_from_hex,
    };
    use alloy_primitives::U256;
    use alloy_sol_types::Eip712Domain;
    use rollups_harness::address_from_signing_key;

    #[test]
    fn worker_context_build_request_nonce_progression() {
        let domain = Eip712Domain {
            name: Some("CartesiAppSequencer".to_string().into()),
            version: Some("1".to_string().into()),
            chain_id: Some(U256::from(31_337_u64)),
            verifying_contract: None,
            salt: None,
        };
        let config = WorkloadConfig {
            kind: WorkloadKind::FundedTransfer,
            accounts_file: None,
            transfer_amount: 1,
        };

        // With empty nonce_offsets, nonces start at 0.
        let contexts = build_worker_contexts(&config, 2, 0, &domain, &[]).expect("contexts");
        assert_eq!(contexts.len(), 2);

        // Worker 0: nonces 0, 1, 2
        let r0 = contexts[0].build_request(0).unwrap();
        let r1 = contexts[0].build_request(1).unwrap();
        let r2 = contexts[0].build_request(2).unwrap();
        assert_eq!(r0.message.nonce, 0);
        assert_eq!(r1.message.nonce, 1);
        assert_eq!(r2.message.nonce, 2);

        // Worker 1: nonces 0, 1, 2
        let s0 = contexts[1].build_request(0).unwrap();
        let s1 = contexts[1].build_request(1).unwrap();
        let s2 = contexts[1].build_request(2).unwrap();
        assert_eq!(s0.message.nonce, 0);
        assert_eq!(s1.message.nonce, 1);
        assert_eq!(s2.message.nonce, 2);

        // Different senders
        assert_ne!(r0.sender, s0.sender);

        // With per-worker offsets, each worker starts at its own nonce.
        let contexts2 = build_worker_contexts(&config, 2, 0, &domain, &[5, 10]).expect("contexts");
        assert_eq!(contexts2[0].build_request(0).unwrap().message.nonce, 5);
        assert_eq!(contexts2[0].build_request(1).unwrap().message.nonce, 6);
        assert_eq!(contexts2[1].build_request(0).unwrap().message.nonce, 10);
        assert_eq!(contexts2[1].build_request(1).unwrap().message.nonce, 11);
    }

    #[test]
    fn funding_plan_matches_per_worker_counts() {
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

        // Use max_fee=0 so fee_to_linear(0)=1, adding 1 gas per send.
        // Worker 0 sends 3 txs, worker 1 sends 2 txs, worker 2 sends 0.
        let plans = funded_account_plans(&config, &[3, 2], 0).expect("funding plan");
        let _ = fs::remove_file(&temp_path);
        assert!(plans.len() >= 3);
        // Account 0: send_count=3 → 3*5 + 3*2/2 + 3*1 = 15 + 3 + 3 = 21
        assert_eq!(plans[0].required_balance, U256::from(21_u64));
        // Account 1: send_count=2 → 2*5 + 2*1/2 + 2*1 = 10 + 1 + 2 = 13
        assert_eq!(plans[1].required_balance, U256::from(13_u64));
        // Account 2: send_count=0 (not in per_worker_counts) → 0
        assert_eq!(plans[2].required_balance, U256::ZERO);
    }

    #[test]
    fn required_balance_helpers_work() {
        // max_fee=0 → fee_to_linear(0)=1, so gas adds send_count to the total.
        assert_eq!(required_funded_transfer_balance(0, 5, 0), U256::ZERO);
        // 3*5 + 3*2/2 + 3*1 = 15 + 3 + 3 = 21
        assert_eq!(
            required_funded_transfer_balance(3, 5, 0),
            U256::from(21_u64)
        );
    }
}
