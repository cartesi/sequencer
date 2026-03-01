// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use alloy::contract::Error as ContractError;
use alloy::contract::Event;
use alloy::eips::BlockNumberOrTag::Safe;
use alloy::providers::Provider;
use alloy::rpc::types::Topic;
use alloy::sol_types::SolEvent;
use alloy_primitives::Address;
use async_recursion::async_recursion;
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;
use tokio::runtime::Builder;
use tracing::{info, trace};

use crate::storage::{IndexedDirectInput, Storage};

#[derive(Debug, Clone)]
pub struct InputReaderConfig {
    /// RPC URL for the reference node (e.g. L1 or authority node).
    pub rpc_url: String,
    /// Contract address that emits InputAdded (e.g. InputBox).
    pub input_box_address: Address,
    /// Application address to filter inputs (topic1). Only InputAdded events for this app are ingested.
    pub app_address_filter: Address,
    /// First block to scan (e.g. InputBox deployment block).
    pub genesis_block: u64,
    /// Poll interval when no new blocks.
    pub poll_interval: Duration,
    /// RPC error substrings that trigger partition retry for large block ranges.
    pub long_block_range_error_codes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum InputReaderError {
    #[error("provider/transport: {0}")]
    Provider(String),
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("shutdown requested")]
    ShutdownRequested,
}

