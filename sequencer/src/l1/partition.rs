// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Range-partitioned `InputAdded` fetch, shared by the input reader and the
//! batch poster: a range's logs come back whole or not at all, in L1 order,
//! decoded.

use std::future::Future;

use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::TransportError;
use alloy_primitives::{Address, B256};
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;
use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;

/// Failure of the partitioned `InputAdded` fetch.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GetLogsError {
    /// `eth_getLogs` failed on a single block, which no split can narrow.
    #[error("eth_getLogs failed at block {block}: {source}")]
    Rpc {
        block: u64,
        #[source]
        source: TransportError,
    },
    /// A returned log lacks a position field. A mined log always carries all
    /// three, and the L1 order every consumer depends on needs them.
    #[error(
        "InputAdded log missing {field} (tx {transaction_hash:?}, block {block_number:?}, \
         log_index {log_index:?})"
    )]
    MissingLogPosition {
        field: &'static str,
        transaction_hash: Option<B256>,
        block_number: Option<u64>,
        log_index: Option<u64>,
    },
    /// A log the topic filter matched does not decode as `InputAdded`.
    #[error(
        "undecodable InputAdded log (tx {transaction_hash:?}, log_index {log_index:?}): {source}"
    )]
    Decode {
        transaction_hash: Option<B256>,
        log_index: Option<u64>,
        #[source]
        source: Box<alloy::sol_types::Error>,
    },
}

/// Every `InputAdded` log for `app_address` in `[start_block, end_block]`,
/// sorted into L1 order `(block, tx index, log index)` and decoded. The
/// reader's dense-index contiguity witness and the poster's batch-nonce fold
/// both need that order, and a single `eth_getLogs` response does not
/// guarantee it. Decoding follows the sort, so an undecodable log is reported
/// as such rather than split like a range failure.
pub(crate) async fn get_input_added_events_ordered(
    provider: &impl Provider,
    app_address: Address,
    input_box_address: &Address,
    start_block: u64,
    end_block: u64,
) -> Result<Vec<(InputAdded, Log)>, GetLogsError> {
    let filter = Filter::new()
        .address(*input_box_address)
        .event_signature(InputAdded::SIGNATURE_HASH)
        .topic1(app_address.into_word());
    let logs = bisect(start_block, end_block, |from, to| {
        let ranged = filter.clone().from_block(from).to_block(to);
        async move { provider.get_logs(&ranged).await }
    })
    .await
    .map_err(|(block, source)| GetLogsError::Rpc { block, source })?;

    let mut positioned = logs
        .into_iter()
        .map(|log| Ok((l1_position(&log)?, log)))
        .collect::<Result<Vec<_>, GetLogsError>>()?;
    positioned.sort_by_key(|(position, _)| *position);
    positioned
        .into_iter()
        .map(|(_, log)| {
            let event = InputAdded::decode_log_data(&log.inner.data).map_err(|source| {
                GetLogsError::Decode {
                    transaction_hash: log.transaction_hash,
                    log_index: log.log_index,
                    source: Box::new(source),
                }
            })?;
            Ok((event, log))
        })
        .collect()
}

/// Fetches `[start, end]`, splitting a range wider than one block in half on
/// any failure and retrying, left half first, so results concatenate in block
/// order. A single block's failure fails the whole call: callers never see a
/// prefix.
///
/// Providers cap ranges, result counts and response sizes under error codes
/// that vary and overlap with rate limits, and a timeout carries no code, so
/// every failure narrows. This protects liveness only: completeness is the
/// reader's index-contiguity and count witnesses, whatever ranges were asked.
/// Assumes one block's logs fit one response; revisit if a single-block query
/// fails persistently on a healthy provider. A persistent failure costs one
/// left spine, `ceil(log2 width) + 1` sequential queries, each sent up to three
/// times when the client resends a rate limit (`l1/provider.rs`). The watchdog's
/// `reader:inputs` (`watchdog/l1.lua`) applies the same rule, and
/// `tests/fixtures/l1_partition_vector.json` pins both.
async fn bisect<T, E, F, Fut>(start: u64, end: u64, mut fetch: F) -> Result<Vec<T>, (u64, E)>
where
    E: std::fmt::Display,
    F: FnMut(u64, u64) -> Fut,
    Fut: Future<Output = Result<Vec<T>, E>>,
{
    let mut out = Vec::new();
    if start > end {
        return Ok(out);
    }
    let mut ranges = vec![(start, end)];
    while let Some((from, to)) = ranges.pop() {
        match fetch(from, to).await {
            Ok(chunk) => out.extend(chunk),
            Err(error) if from < to => {
                tracing::debug!(from, to, %error, "eth_getLogs failed; splitting the range");
                let middle = from + (to - from) / 2;
                ranges.push((middle + 1, to));
                ranges.push((from, middle));
            }
            Err(error) => return Err((from, error)),
        }
    }
    Ok(out)
}

