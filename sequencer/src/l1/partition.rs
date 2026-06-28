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

/// Fetch every `InputAdded` log for `app_address_filter`, splitting the range
/// on RPC long-range errors. The result is in **RPC-return order** — partition
/// sub-ranges are concatenated and a single `eth_getLogs` may itself reorder —
/// so any consumer that relies on L1 order must sort. Prefer
/// [`get_input_added_events_ordered`], which folds that sort in; this raw form
/// is the partition primitive (and the recursion's own building block).
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

pub fn error_message_matches_retry_codes(error_message: &str, codes: &[String]) -> bool {
    codes.iter().any(|c| error_message.contains(c))
}

/// Simulates partitioned `eth_getLogs` bisect (same rules as `watchdog/l1_reader.lua`).
/// `get_logs` returns `Ok` for a successful leaf query or `Err(message)` on RPC failure.
/// Records every attempted `(from_block, to_block)` in call order.
pub fn simulate_partitioned_get_logs<F>(
    start_block: u64,
    end_block: u64,
    long_block_range_error_codes: &[String],
    mut get_logs: F,
) -> (Vec<(u64, u64)>, Result<(), String>)
where
    F: FnMut(u64, u64) -> Result<(), String>,
{
    if end_block < start_block {
        return (Vec::new(), Ok(()));
    }

    fn go<F>(
        start_block: u64,
        end_block: u64,
        long_block_range_error_codes: &[String],
        get_logs: &mut F,
        calls: &mut Vec<(u64, u64)>,
    ) -> Result<(), String>
    where
        F: FnMut(u64, u64) -> Result<(), String>,
    {
        calls.push((start_block, end_block));
        match get_logs(start_block, end_block) {
            Ok(()) => Ok(()),
            Err(err) => {
                if start_block < end_block
                    && error_message_matches_retry_codes(&err, long_block_range_error_codes)
                {
                    let middle = start_block + (end_block - start_block) / 2;
                    go(
                        start_block,
                        middle,
                        long_block_range_error_codes,
                        get_logs,
                        calls,
                    )?;
                    go(
                        middle + 1,
                        end_block,
                        long_block_range_error_codes,
                        get_logs,
                        calls,
                    )?;
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    let mut calls = Vec::new();
    let result = go(
        start_block,
        end_block,
        long_block_range_error_codes,
        &mut get_logs,
        &mut calls,
    );
    (calls, result)
}

/// Total order for merged logs (block, tx index, log index) — matches Lua `sort_logs`.
pub fn sort_logs_by_l1_order<T, FBlock, FTx, FLog>(
    logs: &mut [T],
    block: FBlock,
    tx_index: FTx,
    log_index: FLog,
) where
    FBlock: Fn(&T) -> u64,
    FTx: Fn(&T) -> u64,
    FLog: Fn(&T) -> u64,
{
    logs.sort_by(|a, b| {
        block(a)
            .cmp(&block(b))
            .then_with(|| tx_index(a).cmp(&tx_index(b)))
            .then_with(|| log_index(a).cmp(&log_index(b)))
    });
}

/// [`get_input_added_events`] with the logs returned in canonical L1 order
/// `(block, tx_index, log_index)`. The single fetch path for every consumer that
/// depends on L1 ordering — the reader's dense-index contiguity witness and the
/// submitter's batch-nonce fold both require it — so the sort lives here once
/// instead of being duplicated (and forgettable) at each call site. Matches the
/// order the watchdog applies on its side.
pub(crate) async fn get_input_added_events_ordered(
    provider: &impl Provider,
    app_address_filter: Address,
    input_box_address: &Address,
    start_block: u64,
    end_block: u64,
    long_block_range_error_codes: &[String],
) -> Result<Vec<(InputAdded, alloy::rpc::types::Log)>, Vec<ContractError>> {
    let mut events = get_input_added_events(
        provider,
        app_address_filter,
        input_box_address,
        start_block,
        end_block,
        long_block_range_error_codes,
    )
    .await?;
    sort_logs_by_l1_order(
        &mut events,
        |(_, log)| log.block_number.unwrap_or(0),
        |(_, log)| log.transaction_index.unwrap_or(0),
        |(_, log)| log.log_index.unwrap_or(0),
    );
    Ok(events)
}

pub fn decode_evm_advance_input(input: &[u8]) -> Result<EvmAdvanceCall, String> {
    EvmAdvanceCall::abi_decode(input).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::{U256, address};
    use alloy_sol_types::SolCall;
    use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;
    use serde::Deserialize;

    use super::{
        DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES, decode_evm_advance_input,
        error_message_matches_retry_codes, simulate_partitioned_get_logs, sort_logs_by_l1_order,
    };

    const PARTITION_VECTOR: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/fixtures/l1_partition_vector.json"
    ));

    #[derive(Debug, Deserialize)]
    struct FailRange {
        from: u64,
        to: u64,
        message: String,
    }

    #[derive(Debug, Deserialize)]
    struct PartitionScenario {
        name: String,
        start_block: u64,
        end_block: u64,
        fail_ranges: Vec<FailRange>,
        expect_calls: Vec<[u64; 2]>,
        expect_ok: bool,
    }

    #[derive(Debug, Deserialize)]
    struct LogSortFixture {
        unsorted: Vec<LogSortEntry>,
        expect_block_order: Vec<String>,
        expect_log_index_order: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    struct LogSortEntry {
        #[serde(rename = "blockNumber")]
        block_number: String,
        #[serde(rename = "transactionIndex", default)]
        transaction_index: String,
        #[serde(rename = "logIndex", default)]
        log_index: String,
    }

    #[derive(Debug, Deserialize)]
    struct PartitionVector {
        long_block_range_error_codes: Vec<String>,
        scenarios: Vec<PartitionScenario>,
        log_sort: LogSortFixture,
    }

    fn load_partition_vector() -> PartitionVector {
        serde_json::from_str(PARTITION_VECTOR).expect("parse l1_partition_vector.json")
    }

    fn fail_map(fail_ranges: &[FailRange]) -> HashMap<(u64, u64), String> {
        fail_ranges
            .iter()
            .map(|r| ((r.from, r.to), r.message.clone()))
            .collect()
    }

    #[test]
    fn fixture_default_codes_match_rust_defaults() {
        let vector = load_partition_vector();
        let expected: Vec<String> = DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES
            .iter()
            .map(|c| (*c).to_string())
            .collect();
        assert_eq!(vector.long_block_range_error_codes, expected);
    }

    #[test]
    fn partition_vector_simulation_matches_watchdog() {
        let vector = load_partition_vector();
        for scenario in vector.scenarios {
            let fails = fail_map(&scenario.fail_ranges);
            let codes = vector.long_block_range_error_codes.clone();
            let (calls, result) = simulate_partitioned_get_logs(
                scenario.start_block,
                scenario.end_block,
                &codes,
                |from, to| {
                    if let Some(message) = fails.get(&(from, to)) {
                        Err(message.clone())
                    } else {
                        Ok(())
                    }
                },
            );

            let expected_calls: Vec<(u64, u64)> = scenario
                .expect_calls
                .iter()
                .map(|pair| (pair[0], pair[1]))
                .collect();
            assert_eq!(calls, expected_calls, "scenario {}", scenario.name);
            assert_eq!(
                result.is_ok(),
                scenario.expect_ok,
                "scenario {}",
                scenario.name
            );
        }
    }

    #[test]
    fn log_sort_vector_matches_watchdog_order() {
        let vector = load_partition_vector();
        let mut logs = vector.log_sort.unsorted;
        sort_logs_by_l1_order(
            &mut logs,
            |e| parse_hex_quantity(&e.block_number),
            |e| parse_hex_quantity(&e.transaction_index),
            |e| parse_hex_quantity(&e.log_index),
        );
        let blocks: Vec<String> = logs.iter().map(|e| e.block_number.clone()).collect();
        let indices: Vec<String> = logs.iter().map(|e| e.log_index.clone()).collect();
        assert_eq!(blocks, vector.log_sort.expect_block_order);
        assert_eq!(indices, vector.log_sort.expect_log_index_order);
    }

    fn parse_hex_quantity(value: &str) -> u64 {
        u64::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16).expect("hex quantity")
    }

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