/// Reads InputAdded events in a block range. Retries with half-range partition on configured RPC errors.
#[async_recursion]
async fn get_input_added_events(
    provider: &impl Provider,
    topic1: Option<&Topic>,
    read_from: &Address,
    start_block: u64,
    end_block: u64,
    long_block_range_error_codes: &[String],
) -> Result<Vec<(InputAdded, alloy::rpc::types::Log)>, Vec<ContractError>> {
    let event = {
        let mut e = Event::new_sol(provider, read_from)
            .from_block(start_block)
            .to_block(end_block)
            .event(InputAdded::SIGNATURE);
        if let Some(t) = topic1 {
            e = e.topic1(t.clone());
        }
        e
    };

    match event.query().await {
        Ok(logs) => Ok(logs),
        Err(e) => {
            if should_retry_with_partition(&e, long_block_range_error_codes) {
                let blocks = 1 + end_block - start_block;
                let half = blocks / 2;
                let middle = start_block + half - 1;

                let first = get_input_added_events(
                    provider,
                    topic1,
                    read_from,
                    start_block,
                    middle,
                    long_block_range_error_codes,
                )
                .await;
                let second = get_input_added_events(
                    provider,
                    topic1,
                    read_from,
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

/// Pure predicate: true if `error_message` contains any of `codes`. Used for partition retry.
/// Exposed for unit tests.
pub(crate) fn error_message_matches_retry_codes(error_message: &str, codes: &[String]) -> bool {
    codes.iter().any(|c| error_message.contains(c))
}

/// Builds a contiguous batch of `IndexedDirectInput` from payloads and block numbers.
/// Exposed for unit tests.
pub(crate) fn build_indexed_direct_input_batch(
    payloads_with_blocks: impl IntoIterator<Item = (Vec<u8>, u64)>,
    next_index: u64,
) -> Vec<IndexedDirectInput> {
    payloads_with_blocks
        .into_iter()
        .enumerate()
        .map(|(i, (payload, block_number))| IndexedDirectInput {
            index: next_index + i as u64,
            payload,
            block_number,
        })
        .collect()
}

/// Returns the current chain head using the standard "safe" block tag (alloy `BlockNumberOrTag::Safe`).
async fn latest_safe_block(provider: &impl Provider) -> Result<u64, InputReaderError> {
    let block = provider
        .get_block(Safe.into())
        .await
        .map_err(|e| InputReaderError::Provider(e.to_string()))?
        .ok_or_else(|| InputReaderError::Provider("get_block returned None".to_string()))?;
    let number = block.header.number;
    Ok(number)
}

pub struct InputReader {
    config: InputReaderConfig,
    storage: Storage,
    stop: Arc<AtomicBool>,
}

impl InputReader {
    pub fn new(config: InputReaderConfig, storage: Storage) -> Self {
        Self {
            config,
            storage,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn request_shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Returns true if shutdown has been requested. For tests.
    #[cfg(test)]
    pub(crate) fn is_shutdown_requested(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Run the input reader loop on a dedicated thread. Uses a current-thread async runtime
    /// for provider calls; storage is used synchronously on the same thread.
    pub fn run_blocking(self) -> Result<(), InputReaderError> {
        let stop = self.stop.clone();
        thread::spawn(move || {
            let rt = Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("input reader runtime");
            let mut reader = self;
            while !stop.load(Ordering::Relaxed) {
                if let Err(e) = rt.block_on(reader.advance_once()) {
                    match &e {
                        InputReaderError::ShutdownRequested => break,
                        _ => {
                            tracing::warn!(error = %e, "input reader advance failed, will retry");
                        }
                    }
                }
                thread::sleep(reader.config.poll_interval);
            }
        });
        Ok(())
    }

    /// One iteration of the input reader loop. Public for tests.
    pub(crate) async fn advance_once(&mut self) -> Result<(), InputReaderError> {
        let provider = alloy::providers::ProviderBuilder::new()
            .connect(self.config.rpc_url.as_str())
            .await
            .map_err(|e| InputReaderError::Provider(e.to_string()))?;

        let current = latest_safe_block(&provider).await?;
        let mut prev = self.storage.input_reader_last_processed_block()?;
        if prev == 0 && self.config.genesis_block > 0 {
            prev = self.config.genesis_block.saturating_sub(1);
        }

        if current <= prev {
            return Ok(());
        }

        let start_block = prev + 1;
        let topic1 = self.config.app_address_filter.into_word().into();

        let events = get_input_added_events(
            &provider,
            Some(&topic1),
            &self.config.input_box_address,
            start_block,
            current,
            &self.config.long_block_range_error_codes,
        )
        .await
        .map_err(|errs| {
            InputReaderError::Provider(format!(
                "get_input_added_events: {}",
                errs.into_iter()
                    .next()
                    .map(|e| e.to_string())
                    .unwrap_or_default()
            ))
        })?;

        if events.is_empty() {
            self.storage
                .input_reader_set_last_processed_block(current)?;
            return Ok(());
        }

        let next_index = self.storage.safe_input_end_exclusive()?;
        let payloads_with_blocks: Vec<(Vec<u8>, u64)> = events
            .into_iter()
            .map(|(ev, log)| {
                let block_number = log
                    .block_number
                    .and_then(|n| n.try_into().ok())
                    .unwrap_or(0u64);
                (ev.input.to_vec(), block_number)
            })
            .collect();
        let batch = build_indexed_direct_input_batch(payloads_with_blocks, next_index);

        for item in &batch {
            trace!(
                index = item.index,
                block_number = item.block_number,
                payload_len = item.payload.len(),
                "safe input"
            );
        }
        info!(
            block_range = %format!("{}..={}", start_block, current),
            count = batch.len(),
            "appending safe inputs"
        );

        self.storage.append_safe_direct_inputs(&batch)?;
        self.storage
            .input_reader_set_last_processed_block(current)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::node_bindings::Anvil;
    use alloy_primitives::Address;
    use std::time::Duration;
    use tempfile::NamedTempFile;

    // ----- Unit tests: retry predicate -----------------------------------------

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
    fn error_message_matches_retry_codes_returns_false_when_codes_empty() {
        assert!(!error_message_matches_retry_codes(
            "any error message",
            &[] as &[String]
        ));
    }

    // ----- Unit tests: batch builder -------------------------------------------

    #[test]
    fn build_indexed_direct_input_batch_empty() {
        let batch = build_indexed_direct_input_batch(Vec::<(Vec<u8>, u64)>::new(), 0);
        assert!(batch.is_empty());
    }

    #[test]
    fn build_indexed_direct_input_batch_contiguous_indices_and_block_numbers() {
        let payloads = vec![
            (vec![0x01], 100_u64),
            (vec![0x02, 0x03], 101),
            (vec![], 102),
        ];
        let batch = build_indexed_direct_input_batch(payloads, 5);
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].index, 5);
        assert_eq!(batch[0].payload, vec![0x01]);
        assert_eq!(batch[0].block_number, 100);
        assert_eq!(batch[1].index, 6);
        assert_eq!(batch[1].payload, vec![0x02, 0x03]);
        assert_eq!(batch[1].block_number, 101);
        assert_eq!(batch[2].index, 7);
        assert!(batch[2].payload.is_empty());
        assert_eq!(batch[2].block_number, 102);
    }

    // ----- Unit tests: InputReader construction and shutdown -------------------

    #[test]
    fn input_reader_new_and_request_shutdown_sets_stop_flag() {
        let db_file = NamedTempFile::new().expect("temp file");
        let storage =
            crate::storage::Storage::open(db_file.path().to_string_lossy().as_ref(), "NORMAL")
                .expect("open storage");
        let config = InputReaderConfig {
            rpc_url: "http://127.0.0.1:0".to_string(),
            input_box_address: Address::ZERO,
            app_address_filter: Address::ZERO,
            genesis_block: 0,
            poll_interval: Duration::from_secs(1),
            long_block_range_error_codes: vec![],
        };
        let reader = InputReader::new(config, storage);
        assert!(!reader.is_shutdown_requested());
        reader.request_shutdown();
        assert!(reader.is_shutdown_requested());
    }

    // ----- Integration tests (Anvil) -----------------------------------------

    /// Spawn Anvil, run one advance_once with no InputAdded contract (empty events).
    /// Asserts the reader connects, reads safe block, and updates last_processed_block when block > 0.
    #[tokio::test]
    async fn advance_once_with_anvil_updates_cursor_when_block_available() {
        let anvil = Anvil::default().block_time(1).spawn();
        let rpc_url = anvil.endpoint_url().to_string();
        let db_file = NamedTempFile::new().expect("temp file");
        let db_path = db_file.path().to_string_lossy();
        let storage = crate::storage::Storage::open(&db_path, "NORMAL").expect("open storage");

        let config = InputReaderConfig {
            rpc_url: rpc_url.to_string(),
            input_box_address: Address::ZERO,
            app_address_filter: Address::ZERO,
            genesis_block: 0,
            poll_interval: Duration::from_secs(1),
            long_block_range_error_codes: vec![],
        };
        let mut reader = InputReader::new(config, storage);

        reader.advance_once().await.expect("advance_once");

        let _last = reader
            .storage
            .input_reader_last_processed_block()
            .expect("read cursor");
        assert_eq!(
            reader.storage.safe_input_end_exclusive().expect("safe end"),
            0,
            "no InputAdded contract so no direct inputs"
        );
    }

    /// When storage cursor is already ahead of chain head, advance_once does nothing
    /// and does not overwrite last_processed_block.
    #[tokio::test]
    async fn advance_once_when_cursor_ahead_of_chain_is_no_op() {
        let anvil = Anvil::default().block_time(1).spawn();
        let rpc_url = anvil.endpoint_url().to_string();
        let db_file = NamedTempFile::new().expect("temp file");
        let db_path = db_file.path().to_string_lossy();
        let mut storage = crate::storage::Storage::open(&db_path, "NORMAL").expect("open storage");
        storage
            .input_reader_set_last_processed_block(1000)
            .expect("set cursor ahead of chain");

        let config = InputReaderConfig {
            rpc_url,
            input_box_address: Address::ZERO,
            app_address_filter: Address::ZERO,
            genesis_block: 0,
            poll_interval: Duration::from_secs(1),
            long_block_range_error_codes: vec![],
        };
        let mut reader = InputReader::new(config, storage);

        reader.advance_once().await.expect("advance_once");

        assert_eq!(
            reader
                .storage
                .input_reader_last_processed_block()
                .expect("read"),
            1000,
            "cursor must not be overwritten when chain is behind"
        );
    }
}
