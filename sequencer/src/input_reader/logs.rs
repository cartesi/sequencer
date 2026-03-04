// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy::contract::Error as ContractError;
use alloy::contract::Event;
use alloy::providers::Provider;
use alloy::sol_types::SolEvent;
use alloy_primitives::Address;
use async_recursion::async_recursion;
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;

#[async_recursion]
pub(crate) async fn get_input_added_events(
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

pub(crate) fn error_message_matches_retry_codes(error_message: &str, codes: &[String]) -> bool {
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
