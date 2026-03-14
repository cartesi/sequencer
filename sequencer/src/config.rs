// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;
use clap::{ArgGroup, Parser};

pub const DOMAIN_NAME: &str = "CartesiAppSequencer";
pub const DOMAIN_VERSION: &str = "1";

const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:3000";
const DEFAULT_DB_PATH: &str = "sequencer.db";

/// `-32005` Infura
/// `-32600`, `-32602` Alchemy
/// `-32616` QuickNode
const DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES: &[&str] = &["-32005", "-32600", "-32602", "-32616"];

/// Shared L1 / InputBox configuration used by both the input reader and the batch submitter.
///
/// Built once at startup from `RunConfig` plus the discovered InputBox address, so RPC URL,
/// InputBox address, and app (verifying contract) address are defined in a single place and
/// not duplicated across component configs.
#[derive(Debug, Clone)]
pub struct L1Config {
    /// L1 Ethereum RPC URL (e.g. for reading safe blocks and posting batch inputs).
    pub eth_rpc_url: String,
    /// InputBox contract address (same contract for ingesting direct inputs and for submitting batches).
    pub input_box_address: Address,
    /// Application / verifying contract address (used to discover InputBox and filter inputs).
    pub app_address: Address,
    /// Hex-encoded private key used by the batch submitter for posting batches to L1.
    ///
    /// `RunConfig` is responsible for resolving whether this comes from an inline
    /// value or a key file; by the time `L1Config` is constructed this is always
    /// the fully resolved private key.
    pub batch_submitter_private_key: String,
    /// EOA address of the batch submitter (derived from `batch_submitter_private_key`).
    /// Inputs from this sender are batch submissions; all others are direct inputs.
    pub batch_submitter_address: Address,
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "sequencer",
    about = "Deterministic sequencer prototype with low-latency soft confirmations",
    version,
    after_help = "\
Examples:
  sequencer \\
    --eth-rpc-url http://127.0.0.1:8545 \\
    --domain-chain-id 31337 \\
    --domain-verifying-contract 0x1111111111111111111111111111111111111111 \\
    --batch-submitter-private-key 0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef

  sequencer \\
    --http-addr 0.0.0.0:3000 \\
    --db-path ./sequencer.db \\
    --eth-rpc-url https://eth.example \\
    --domain-chain-id 1 \\
    --domain-verifying-contract 0x4444444444444444444444444444444444444444 \\
    --batch-submitter-private-key 0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\
",
    group(
        ArgGroup::new("batch_submitter_key_source")
            .args(&["batch_submitter_private_key", "batch_submitter_private_key_file"])
            .required(true)
            .multiple(false)
    )
)]
pub struct RunConfig {
    #[arg(long, env = "SEQ_HTTP_ADDR", default_value = DEFAULT_HTTP_ADDR, value_parser = parse_non_empty_string)]
    pub http_addr: String,
    #[arg(long, env = "SEQ_DB_PATH", default_value = DEFAULT_DB_PATH, value_parser = parse_non_empty_string)]
    pub db_path: String,
    #[arg(long, env = "SEQ_ETH_RPC_URL", value_parser = parse_non_empty_string)]
    pub eth_rpc_url: String,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    #[arg(long, env = "SEQ_LONG_BLOCK_RANGE_ERROR_CODES", value_delimiter = ',', default_values = DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES)]
    pub long_block_range_error_codes: Vec<String>,
    #[arg(long, env = "SEQ_DOMAIN_CHAIN_ID")]
    pub domain_chain_id: u64,
    #[arg(long, env = "SEQ_DOMAIN_VERIFYING_CONTRACT", value_parser = parse_address)]
    pub domain_verifying_contract: Address,
    /// Hex-encoded private key used by the batch submitter for posting batches to L1.
    /// Exactly one of this or `batch_submitter_private_key_file` must be set when a signer
    /// is desired.
    #[arg(
        long,
        env = "SEQ_BATCH_SUBMITTER_PRIVATE_KEY",
        group = "batch_submitter_key_source"
    )]
    pub batch_submitter_private_key: Option<String>,
    /// Path to a file whose first line contains the batch submitter private key. Takes
    /// precedence over `batch_submitter_private_key` if both are set (checked by clap group).
    #[arg(
        long,
        env = "SEQ_BATCH_SUBMITTER_PRIVATE_KEY_FILE",
        group = "batch_submitter_key_source"
    )]
    pub batch_submitter_private_key_file: Option<String>,

    /// How often the batch submitter polls for new work when idle.
    #[arg(
        long,
        env = "SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS",
        default_value = "5000"
    )]
    pub batch_submitter_idle_poll_interval_ms: u64,

    /// Maximum number of batches to submit in a single loop iteration.
    #[arg(
        long,
        env = "SEQ_BATCH_SUBMITTER_MAX_BATCHES_PER_LOOP",
        default_value = "4"
    )]
    pub batch_submitter_max_batches_per_loop: usize,

    /// Number of blocks to scan back from Latest when deriving the latest submitted batch index.
    /// 0 means use only the latest block. Used for at-least-once recovery (reorg-safe).
    #[arg(long, env = "SEQ_BATCH_SUBMITTER_SCAN_DEPTH", default_value = "0")]
    pub batch_submitter_scan_depth: u64,
}

