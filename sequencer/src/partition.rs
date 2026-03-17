// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Block-range partition retry: when a log query fails with a "long block range" RPC error,
//! split the range in half and retry. Shared by the input reader (safe-head advancement)
//! and the batch submitter (latest batch submitted scan).
//!
//! This module is stateless: callers pass the retry error codes explicitly. There is no
//! global mutable state; `RunConfig` owns the codes and passes them down via configs.

/// Default RPC error codes that trigger partition retry (e.g. Infura -32005, Alchemy -32600/-32602, QuickNode -32616).
pub const DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES: &[&str] = &["-32005", "-32600", "-32602", "-32616"];

use alloy::contract::Error as ContractError;
use alloy::contract::Event;
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use alloy::sol_types::SolEvent;
use alloy_primitives::Address;
use async_recursion::async_recursion;
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;
use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;

#[async_recursion]
pub async fn get_input_added_events(
    provider: &impl Provider,
    app_address_filter: Address,
    input_box_address: &Address,
    start_block: u64,
    end_block: u64,
    long_block_range_error_codes: &[String],
) -> Result<Vec<(InputAdded, alloy::rpc::types::Log)>, Vec<ContractError>> {
    let event = Event::new_sol(provider, input_box_address)
        .from_block(start_block)
        .to_block(end_block)
        .event(InputAdded::SIGNATURE)
        .topic1(app_address_filter.into_word());

    match event.query().await {
        Ok(logs) => Ok(logs),
        Err(e) => {
            if should_retry_with_partition(&e, long_block_range_error_codes) {
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
                    long_block_range_error_codes,
                )
                .await;
                let second = get_input_added_events(
                    provider,
                    app_address_filter,
                    input_box_address,
                    middle + 1,
                    end_block,
                    long_block_range_error_codes,
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

pub fn decode_evm_advance_input(input: &[u8]) -> Result<EvmAdvanceCall, String> {
    EvmAdvanceCall::abi_decode(input).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};
    use alloy_sol_types::SolCall;
    use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;

    use super::{decode_evm_advance_input, error_message_matches_retry_codes};

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

    #[test]
    fn decode_evm_advance_input_round_trips() {
        let encoded = EvmAdvanceCall {
            chainId: U256::from(31337_u64),
            appContract: address!("0x1111111111111111111111111111111111111111"),
            msgSender: address!("0x2222222222222222222222222222222222222222"),
            blockNumber: U256::from(99_u64),
            blockTimestamp: U256::from(1234_u64),
            prevRandao: U256::from(7_u64),
            index: U256::from(3_u64),
            payload: vec![0xaa, 0xbb].into(),
        }
        .abi_encode();

        let decoded = decode_evm_advance_input(encoded.as_slice()).expect("decode evm advance");
        assert_eq!(
            decoded.msgSender,
            address!("0x2222222222222222222222222222222222222222")
        );
        assert_eq!(decoded.blockNumber, U256::from(99_u64));
        assert_eq!(decoded.payload.as_ref(), &[0xaa, 0xbb]);
    }
}
