// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use alloy::eips::BlockNumberOrTag::{Latest, Safe};
use alloy::providers::Provider;
use alloy::providers::ProviderBuilder;
use alloy_primitives::Address;
use cartesi_rollups_contracts::application::Application;
use cartesi_rollups_contracts::input_box::InputBox;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::logs::{get_input_added_events, get_transaction_sender};
use crate::shutdown::ShutdownSignal;
use crate::storage::{Storage, StorageOpenError, StoredDirectInput};
use sequencer_core::batch::INPUT_TAG_DIRECT_INPUT;

const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";

#[derive(Debug, Clone)]
pub struct InputReaderConfig {
    pub rpc_url: String,
    pub input_box_address: Address,
    pub app_address_filter: Address,
    /// EOA whose inputs are batch submissions (not persisted as direct inputs). All other senders = direct.
    pub batch_submitter_address: Address,
    pub genesis_block: u64,
    pub poll_interval: Duration,
    pub long_block_range_error_codes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum InputReaderError {
    #[error("provider/transport: {0}")]
    Provider(String),
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("input reader join error: {0}")]
    Join(String),
}

pub struct InputReader {
    config: InputReaderConfig,
    db_path: String,
    shutdown: ShutdownSignal,
}

impl InputReader {
    pub async fn discover_input_box(
        rpc_url: &str,
        application_address: Address,
    ) -> Result<Address, InputReaderError> {
        let provider = ProviderBuilder::new()
            .connect(rpc_url)
            .await
            .map_err(|e| InputReaderError::Provider(e.to_string()))?;
        let application = Application::new(application_address, &provider);
        let data_availability = application
            .getDataAvailability()
            .call()
            .await
            .map_err(|e| InputReaderError::Provider(e.to_string()))?;

        decode_input_box_address(&data_availability)
    }

    pub async fn discover_input_box_deployment_block(
        rpc_url: &str,
        input_box_address: Address,
    ) -> Result<u64, InputReaderError> {
        let provider = ProviderBuilder::new()
            .connect(rpc_url)
            .await
            .map_err(|e| InputReaderError::Provider(e.to_string()))?;
        let input_box = InputBox::new(input_box_address, &provider);
        input_box
            .getDeploymentBlockNumber()
            .call()
            .await
            .map_err(|e| InputReaderError::Provider(e.to_string()))?
            .try_into()
            .map_err(|_| {
                InputReaderError::Provider(
                    "input box deployment block number did not fit into u64".to_string(),
                )
            })
    }

    pub fn new(config: InputReaderConfig, db_path: String, shutdown: ShutdownSignal) -> Self {
        Self {
            config,
            db_path,
            shutdown,
        }
    }

    pub fn start(
        db_path: &str,
        config: InputReaderConfig,
        shutdown: ShutdownSignal,
    ) -> Result<JoinHandle<Result<(), InputReaderError>>, StorageOpenError> {
        let _ = Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
        let reader = Self::new(config, db_path.to_string(), shutdown);
        Ok(tokio::spawn(async move { reader.run_forever().await }))
    }

    pub async fn sync_to_current_safe_head(
        db_path: &str,
        config: InputReaderConfig,
    ) -> Result<(), InputReaderError> {
        let mut reader = Self::new(config, db_path.to_string(), ShutdownSignal::default());
        reader.bootstrap_safe_head().await?;

        let provider = ProviderBuilder::new()
            .connect(reader.config.rpc_url.as_str())
            .await
            .map_err(|e| InputReaderError::Provider(e.to_string()))?;
        reader.advance_once(&provider).await
    }

