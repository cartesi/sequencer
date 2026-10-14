// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Block-range partition retry: when a log query fails with a "long block range" RPC error,
//! split the range in half and retry. Shared by the input reader (safe-head advancement)
//! and the batch submitter (latest batch submitted scan).
//!
//! This module is stateless: callers pass the retry error codes explicitly. There is no
//! global mutable state; `RunConfig` owns the codes and passes them down via configs.

/// Default RPC error codes that trigger partition retry (e.g. Infura -32005,
/// Alchemy -32600/-32602, QuickNode -32616, Goldsky / some JSON-RPC proxies
/// -32012).
///
/// **Overloaded codes (accepted).** `-32005` and `-32012` are not range-only
/// across providers: Alloy's transport also treats bare `-32005` as Infura
/// rate-limit, and `-32012` + `"credits"` as QuickNode credit rate-limit.
/// We still match on the bare code — the same policy already used for
/// `-32005` — because range-error messages are not stable across proxies
/// (Goldsky vs custom gateways). A persistent QuickNode credits limit that
/// survives Alloy's own retries can therefore fan an N-block scan into
/// `2N−1` partition queries before failing; operators on QuickNode can drop
/// `-32012` from `CARTESI_SEQUENCER_LONG_BLOCK_RANGE_ERROR_CODES` if that
/// matters. We deliberately do **not** add message-marker heuristics here.
pub const DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES: &[&str] =
    &["-32005", "-32012", "-32600", "-32602", "-32616"];

use alloy::contract::Error as ContractError;
use alloy::contract::Event;
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use alloy::sol_types::SolEvent;
use alloy_primitives::{Address, B256};
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;
use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;

/// Failure of the partitioned `InputAdded` fetch.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GetLogsError {
    /// The first RPC failure the bisect could not (or may not) split away.
    /// The scan short-circuits on it — matching the watchdog — so sub-ranges
    /// after the failing one are never queried.
    #[error(transparent)]
    Rpc(#[from] ContractError),
    /// A returned log carried no block number. `eth_getLogs` over a mined
    /// range cannot produce this from a consistent node, and the L1 order
    /// every consumer depends on cannot be established without it (the
    /// watchdog hard-errors on the same condition).
    #[error(
        "InputAdded log missing block_number (tx {transaction_hash:?}, log_index {log_index:?})"
    )]
    MissingBlockNumber {
        transaction_hash: Option<B256>,
        log_index: Option<u64>,
    },
}

/// Fetch every `InputAdded` log for `app_address_filter`, splitting the range
/// on RPC long-range errors. The result is in **RPC-return order** — partition
/// sub-ranges are concatenated and a single `eth_getLogs` may itself reorder —
/// so consumers go through [`get_input_added_events_ordered`], which folds the
/// L1-order sort in; this raw form is the partition primitive.
async fn get_input_added_events(
    provider: &impl Provider,
    app_address_filter: Address,
    input_box_address: &Address,
    start_block: u64,
    end_block: u64,
    long_block_range_error_codes: &[String],
) -> Result<Vec<(InputAdded, alloy::rpc::types::Log)>, ContractError> {
    bisect_block_range(
        start_block,
        end_block,
        |e| should_retry_with_partition(e, long_block_range_error_codes),
        |from, to| {
            let event = Event::new_sol(provider, input_box_address)
                .from_block(from)
                .to_block(to)
                .event(InputAdded::SIGNATURE)
                .topic1(app_address_filter.into_word());
            async move { event.query().await }
        },
    )
    .await
}

