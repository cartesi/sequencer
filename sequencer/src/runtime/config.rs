// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::Address;
use alloy_sol_types::Eip712Domain;
use clap::{ArgGroup, Parser};
use sequencer_core::protocol::{ProtocolTiming, ProtocolTimingError};

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

  CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=http://127.0.0.1:8545 \\
  CARTESI_SEQUENCER_BLOCKCHAIN_ID=31337 \\
  CARTESI_SEQUENCER_APP_ADDRESS=0x1111111111111111111111111111111111111111 \\
  CARTESI_SEQUENCER_AUTH_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \\
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
    #[arg(long, env = "CARTESI_SEQUENCER_HTTP_ADDR", default_value = DEFAULT_HTTP_ADDR, value_parser = parse_non_empty_string)]
    pub http_addr: String,
    #[arg(long, env = "CARTESI_SEQUENCER_DATA_DIR", default_value = DEFAULT_DATA_DIR, value_parser = parse_non_empty_string)]
    pub data_dir: String,
    #[arg(long, env = "CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT", value_parser = parse_non_empty_string)]
    pub eth_rpc_url: String,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    #[arg(long, env = "CARTESI_SEQUENCER_LONG_BLOCK_RANGE_ERROR_CODES", value_delimiter = ',', default_values = crate::l1::partition::DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES)]
    pub long_block_range_error_codes: Vec<String>,
    /// Expected chain ID. Validated against the RPC at startup.
    #[arg(long, env = "CARTESI_SEQUENCER_BLOCKCHAIN_ID")]
    pub chain_id: u64,
    /// Application (EIP-712 verifying contract) address.
    #[arg(long, env = "CARTESI_SEQUENCER_APP_ADDRESS", value_parser = parse_address)]
    pub app_address: Address,
    /// Hex-encoded private key for the batch submitter.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_AUTH_PRIVATE_KEY",
        group = "batch_submitter_key_source"
    )]
    batch_submitter_private_key: Option<String>,
    /// Path to a file whose first line contains the batch submitter private key.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE",
        group = "batch_submitter_key_source"
    )]
    batch_submitter_private_key_file: Option<String>,

    /// How often the batch submitter polls for new work when idle.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS",
        default_value = "5000"
    )]
    pub batch_submitter_idle_poll_interval_ms: u64,

    /// Additional confirmations to wait for after a batch-submission tx is included on L1.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_BATCH_SUBMITTER_CONFIRMATION_DEPTH",
        default_value = "2"
    )]
    pub batch_submitter_confirmation_depth: u64,

    /// Blocks before MAX_WAIT_BLOCKS to trigger preemptive recovery.
    /// The danger threshold is MAX_WAIT_BLOCKS minus this margin.
    /// Must be less than MAX_WAIT_BLOCKS (validated at startup).
    ///
    /// Default 300 (~1h at 12s/block) is sized to give operators meaningful
    /// runway to investigate before the system gives up on the current
    /// batches — see `docs/recovery/README.md` "Step 1: Danger threshold"
    /// for the rationale.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_PREEMPTIVE_MARGIN_BLOCKS",
        default_value = "300"
    )]
    pub preemptive_margin_blocks: u64,

    /// Blocks of safe-head age after which the L1 read view is considered too
    /// stale to trust. Independent of the preemptive margin — a separate
    /// concern ("how old is the cached L1 view before we stop trusting it" vs.
    /// "how much runway before write-side recovery trips"). Must be strictly
    /// less than the danger threshold (validated at startup).
    ///
    /// Default 600 (~2h at 12s/block).
    #[arg(long, env = "CARTESI_SEQUENCER_L1_READ_STALE_AFTER_BLOCKS", default_value = "600", value_parser = clap::value_parser!(u64).range(1..))]
    pub l1_read_stale_after_blocks: u64,

    /// Assumed L1 block time in seconds. Used to estimate block progression from
    /// wall-clock time when the L1 provider is unreachable.
    #[arg(long, env = "CARTESI_SEQUENCER_SECONDS_PER_BLOCK", default_value = "12", value_parser = clap::value_parser!(u64).range(1..))]
    pub seconds_per_block: u64,
}