fn l1_position(log: &Log) -> Result<(u64, u64, u64), GetLogsError> {
    let missing = |field| GetLogsError::MissingLogPosition {
        field,
        transaction_hash: log.transaction_hash,
        block_number: log.block_number,
        log_index: log.log_index,
    };
    Ok((
        log.block_number.ok_or_else(|| missing("block_number"))?,
        log.transaction_index
            .ok_or_else(|| missing("transaction_index"))?,
        log.log_index.ok_or_else(|| missing("log_index"))?,
    ))
}

/// Decode the `EvmAdvance` calldata carried in `InputAdded.input` — the shared
/// decode entry point (parity counterpart of the watchdog's
/// `abi.decode_evm_advance`). Returns the typed decode error; callers
/// attach the log context (tx hash, index) only they have.
pub(crate) fn decode_evm_advance_input(
    input: &[u8],
) -> Result<EvmAdvanceCall, alloy::sol_types::Error> {
    EvmAdvanceCall::abi_decode(input)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;
    use alloy_primitives::{Bytes, U256, address};
    use serde::Deserialize;

    use super::*;

    const PARTITION_VECTOR: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/fixtures/l1_partition_vector.json"
    ));

    #[derive(Debug, Deserialize)]
    struct FailRange {
        from: u64,
        to: u64,
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
    struct PartitionVector {
        scenarios: Vec<PartitionScenario>,
    }

    /// The vector the watchdog's `tests/run.lua` replays against its own
    /// `reader:inputs`; here it drives the production worklist.
    #[tokio::test]
    async fn partition_vector_matches_watchdog() {
        let vector: PartitionVector =
            serde_json::from_str(PARTITION_VECTOR).expect("parse l1_partition_vector.json");
        for scenario in vector.scenarios {
            let fails: HashSet<(u64, u64)> = scenario
                .fail_ranges
                .iter()
                .map(|range| (range.from, range.to))
                .collect();
            let mut calls = Vec::new();
            let result = bisect(scenario.start_block, scenario.end_block, |from, to| {
                calls.push((from, to));
                let failed = fails.contains(&(from, to));
                async move {
                    if failed {
                        Err("injected failure")
                    } else {
                        Ok(Vec::<()>::new())
                    }
                }
            })
            .await;

            let expected: Vec<(u64, u64)> = scenario
                .expect_calls
                .iter()
                .map(|pair| (pair[0], pair[1]))
                .collect();
            assert_eq!(calls, expected, "scenario {}", scenario.name);
            assert_eq!(
                result.is_ok(),
                scenario.expect_ok,
                "scenario {}",
                scenario.name
            );
        }
    }

    fn input_added_log(
        index: u64,
        data: Option<Bytes>,
        position: (Option<u64>, Option<u64>, Option<u64>),
    ) -> Log {
        let event = InputAdded {
            appContract: Address::ZERO,
            index: U256::from(index),
            input: Bytes::new(),
        };
        let mut log_data = event.encode_log_data();
        if let Some(data) = data {
            log_data = alloy_primitives::LogData::new_unchecked(log_data.topics().to_vec(), data);
        }
        Log {
            inner: alloy_primitives::Log {
                address: Address::ZERO,
                data: log_data,
            },
            block_number: position.0,
            transaction_index: position.1,
            log_index: position.2,
            transaction_hash: Some(B256::repeat_byte(0xcc)),
            ..Default::default()
        }
    }

    async fn fetch_one_response(logs: Vec<Log>) -> Result<Vec<(InputAdded, Log)>, GetLogsError> {
        let asserter = Asserter::new();
        asserter.push_success(&logs);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let result =
            get_input_added_events_ordered(&provider, Address::ZERO, &Address::ZERO, 1, 4).await;
        assert!(asserter.read_q().is_empty(), "one eth_getLogs, no split");
        result
    }

    #[tokio::test]
    async fn a_reordered_response_comes_back_in_l1_order() {
        let events = fetch_one_response(vec![
            input_added_log(3, None, (Some(4), Some(0), Some(0))),
            input_added_log(1, None, (Some(2), Some(1), Some(4))),
            input_added_log(2, None, (Some(2), Some(1), Some(5))),
            input_added_log(0, None, (Some(2), Some(0), Some(9))),
        ])
        .await
        .expect("a consistent response");
        let indices: Vec<u64> = events.iter().map(|(event, _)| event.index.to()).collect();
        assert_eq!(indices, [0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn a_log_without_a_position_fails_loud() {
        for (field, position) in [
            ("block_number", (None, Some(0), Some(0))),
            ("transaction_index", (Some(2), None, Some(0))),
            ("log_index", (Some(2), Some(0), None)),
        ] {
            let error = fetch_one_response(vec![input_added_log(0, None, position)])
                .await
                .expect_err("a mined log carries its full position");
            assert!(
                matches!(error, GetLogsError::MissingLogPosition { field: f, .. } if f == field),
                "{field}: got {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_undecodable_log_is_reported_not_split() {
        let error = fetch_one_response(vec![input_added_log(
            0,
            Some(Bytes::from_static(&[0x01])),
            (Some(2), Some(0), Some(0)),
        )])
        .await
        .expect_err("garbage data must not decode");
        assert!(
            matches!(error, GetLogsError::Decode { .. }),
            "got {error:?}"
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
