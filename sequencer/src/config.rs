// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;
use clap::{ArgGroup, Parser};

pub const DOMAIN_NAME: &str = "CartesiAppSequencer";
pub const DOMAIN_VERSION: &str = "1";

const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:3000";
const DEFAULT_DATA_DIR: &str = "sequencer-data";
const DB_FILENAME: &str = "sequencer.db";

/// Shared L1 / InputBox configuration used by both the input reader and the batch submitter.
///
/// Built once at startup from `RunConfig` plus the discovered InputBox address, so RPC URL,
/// InputBox address, and app address are defined in a single place and not duplicated across
/// component configs.
#[derive(Debug, Clone)]
pub struct L1Config {
    pub eth_rpc_url: String,
    pub input_box_address: Address,
    pub app_address: Address,
    pub batch_submitter_private_key: String,
    pub batch_submitter_address: Address,
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "sequencer",
    about = "Deterministic sequencer prototype with low-latency soft confirmations.\n\n\
             All options can also be set via environment variables (shown in brackets).",
    version,
    after_help = "\
Examples:
  sequencer \\
    --eth-rpc-url http://127.0.0.1:8545 \\
    --chain-id 31337 \\
    --app-address 0x1111111111111111111111111111111111111111 \\
    --batch-submitter-private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

  SEQ_ETH_RPC_URL=http://127.0.0.1:8545 \\
  SEQ_CHAIN_ID=31337 \\
  SEQ_APP_ADDRESS=0x1111111111111111111111111111111111111111 \\
  SEQ_BATCH_SUBMITTER_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \\
  sequencer\
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
    #[arg(long, env = "SEQ_DATA_DIR", default_value = DEFAULT_DATA_DIR, value_parser = parse_non_empty_string)]
    pub data_dir: String,
    #[arg(long, env = "SEQ_ETH_RPC_URL", value_parser = parse_non_empty_string)]
    pub eth_rpc_url: String,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    #[arg(long, env = "SEQ_LONG_BLOCK_RANGE_ERROR_CODES", value_delimiter = ',', default_values = crate::partition::DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES)]
    pub long_block_range_error_codes: Vec<String>,
    /// Expected chain ID. Validated against the RPC at startup.
    #[arg(long, env = "SEQ_CHAIN_ID")]
    pub chain_id: u64,
    /// Application (EIP-712 verifying contract) address.
    #[arg(long, env = "SEQ_APP_ADDRESS", value_parser = parse_address)]
    pub app_address: Address,
    /// Hex-encoded private key for the batch submitter.
    #[arg(
        long,
        env = "SEQ_BATCH_SUBMITTER_PRIVATE_KEY",
        group = "batch_submitter_key_source"
    )]
    batch_submitter_private_key: Option<String>,
    /// Path to a file whose first line contains the batch submitter private key.
    #[arg(
        long,
        env = "SEQ_BATCH_SUBMITTER_PRIVATE_KEY_FILE",
        group = "batch_submitter_key_source"
    )]
    batch_submitter_private_key_file: Option<String>,

    /// How often the batch submitter polls for new work when idle.
    #[arg(
        long,
        env = "SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS",
        default_value = "5000"
    )]
    pub batch_submitter_idle_poll_interval_ms: u64,

    /// Additional confirmations to wait for after a batch-submission tx is included on L1.
    #[arg(
        long,
        env = "SEQ_BATCH_SUBMITTER_CONFIRMATION_DEPTH",
        default_value = "2"
    )]
    pub batch_submitter_confirmation_depth: u64,

    /// Blocks before MAX_WAIT_BLOCKS to trigger preemptive recovery.
    /// The danger threshold is MAX_WAIT_BLOCKS minus this margin.
    /// Must be less than MAX_WAIT_BLOCKS (validated at startup).
    #[arg(long, env = "SEQ_PREEMPTIVE_MARGIN_BLOCKS", default_value = "75")]
    pub preemptive_margin_blocks: u64,

    /// Assumed L1 block time in seconds. Used to estimate block progression from
    /// wall-clock time when the L1 provider is unreachable.
    #[arg(long, env = "SEQ_SECONDS_PER_BLOCK", default_value = "12")]
    pub seconds_per_block: u64,
}

impl RunConfig {
    pub fn build_domain(&self) -> Eip712Domain {
        Eip712Domain {
            name: Some(DOMAIN_NAME.into()),
            version: Some(DOMAIN_VERSION.into()),
            chain_id: Some(U256::from(self.chain_id)),
            verifying_contract: Some(self.app_address),
            salt: None,
        }
    }

    /// Full path to the SQLite database file inside `data_dir`.
    pub fn db_path(&self) -> String {
        std::path::Path::new(&self.data_dir)
            .join(DB_FILENAME)
            .to_string_lossy()
            .into_owned()
    }

    /// Resolve the batch submitter private key from either the inline value or a key file.
    pub fn resolve_private_key(&self) -> Result<String, std::io::Error> {
        if let Some(file) = &self.batch_submitter_private_key_file {
            let contents = std::fs::read_to_string(file)?;
            Ok(contents.lines().next().unwrap_or("").trim().to_string())
        } else {
            Ok(self
                .batch_submitter_private_key
                .clone()
                .expect("batch submitter private key is required by CLI arg group"))
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
        return Err("address must be 0x-prefixed".to_string());
    }

    let bytes =
        alloy_primitives::hex::decode(raw).map_err(|err| format!("invalid address hex: {err}"))?;
    if bytes.len() != 20 {
        return Err("address must be 20 bytes".to_string());
    }
    Ok(Address::from_slice(&bytes))
}

#[cfg(test)]
mod tests {
    use super::{DOMAIN_NAME, DOMAIN_VERSION, RunConfig};
    use alloy_primitives::{Address, U256};
    use clap::Parser;

    const TEST_ARGS: [&str; 9] = [
        "sequencer",
        "--eth-rpc-url",
        "http://127.0.0.1:8545",
        "--chain-id",
        "31337",
        "--app-address",
        "0x1111111111111111111111111111111111111111",
        "--batch-submitter-private-key",
        "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ];

    #[test]
    fn run_config_requires_essential_inputs() {
        let err = RunConfig::try_parse_from([
            "sequencer",
            "--batch-submitter-private-key",
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ])
        .expect_err("essential inputs are required");

        let message = err.to_string();
        assert!(message.contains("--eth-rpc-url"));
        assert!(message.contains("--chain-id"));
        assert!(message.contains("--app-address"));
    }

    #[test]
    fn run_config_uses_default_block_range_retry_codes() {
        let config = RunConfig::try_parse_from(TEST_ARGS).expect("parse run config");

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
    fn run_config_defaults_batch_submitter_confirmation_depth_to_two() {
        let config = RunConfig::try_parse_from(TEST_ARGS).expect("parse run config");

        assert_eq!(config.batch_submitter_confirmation_depth, 2);
    }

    #[test]
    fn run_config_builds_domain_with_fixed_name_and_version() {
        let config = RunConfig::try_parse_from(TEST_ARGS).expect("parse run config");

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