impl RunConfig {
    pub fn build_domain(&self) -> Eip712Domain {
        sequencer_core::build_input_domain(self.chain_id, self.app_address)
    }

    /// Build a validated [`ProtocolTiming`] from this config's tuning fields.
    /// Pure derivation — does not touch I/O. `max_wait_blocks` is the shared
    /// scheduler constant; the rest come from the operator-tunable CLI args.
    pub fn protocol_timing(&self) -> Result<ProtocolTiming, ProtocolTimingError> {
        ProtocolTiming::try_new(
            sequencer_core::MAX_WAIT_BLOCKS,
            self.preemptive_margin_blocks,
            self.l1_read_stale_after_blocks,
            self.seconds_per_block,
        )
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
    use super::RunConfig;
    use alloy_primitives::{Address, U256};
    use clap::Parser;
    use sequencer_core::{DOMAIN_NAME, DOMAIN_VERSION};

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

    // ── H8 regression: CARTESI_SEQUENCER_SECONDS_PER_BLOCK=0 is rejected by clap ──
    //
    // The H8 hardening added `value_parser = clap::value_parser!(u64).range(1..)`
    // on `seconds_per_block` to prevent a divide-by-zero panic in the
    // wall-clock fallback (`elapsed_secs / seconds_per_block`). Without the
    // value parser, an operator typo would panic the process during the worst
    // possible moment — an L1 outage. These tests lock the clap-level guard.

    fn args_with_seconds_per_block(value: &str) -> Vec<&str> {
        let mut args: Vec<&str> = TEST_ARGS.to_vec();
        args.push("--seconds-per-block");
        args.push(value);
        args
    }

    fn args_with_l1_read_stale_after_blocks(value: &str) -> Vec<&str> {
        let mut args: Vec<&str> = TEST_ARGS.to_vec();
        args.push("--l1-read-stale-after-blocks");
        args.push(value);
        args
    }

    #[test]
    fn run_config_rejects_seconds_per_block_zero() {
        let err = RunConfig::try_parse_from(args_with_seconds_per_block("0"))
            .expect_err("seconds_per_block=0 must be rejected");
        let message = err.to_string();
        // The exact clap wording depends on the version; the specific field is
        // what we want to pin.
        assert!(
            message.contains("--seconds-per-block") || message.contains("seconds_per_block"),
            "error must name the offending field, got: {message}"
        );
    }

    #[test]
    fn run_config_accepts_seconds_per_block_one() {
        // One is the minimum allowed (1..).
        let config =
            RunConfig::try_parse_from(args_with_seconds_per_block("1")).expect("parse succeeds");
        assert_eq!(config.seconds_per_block, 1);
    }

    #[test]
    fn run_config_default_seconds_per_block_is_12() {
        let config = RunConfig::try_parse_from(TEST_ARGS).expect("parse run config");
        assert_eq!(
            config.seconds_per_block, 12,
            "default should reflect Ethereum block time"
        );
    }

    #[test]
    fn run_config_rejects_l1_read_stale_after_blocks_zero() {
        let err = RunConfig::try_parse_from(args_with_l1_read_stale_after_blocks("0"))
            .expect_err("l1_read_stale_after_blocks=0 must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("--l1-read-stale-after-blocks")
                || message.contains("l1_read_stale_after_blocks"),
            "error must name the offending field, got: {message}"
        );
    }

    #[test]
    fn run_config_default_l1_read_stale_after_blocks_is_600() {
        // Independent default (NOT derived from margin) — see field doc.
        let config = RunConfig::try_parse_from(TEST_ARGS).expect("parse run config");
        assert_eq!(config.l1_read_stale_after_blocks, 600);
    }

    #[test]
    fn run_config_accepts_l1_read_stale_after_blocks_one() {
        let config = RunConfig::try_parse_from(args_with_l1_read_stale_after_blocks("1"))
            .expect("parse succeeds");
        assert_eq!(config.l1_read_stale_after_blocks, 1);
    }
}