/// Query `[start_block, end_block]`, splitting a range in half whenever its
/// error is `splittable` (and it spans more than one block).
///
/// Depth-first, left-first worklist (push right, then push left): the query
/// order and the concatenation order match the watchdog's recursive `scan`
/// (`watchdog/l1.lua`) exactly; `tests/fixtures/l1_partition_vector.json`
/// pins the query order and outcome on both sides. A terminal error
/// short-circuits — once one sub-range fails for good, the ranges after it
/// are not queried.
async fn bisect_block_range<T, E, Fut>(
    start_block: u64,
    end_block: u64,
    mut splittable: impl FnMut(&E) -> bool,
    mut query: impl FnMut(u64, u64) -> Fut,
) -> Result<Vec<T>, E>
where
    Fut: Future<Output = Result<Vec<T>, E>>,
{
    let mut items = Vec::new();
    let mut ranges = vec![(start_block, end_block)];
    while let Some((from, to)) = ranges.pop() {
        match query(from, to).await {
            Ok(chunk) => items.extend(chunk),
            Err(e) if from < to && splittable(&e) => {
                let middle = from + (to - from) / 2;
                ranges.push((middle + 1, to));
                ranges.push((from, middle));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(items)
}

fn should_retry_with_partition(err: &ContractError, codes: &[String]) -> bool {
    error_message_matches_retry_codes(&format!("{err:?}"), codes)
}

fn error_message_matches_retry_codes(error_message: &str, codes: &[String]) -> bool {
    codes.iter().any(|c| error_message.contains(c))
}

/// Total order for merged logs `(block, tx index, log index)`, the key the
/// watchdog's `sort_logs` also uses; only this side runs the shared log-sort
/// vector. The caller has already refused a log without a block number.
fn l1_order_key(log: &alloy::rpc::types::Log) -> (u64, u64, u64) {
    (
        log.block_number
            .expect("caller guards: every log has a block number"),
        log.transaction_index.unwrap_or(0),
        log.log_index.unwrap_or(0),
    )
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
) -> Result<Vec<(InputAdded, alloy::rpc::types::Log)>, GetLogsError> {
    let mut events = get_input_added_events(
        provider,
        app_address_filter,
        input_box_address,
        start_block,
        end_block,
        long_block_range_error_codes,
    )
    .await?;
    // L1 order needs provenance: a mined log always carries its block number,
    // so refuse to sort (and mis-place) one that lacks it. The tx/log indices
    // may default to 0 — watchdog parity (Lua: `transactionIndex or "0x0"`,
    // but a missing `blockNumber` hard-errors there too).
    if let Some((_, log)) = events.iter().find(|(_, log)| log.block_number.is_none()) {
        return Err(GetLogsError::MissingBlockNumber {
            transaction_hash: log.transaction_hash,
            log_index: log.log_index,
        });
    }
    events.sort_by_key(|(_, log)| l1_order_key(log));
    Ok(events)
}

/// Decode the `EvmAdvance` calldata carried in `InputAdded.input` — the shared
/// decode entry point (parity counterpart of the watchdog's
/// `abi.decode_evm_advance_call`). Returns the typed decode error; callers
/// attach the log context (tx hash, index) only they have.
pub(crate) fn decode_evm_advance_input(
    input: &[u8],
) -> Result<EvmAdvanceCall, alloy::sol_types::Error> {
    EvmAdvanceCall::abi_decode(input)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::{U256, address};
    use alloy_sol_types::SolCall;
    use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;
    use serde::Deserialize;

    use super::{
        DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES, bisect_block_range, decode_evm_advance_input,
        error_message_matches_retry_codes, l1_order_key,
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

    #[tokio::test]
    async fn partition_vector_bisect_matches_watchdog() {
        let vector = load_partition_vector();
        let codes = vector.long_block_range_error_codes;
        for scenario in vector.scenarios {
            let expected_calls: Vec<(u64, u64)> = scenario
                .expect_calls
                .iter()
                .map(|pair| (pair[0], pair[1]))
                .collect();
            // An empty range never reaches the bisect: both production callers
            // return before the fetch (reader `read_safe_inputs`, poster
            // `observed_submitted_batch_nonces`), like the watchdog's `from <= to`.
            if scenario.start_block > scenario.end_block {
                assert!(
                    expected_calls.is_empty() && scenario.expect_ok,
                    "scenario {}",
                    scenario.name
                );
                continue;
            }

            let fails = fail_map(&scenario.fail_ranges);
            let mut calls = Vec::new();
            let result: Result<Vec<()>, String> = bisect_block_range(
                scenario.start_block,
                scenario.end_block,
                |err: &String| error_message_matches_retry_codes(err, &codes),
                |from, to| {
                    calls.push((from, to));
                    std::future::ready(fails.get(&(from, to)).cloned().map_or(Ok(Vec::new()), Err))
                },
            )
            .await;

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
    fn log_sort_vector_orders_by_l1_position() {
        let log_sort = load_partition_vector().log_sort;
        let mut logs: Vec<alloy::rpc::types::Log> = log_sort
            .unsorted
            .iter()
            .map(|e| alloy::rpc::types::Log {
                block_number: Some(parse_hex_quantity(&e.block_number)),
                transaction_index: Some(parse_hex_quantity(&e.transaction_index)),
                log_index: Some(parse_hex_quantity(&e.log_index)),
                ..Default::default()
            })
            .collect();
        logs.sort_by_key(l1_order_key);
        let blocks: Vec<u64> = logs.iter().filter_map(|log| log.block_number).collect();
        let indices: Vec<u64> = logs.iter().filter_map(|log| log.log_index).collect();
        let quantities = |hex: &[String]| -> Vec<u64> {
            hex.iter().map(|value| parse_hex_quantity(value)).collect()
        };
        assert_eq!(blocks, quantities(&log_sort.expect_block_order));
        assert_eq!(indices, quantities(&log_sort.expect_log_index_order));
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
    fn default_codes_include_proxy_range_exceeded() {
        assert!(
            DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES.contains(&"-32012"),
            "default codes must include -32012 (JSON-RPC proxy range limit)"
        );
    }

    #[test]
    fn proxy_range_exceeded_error_triggers_retry() {
        let msg = r#"-32012: getLogs request exceeded max allowed range, data: {"code":"ErrGetLogsExceededMaxAllowedRange"}"#;
        let codes: Vec<String> = DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES
            .iter()
            .map(|c| (*c).to_string())
            .collect();
        assert!(
            error_message_matches_retry_codes(msg, &codes),
            "proxy -32012 error must match retry codes"
        );
    }

    #[test]
    fn real_alloy_range_error_matches_retry_codes() {
        // The bisect classifier reads the JSON-RPC code out of ContractError's
        // *Debug* output — an unstable cross-crate coupling. This pins it
        // against a real alloy error so an alloy Debug-format change fails
        // here at upgrade time instead of silently disabling partition retry.
        let payload = alloy::rpc::json_rpc::ErrorPayload {
            code: -32005,
            message: "query returned more than 10000 results".into(),
            data: None,
        };
        let err =
            super::ContractError::TransportError(alloy::transports::RpcError::ErrorResp(payload));
        let codes: Vec<String> = DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES
            .iter()
            .map(|c| (*c).to_string())
            .collect();
        assert!(
            super::should_retry_with_partition(&err, &codes),
            "a real -32005 ErrorResp must classify as partition-retryable"
        );
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
