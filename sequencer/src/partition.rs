// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Block-range partition retry: when a log query fails with a "long block range" RPC error,
//! split the range in half and retry. Shared by the input reader (safe-head advancement)
//! and the batch submitter (latest batch submitted scan).
//!
//! Config is static: call `init()` once at startup (e.g. from runtime); then input reader
//! and batch poster just call `get_input_added_events(...)` with no config.
//!
//! Tests that run the full runtime (and thus call `init()`) share this static; first init wins.
//! To avoid cross-test ordering effects, run sequencer tests sequentially, e.g.
//! `cargo test -p sequencer -- --test-threads=1` or `just test-sequencer`.

/// Default RPC error codes that trigger partition retry (e.g. Infura -32005, Alchemy -32600/-32602, QuickNode -32616).
pub const DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES: &[&str] = &["-32005", "-32600", "-32602", "-32616"];

use std::sync::{OnceLock, RwLock};

use alloy::contract::Error as ContractError;
use alloy::contract::Event;
use alloy::providers::Provider;
use alloy::sol_types::SolEvent;
use alloy_primitives::Address;
use async_recursion::async_recursion;
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;

/// Config for partition retry. Set once via `init()`; then used by `get_input_added_events`.
#[derive(Debug, Clone)]
pub struct PartitionConfig {
    /// RPC error codes that trigger get_logs retry with a shorter block range.
    pub long_block_range_error_codes: Vec<String>,
}

impl PartitionConfig {
    pub fn new(long_block_range_error_codes: Vec<String>) -> Self {
        Self {
            long_block_range_error_codes,
        }
    }
}

/// Lazy default, then overwritten by `init()`. No leak: default is an empty config;
/// when `init()` is called we replace it with the real config.
static CONFIG: OnceLock<RwLock<PartitionConfig>> = OnceLock::new();

fn config_storage() -> &'static RwLock<PartitionConfig> {
    CONFIG.get_or_init(|| {
        RwLock::new(PartitionConfig::new(
            DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ))
    })
}

/// Initializes the partition config. Call once at startup (e.g. from runtime).
/// Overwrites the default (empty codes) with the given config.
pub fn init(config: PartitionConfig) {
    *config_storage().write().expect("partition config lock") = config;
}

fn config_codes() -> Vec<String> {
    config_storage()
        .read()
        .expect("partition config lock")
        .long_block_range_error_codes
        .clone()
}

#[async_recursion]
pub async fn get_input_added_events(
    provider: &impl Provider,
    app_address_filter: Address,
    input_box_address: &Address,
    start_block: u64,
    end_block: u64,
) -> Result<Vec<(InputAdded, alloy::rpc::types::Log)>, Vec<ContractError>> {
    let codes = config_codes();
    let codes = codes.as_slice();
    let event = Event::new_sol(provider, input_box_address)
        .from_block(start_block)
        .to_block(end_block)
        .event(InputAdded::SIGNATURE)
        .topic1(app_address_filter.into_word());

    match event.query().await {
        Ok(logs) => Ok(logs),
        Err(e) => {
            if should_retry_with_partition(&e, codes) {
                if start_block >= end_block {
                    return Err(vec![e]);
                }
                let middle = start_block + (end_block - start_block) / 2;
                let first = get_input_added_events(
                    provider,
                    app_address_filter,
                    input_box_address,
                    start_block,
                    middle,
                )
                .await;
                let second = get_input_added_events(
                    provider,
                    app_address_filter,
                    input_box_address,
                    middle + 1,
                    end_block,
                )
                .await;

                match (first, second) {
                    (Ok(mut a), Ok(b)) => {
                        a.extend(b);
                        Ok(a)
                    }
                    (Err(mut a), Err(b)) => {
                        a.extend(b);
                        Err(a)
                    }
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            } else {
                Err(vec![e])
            }
        }
    }
}

fn should_retry_with_partition(err: &ContractError, codes: &[String]) -> bool {
    error_message_matches_retry_codes(&format!("{err:?}"), codes)
}

pub fn error_message_matches_retry_codes(error_message: &str, codes: &[String]) -> bool {
    codes.iter().any(|c| error_message.contains(c))
}

#[cfg(test)]
mod tests {
    use super::error_message_matches_retry_codes;

    #[test]
    fn error_message_matches_retry_codes_returns_true_when_message_contains_code() {
        assert!(error_message_matches_retry_codes(
            "RPC error: block range too large",
            &["block range".to_string(), "timeout".to_string()]
        ));
        assert!(error_message_matches_retry_codes(
            "timeout after 30s",
            &["timeout".to_string()]
        ));
    }

    #[test]
    fn error_message_matches_retry_codes_returns_false_when_no_match() {
        assert!(!error_message_matches_retry_codes(
            "connection refused",
            &["block range".to_string(), "timeout".to_string()]
        ));
        assert!(!error_message_matches_retry_codes("ok", &[]));
    }
}
