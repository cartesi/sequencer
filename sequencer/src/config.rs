// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;
use clap::Parser;

pub const DOMAIN_NAME: &str = "CartesiAppSequencer";
pub const DOMAIN_VERSION: &str = "1";

const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:3000";
const DEFAULT_DB_PATH: &str = "sequencer.db";

/// `-32005` Infura
/// `-32600`, `-32602` Alchemy
/// `-32616` QuickNode
const DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES: &[&str] = &["-32005", "-32600", "-32602", "-32616"];

#[derive(Debug, Clone, Parser)]
#[command(
    name = "sequencer",
    about = "Deterministic sequencer prototype with low-latency soft confirmations",
    version,
    after_help = "Examples:\n  sequencer --eth-rpc-url http://127.0.0.1:8545 --domain-chain-id 31337 --domain-verifying-contract 0x1111111111111111111111111111111111111111\n  sequencer --http-addr 0.0.0.0:3000 --db-path ./sequencer.db --eth-rpc-url https://eth.example --domain-chain-id 1 --domain-verifying-contract 0x4444444444444444444444444444444444444444"
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
        let err = RunConfig::try_parse_from(["sequencer"]).expect_err("domain inputs are required");

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