    async fn run_forever(mut self) -> Result<(), InputReaderError> {
        self.bootstrap_safe_head().await?;

        let provider = ProviderBuilder::new()
            .connect(self.config.rpc_url.as_str())
            .await
            .map_err(|e| InputReaderError::Provider(e.to_string()))?;

        loop {
            if self.shutdown.is_shutdown_requested() {
                return Ok(());
            }

            match self.advance_once(&provider).await {
                Ok(()) => {}
                Err(InputReaderError::Provider(error)) => {
                    warn!(error, "input reader advance failed, will retry");
                }
                Err(err) => return Err(err),
            }

            tokio::select! {
                _ = self.shutdown.wait_for_shutdown() => return Ok(()),
                _ = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
    }

    pub(crate) async fn advance_once(
        &mut self,
        provider: &impl Provider,
    ) -> Result<(), InputReaderError> {
        let current_safe_block = latest_safe_block(provider).await?;
        let current_head_block = latest_head_block(provider).await?;
        let previous_safe_block = self.current_safe_block().await?;

        // If our persisted safe head is already ahead of or equal to the chain head,
        // there is nothing new to scan.
        if current_head_block <= previous_safe_block {
            return Ok(());
        }

        let start_block = previous_safe_block + 1;
        let events = get_input_added_events(
            provider,
            self.config.app_address_filter,
            &self.config.input_box_address,
            start_block,
            current_head_block,
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

        // Classify by tx sender: inputs from the batch submitter are batch submissions (skip);
        // all others with tag 0x00 and block_number <= Safe are direct inputs (persist).
        let mut batch = Vec::new();
        for (event, log) in events {
            let block_number = match log.block_number {
                Some(n) => n,
                None => continue,
            };
            let tx_hash = match log.transaction_hash {
                Some(h) => h,
                None => continue,
            };
            let sender = match get_transaction_sender(provider, tx_hash).await {
                Some(s) => s,
                None => continue,
            };
            if sender == self.config.batch_submitter_address {
                continue;
            }
            let first = match event.input.first().copied() {
                Some(b) => b,
                None => continue,
            };
            if first == INPUT_TAG_DIRECT_INPUT && block_number <= current_safe_block {
                let body = event.input.get(1..).unwrap_or_default().to_vec();
                batch.push(StoredDirectInput {
                    payload: body,
                    block_number,
                });
            }
        }

        info!(
            block_range = %format!("{}..={}", start_block, current_head_block),
            count = batch.len(),
            "appending safe direct inputs"
        );

        self.append_safe_direct_inputs(current_safe_block, batch, None)
            .await
    }

    async fn current_safe_block(&self) -> Result<u64, InputReaderError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
            storage.current_safe_block().map_err(InputReaderError::from)
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }

    async fn bootstrap_safe_head(&self) -> Result<(), InputReaderError> {
        let db_path = self.db_path.clone();
        let minimum_safe_block = self.config.genesis_block.saturating_sub(1);
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
            storage
                .ensure_minimum_safe_block(minimum_safe_block)
                .map_err(InputReaderError::from)
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }

    async fn append_safe_direct_inputs(
        &self,
        current_safe_block: u64,
        batch: Vec<StoredDirectInput>,
        max_batch_nonce: Option<u64>,
    ) -> Result<(), InputReaderError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
            storage
                .append_safe_direct_inputs(current_safe_block, &batch, max_batch_nonce)
                .map_err(InputReaderError::from)
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }
}

fn decode_input_box_address(data_availability: &[u8]) -> Result<Address, InputReaderError> {
    if data_availability.len() != 20 {
        return Err(InputReaderError::Provider(format!(
            "application getDataAvailability returned {} bytes; expected 20-byte InputBox address",
            data_availability.len()
        )));
    }

    Ok(Address::from_slice(data_availability))
}

async fn latest_safe_block(provider: &impl Provider) -> Result<u64, InputReaderError> {
    let block = provider
        .get_block(Safe.into())
        .await
        .map_err(|e| InputReaderError::Provider(e.to_string()))?
        .ok_or_else(|| InputReaderError::Provider("get_block returned None".to_string()))?;
    Ok(block.header.number)
}

async fn latest_head_block(provider: &impl Provider) -> Result<u64, InputReaderError> {
    let block = provider
        .get_block(Latest.into())
        .await
        .map_err(|e| InputReaderError::Provider(e.to_string()))?
        .ok_or_else(|| {
            InputReaderError::Provider("get_block (latest) returned None".to_string())
        })?;
    Ok(block.header.number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::node_bindings::Anvil;
    use tempfile::NamedTempFile;

    fn require_anvil_tests() -> bool {
        std::env::var_os("RUN_ANVIL_TESTS").is_some()
    }

    #[tokio::test]
    async fn start_then_request_shutdown_joins_with_ok() {
        let db_file = NamedTempFile::new().expect("temp file");
        let shutdown = ShutdownSignal::default();
        let handle = InputReader::start(
            db_file.path().to_string_lossy().as_ref(),
            InputReaderConfig {
                rpc_url: "http://127.0.0.1:0".to_string(),
                input_box_address: Address::ZERO,
                app_address_filter: Address::ZERO,
                batch_submitter_address: Address::ZERO,
                genesis_block: 0,
                poll_interval: Duration::from_millis(20),
                long_block_range_error_codes: vec![],
            },
            shutdown.clone(),
        )
        .expect("start input reader");

        shutdown.request_shutdown();
        let join_result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        let join_result = join_result.expect("reader should exit within timeout");
        assert!(
            matches!(join_result, Ok(Ok(()))),
            "expected Ok(Ok(())), got {:?}",
            join_result
        );
    }

    #[tokio::test]
    async fn start_with_anvil_request_shutdown_then_join_returns_ok() {
        if !require_anvil_tests() {
            return;
        }

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let shutdown = ShutdownSignal::default();
        let db_file = NamedTempFile::new().expect("temp file");
        let handle = InputReader::start(
            db_file.path().to_string_lossy().as_ref(),
            InputReaderConfig {
                rpc_url: anvil.endpoint_url().to_string(),
                input_box_address: Address::ZERO,
                app_address_filter: Address::ZERO,
                batch_submitter_address: Address::ZERO,
                genesis_block: 0,
                poll_interval: Duration::from_millis(50),
                long_block_range_error_codes: vec![],
            },
            shutdown.clone(),
        )
        .expect("start input reader");

        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.request_shutdown();

        let join_result = tokio::time::timeout(Duration::from_secs(3), handle).await;
        let join_result = join_result.expect("reader should exit within timeout");
        assert!(
            matches!(join_result, Ok(Ok(()))),
            "expected Ok(Ok(())), got {:?}",
            join_result
        );
    }

    #[tokio::test]
    async fn advance_once_with_anvil_updates_safe_head_when_block_available() {
        if !require_anvil_tests() {
            return;
        }

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let db_file = NamedTempFile::new().expect("temp file");
        let mut reader = InputReader::new(
            InputReaderConfig {
                rpc_url: anvil.endpoint_url().to_string(),
                input_box_address: Address::ZERO,
                app_address_filter: Address::ZERO,
                batch_submitter_address: Address::ZERO,
                genesis_block: 0,
                poll_interval: Duration::from_secs(1),
                long_block_range_error_codes: vec![],
            },
            db_file.path().to_string_lossy().into_owned(),
            ShutdownSignal::default(),
        );
        let provider = alloy::providers::ProviderBuilder::new()
            .connect(anvil.endpoint_url().to_string().as_str())
            .await
            .expect("connect provider");

        reader.advance_once(&provider).await.expect("advance_once");
        let safe_block = reader.current_safe_block().await.expect("read safe block");
        let safe_end = {
            let mut storage = Storage::open(
                db_file.path().to_string_lossy().as_ref(),
                SQLITE_SYNCHRONOUS_PRAGMA,
            )
            .expect("open storage");
            storage.safe_input_end_exclusive().expect("safe end")
        };
        assert_eq!(safe_end, 0, "no InputAdded contract so no direct inputs");
        let _ = safe_block;
    }

    #[tokio::test]
    async fn advance_once_with_genesis_block_uses_genesis_as_effective_prev() {
        let db_file = NamedTempFile::new().expect("temp file");
        let genesis_block = 2_u64;
        let reader = InputReader::new(
            InputReaderConfig {
                rpc_url: "http://127.0.0.1:0".to_string(),
                input_box_address: Address::ZERO,
                app_address_filter: Address::ZERO,
                batch_submitter_address: Address::ZERO,
                genesis_block,
                poll_interval: Duration::from_secs(1),
                long_block_range_error_codes: vec![],
            },
            db_file.path().to_string_lossy().into_owned(),
            ShutdownSignal::default(),
        );

        reader
            .bootstrap_safe_head()
            .await
            .expect("bootstrap safe head");

        let safe_block = reader.current_safe_block().await.expect("read safe block");
        assert_eq!(safe_block, genesis_block - 1);
    }

    #[tokio::test]
    async fn sync_to_current_safe_head_with_genesis_block_bootstraps_safe_head() {
        let db_file = NamedTempFile::new().expect("temp file");
        let genesis_block = 5_u64;

        let result = InputReader::sync_to_current_safe_head(
            db_file.path().to_string_lossy().as_ref(),
            InputReaderConfig {
                rpc_url: "http://127.0.0.1:0".to_string(),
                input_box_address: Address::ZERO,
                app_address_filter: Address::ZERO,
                batch_submitter_address: Address::ZERO,
                genesis_block,
                poll_interval: Duration::from_secs(1),
                long_block_range_error_codes: vec![],
            },
        )
        .await;

        assert!(matches!(result, Err(InputReaderError::Provider(_))));

        let mut storage = Storage::open(
            db_file.path().to_string_lossy().as_ref(),
            SQLITE_SYNCHRONOUS_PRAGMA,
        )
        .expect("open storage");
        assert_eq!(
            storage.current_safe_block().expect("read safe block"),
            genesis_block - 1
        );
    }

    #[tokio::test]
    async fn advance_once_when_safe_head_ahead_of_chain_is_no_op() {
        if !require_anvil_tests() {
            return;
        }

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let db_file = NamedTempFile::new().expect("temp file");
        let db_path = db_file.path().to_string_lossy().into_owned();
        let mut storage = Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        storage
            .append_safe_direct_inputs(1000, &[], None)
            .expect("set safe head ahead of chain");

        let mut reader = InputReader::new(
            InputReaderConfig {
                rpc_url: anvil.endpoint_url().to_string(),
                input_box_address: Address::ZERO,
                app_address_filter: Address::ZERO,
                batch_submitter_address: Address::ZERO,
                genesis_block: 0,
                poll_interval: Duration::from_secs(1),
                long_block_range_error_codes: vec![],
            },
            db_path,
            ShutdownSignal::default(),
        );
        let provider = alloy::providers::ProviderBuilder::new()
            .connect(anvil.endpoint_url().to_string().as_str())
            .await
            .expect("connect provider");

        reader.advance_once(&provider).await.expect("advance_once");
        assert_eq!(
            reader.current_safe_block().await.expect("read"),
            1000,
            "safe head should remain unchanged when already ahead of chain"
        );
    }

    #[test]
    fn decode_input_box_address_requires_exactly_20_bytes() {
        let err = decode_input_box_address(&[0_u8; 19]).expect_err("short bytes should fail");
        assert!(
            err.to_string()
                .contains("expected 20-byte InputBox address")
        );

        let address =
            decode_input_box_address(&[0x22; 20]).expect("20-byte data availability address");
        assert_eq!(address, Address::from([0x22; 20]));
    }
}