impl RunConfig {
    pub fn build_domain(&self) -> Eip712Domain {
        Eip712Domain {
            name: Some(DOMAIN_NAME.into()),
            version: Some(DOMAIN_VERSION.into()),
            chain_id: Some(U256::from(self.domain_chain_id)),
            verifying_contract: Some(self.domain_verifying_contract),
            salt: None,
        }
    }
}

fn parse_non_empty_string(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("value cannot be empty".to_string());
    }
    Ok(value.to_string())
}

fn parse_address(raw: &str) -> Result<Address, String> {
    if !raw.starts_with("0x") {
        return Err("verifying contract must be 0x-prefixed".to_string());
    }

    let bytes = alloy_primitives::hex::decode(raw)
        .map_err(|err| format!("invalid verifying contract hex: {err}"))?;
    if bytes.len() != 20 {
        return Err("verifying contract must be 20 bytes".to_string());
    }
    Ok(Address::from_slice(&bytes))
}

#[cfg(test)]
mod tests {
    use super::{DOMAIN_NAME, DOMAIN_VERSION, RunConfig};
    use alloy_primitives::{Address, U256};
    use clap::Parser;

    #[test]
    fn run_config_requires_deployment_domain_inputs() {
        let err = RunConfig::try_parse_from([
            "sequencer",
            "--batch-submitter-private-key",
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ])
        .expect_err("domain inputs are required");

        let message = err.to_string();
        assert!(message.contains("--eth-rpc-url"));
        assert!(message.contains("--domain-chain-id"));
        assert!(message.contains("--domain-verifying-contract"));
    }

    #[test]
    fn run_config_uses_default_block_range_retry_codes() {
        let config = RunConfig::try_parse_from([
            "sequencer",
            "--eth-rpc-url",
            "http://127.0.0.1:8545",
            "--domain-chain-id",
            "31337",
            "--domain-verifying-contract",
            "0x1111111111111111111111111111111111111111",
            "--batch-submitter-private-key",
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ])
        .expect("parse run config");

        assert_eq!(
            config.long_block_range_error_codes,
            vec![
                "-32005".to_string(),
                "-32600".to_string(),
                "-32602".to_string(),
                "-32616".to_string()
            ]
        );
    }

    #[test]
    fn run_config_builds_domain_with_fixed_name_and_version() {
        let config = RunConfig::try_parse_from([
            "sequencer",
            "--eth-rpc-url",
            "http://127.0.0.1:8545",
            "--domain-chain-id",
            "31337",
            "--domain-verifying-contract",
            "0x1111111111111111111111111111111111111111",
            "--batch-submitter-private-key",
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ])
        .expect("parse run config");

        let domain = config.build_domain();
        assert_eq!(domain.name.as_deref(), Some(DOMAIN_NAME));
        assert_eq!(domain.version.as_deref(), Some(DOMAIN_VERSION));
        assert_eq!(domain.chain_id, Some(U256::from(31337_u64)));
        assert_eq!(
            domain.verifying_contract,
            Some(Address::from_slice(&[0x11; 20]))
        );
    }
}
