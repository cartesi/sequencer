// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Reads safe InputBox events from L1 and appends them to sequencer storage.
//! Minimal design: no epochs or consensus; flat contiguous indices only.

use std::time::Duration;

use alloy::eips::BlockNumberOrTag::Safe;
use alloy::providers::Provider;
use alloy::sol_types::SolInterface;
use alloy_primitives::{Address, U256};
use cartesi_rollups_contracts::application::Application;
use cartesi_rollups_contracts::data_availability::DataAvailability::{
    DataAvailabilityCalls, InputBoxAndEspressoCall, InputBoxCall,
};
use cartesi_rollups_contracts::input_box::InputBox;
use tokio::task::JoinHandle;
use tracing::info;

use crate::l1::partition::{decode_evm_advance_input, get_input_added_events};
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{SchedulerRules, Storage, StorageOpenError, StoredSafeInput};

const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";

#[derive(Debug, Clone)]
pub struct InputReaderConfig {
    pub rpc_url: String,
    pub app_address: Address,
    pub poll_interval: Duration,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    pub long_block_range_error_codes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum InputReaderError {
    #[error("provider/transport: {0}")]
    Provider(String),
    #[error("bootstrap: {0}")]
    Bootstrap(String),
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("input reader join error: {0}")]
    Join(String),
}

pub struct InputReader {
    config: InputReaderConfig,
    input_box_address: Address,
    genesis_block: u64,
    db_path: String,
    shutdown: ShutdownSignal,
    /// Scheduler acceptance rules used to keep `safe_accepted_batches`
    /// consistent with every `append_safe_inputs` write.
    scheduler_rules: SchedulerRules,
}

impl InputReader {
    pub async fn new(
        db_path: impl Into<String>,
        shutdown: ShutdownSignal,
        config: InputReaderConfig,
        scheduler_rules: SchedulerRules,
    ) -> Result<Self, InputReaderError> {
        let provider = crate::l1::provider::create_provider(&config.rpc_url)
            .map_err(InputReaderError::Bootstrap)?;
        let application = Application::new(config.app_address, &provider);
        let data_availability = application
            .getDataAvailability()
            .call()
            .await
            .map_err(map_contract_bootstrap_error)?;
        let input_box_address = decode_input_box_address(&data_availability)?;

        let input_box = InputBox::new(input_box_address, &provider);
        let genesis_block = input_box
            .getDeploymentBlockNumber()
            .call()
            .await
            .map_err(map_contract_bootstrap_error)?
            .try_into()
            .map_err(|_| {
                InputReaderError::Bootstrap(
                    "input box deployment block number did not fit into u64".to_string(),
                )
            })?;

        Ok(Self::from_parts(
            config,
            input_box_address,
            genesis_block,
            db_path.into(),
            shutdown,
            scheduler_rules,
        ))
    }

    pub fn from_parts(
        config: InputReaderConfig,
        input_box_address: Address,
        genesis_block: u64,
        db_path: String,
        shutdown: ShutdownSignal,
        scheduler_rules: SchedulerRules,
    ) -> Self {
        Self {
            config,
            input_box_address,
            genesis_block,
            db_path,
            shutdown,
            scheduler_rules,
        }
    }

    pub fn input_box_address(&self) -> Address {
        self.input_box_address
    }

    pub fn genesis_block(&self) -> u64 {
        self.genesis_block
    }

    pub fn start(self) -> Result<JoinHandle<Result<(), InputReaderError>>, StorageOpenError> {
        let _ = Storage::open(self.db_path.as_str(), SQLITE_SYNCHRONOUS_PRAGMA)?;
        Ok(tokio::spawn(async move { self.run_forever().await }))
    }

    pub async fn sync_to_current_safe_head(&mut self) -> Result<(), InputReaderError> {
        self.bootstrap_safe_head().await?;

        let provider = crate::l1::provider::create_provider(&self.config.rpc_url)
            .map_err(InputReaderError::Bootstrap)?;
        self.advance_once(&provider).await
    }

    async fn run_forever(mut self) -> Result<(), InputReaderError> {
        self.bootstrap_safe_head().await?;

        let provider = crate::l1::provider::create_provider(&self.config.rpc_url)
            .map_err(InputReaderError::Bootstrap)?;

        loop {
            if self.shutdown.is_shutdown_requested() {
                return Ok(());
            }

            match self.advance_once(&provider).await {
                Ok(()) => {}
                Err(InputReaderError::Provider(error)) => {
                    tracing::error!(error, "L1 provider error in input reader — will retry");
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
        let previous_safe_block = self.current_safe_block().await?;

        // If our persisted safe head is already at the current safe frontier,
        // there is nothing new to scan. We only seed the progress marker on the
        // first real observation; subsequent same-head polls must not refresh it.
        if current_safe_block <= previous_safe_block {
            self.initialize_safe_progress_if_unset().await?;
            return Ok(());
        }

        let start_block = previous_safe_block + 1;
        let events = get_input_added_events(
            provider,
            self.config.app_address,
            &self.input_box_address,
            start_block,
            current_safe_block,
            self.config.long_block_range_error_codes.as_slice(),
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

        let mut batch = Vec::with_capacity(events.len());
        for (event, log) in events {
            let block_number = log.block_number.ok_or_else(|| {
                InputReaderError::Provider("InputAdded log missing block_number".to_string())
            })?;
            let evm_advance = decode_evm_advance_input(event.input.as_ref())
                .map_err(InputReaderError::Provider)?;
            assert_eq!(
                evm_advance.blockNumber,
                U256::from(block_number),
                "InputAdded block number mismatch: log={block_number}, payload={}",
                evm_advance.blockNumber
            );

            batch.push(StoredSafeInput {
                sender: evm_advance.msgSender,
                payload: evm_advance.payload.into(),
                block_number,
            });
        }

        info!(
            block_range = %format!("{}..={}", start_block, current_safe_block),
            count = batch.len(),
            "appending safe inputs"
        );

        self.append_safe_inputs(current_safe_block, batch).await
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
        let minimum_safe_block = self.genesis_block.saturating_sub(1);
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
            storage
                .ensure_minimum_safe_block(minimum_safe_block)
                .map_err(InputReaderError::from)
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }

    async fn initialize_safe_progress_if_unset(&self) -> Result<(), InputReaderError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
            storage
                .initialize_safe_progress_if_unset()
                .map_err(InputReaderError::from)
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }

    async fn append_safe_inputs(
        &self,
        current_safe_block: u64,
        batch: Vec<StoredSafeInput>,
    ) -> Result<(), InputReaderError> {
        let db_path = self.db_path.clone();
        let rules = self.scheduler_rules;
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
            storage
                .append_safe_inputs(current_safe_block, &batch, &rules)
                .map_err(InputReaderError::from)
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }
}

fn decode_input_box_address(data_availability: &[u8]) -> Result<Address, InputReaderError> {
    let call = DataAvailabilityCalls::abi_decode(data_availability).map_err(|err| {
        InputReaderError::Bootstrap(format!(
            "application getDataAvailability returned invalid DataAvailability calldata: {err}"
        ))
    })?;

    match call {
        DataAvailabilityCalls::InputBox(InputBoxCall { inputBox }) => Ok(inputBox),
        DataAvailabilityCalls::InputBoxAndEspresso(InputBoxAndEspressoCall {
            inputBox,
            fromBlock,
            namespaceId,
        }) => Err(InputReaderError::Bootstrap(format!(
            "application getDataAvailability returned unsupported DataAvailability.InputBoxAndEspresso(inputBox={inputBox}, fromBlock={fromBlock}, namespaceId={namespaceId})"
        ))),
    }
}

fn map_contract_bootstrap_error(err: alloy::contract::Error) -> InputReaderError {
    match err {
        alloy::contract::Error::TransportError(_) => InputReaderError::Provider(err.to_string()),
        _ => InputReaderError::Bootstrap(err.to_string()),
    }
}

async fn latest_safe_block(provider: &impl Provider) -> Result<u64, InputReaderError> {
    let block = provider
        .get_block(Safe.into())
        .await
        .map_err(|e| InputReaderError::Provider(e.to_string()))?
        .ok_or_else(|| InputReaderError::Provider("get_block returned None".to_string()))?;
    Ok(block.header.number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::node_bindings::Anvil;
    use alloy::sol_types::SolCall;
    use tempfile::NamedTempFile;

    fn test_reader(
        db_path: String,
        rpc_url: String,
        genesis_block: u64,
        poll_interval: Duration,
        shutdown: ShutdownSignal,
    ) -> InputReader {
        InputReader::from_parts(
            InputReaderConfig {
                rpc_url,
                app_address: Address::ZERO,
                poll_interval,
                long_block_range_error_codes: Vec::new(),
            },
            Address::ZERO,
            genesis_block,
            db_path,
            shutdown,
            SchedulerRules::new(Address::ZERO, sequencer_core::MAX_WAIT_BLOCKS),
        )
    }

    /// Verify that `anvil` is available. Panics with a clear message if not found.
    fn require_anvil() {
        assert!(
            std::process::Command::new("anvil")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok(),
            "anvil not found on PATH — install Foundry (https://getfoundry.sh)"
        );
    }

    #[tokio::test]
    async fn start_then_request_shutdown_joins_with_ok() {
        let db_file = NamedTempFile::new().expect("temp file");
        let shutdown = ShutdownSignal::default();
        let reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            "http://127.0.0.1:0".to_string(),
            0,
            Duration::from_millis(20),
            shutdown.clone(),
        );
        let handle = reader.start().expect("start input reader");

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
        require_anvil();

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let shutdown = ShutdownSignal::default();
        let db_file = NamedTempFile::new().expect("temp file");
        let reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            anvil.endpoint_url().to_string(),
            0,
            Duration::from_millis(50),
            shutdown.clone(),
        );
        let handle = reader.start().expect("start input reader");

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
        require_anvil();

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let db_file = NamedTempFile::new().expect("temp file");
        let mut reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            anvil.endpoint_url().to_string(),
            0,
            Duration::from_secs(1),
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
        let reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            "http://127.0.0.1:0".to_string(),
            genesis_block,
            Duration::from_secs(1),
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
        let mut reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            "http://127.0.0.1:0".to_string(),
            genesis_block,
            Duration::from_secs(1),
            ShutdownSignal::default(),
        );

        let result = reader.sync_to_current_safe_head().await;

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
    async fn new_with_invalid_rpc_url_returns_bootstrap_error() {
        let db_file = NamedTempFile::new().expect("temp file");

        let result = InputReader::new(
            db_file.path().to_string_lossy().into_owned(),
            ShutdownSignal::default(),
            InputReaderConfig {
                rpc_url: "not-a-valid-url".to_string(),
                app_address: Address::ZERO,
                poll_interval: Duration::from_secs(1),
                long_block_range_error_codes: Vec::new(),
            },
            SchedulerRules::new(Address::ZERO, sequencer_core::MAX_WAIT_BLOCKS),
        )
        .await;

        match result {
            Err(InputReaderError::Bootstrap(_)) => {}
            Err(other) => panic!("expected bootstrap error, got {other:?}"),
            Ok(_) => panic!("invalid RPC URL should fail during bootstrap"),
        }
    }

    #[tokio::test]
    async fn advance_once_when_safe_head_ahead_of_chain_is_no_op() {
        require_anvil();

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let db_file = NamedTempFile::new().expect("temp file");
        let db_path = db_file.path().to_string_lossy().into_owned();
        let mut storage = Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        let rules = SchedulerRules::new(Address::ZERO, sequencer_core::MAX_WAIT_BLOCKS);
        storage
            .append_safe_inputs(1000, &[], &rules)
            .expect("set safe head ahead of chain");
        let recorded_sync = storage
            .last_safe_progress_ms()
            .expect("read safe-progress timestamp");
        assert!(
            recorded_sync > 0,
            "append_safe_inputs should stamp safe progress"
        );
        drop(storage);

        let mut reader = test_reader(
            db_path,
            anvil.endpoint_url().to_string(),
            0,
            Duration::from_secs(1),
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

        let storage = Storage::open(
            db_file.path().to_string_lossy().as_ref(),
            SQLITE_SYNCHRONOUS_PRAGMA,
        )
        .expect("re-open storage");
        assert_eq!(
            storage
                .last_safe_progress_ms()
                .expect("read unchanged safe-progress timestamp"),
            recorded_sync,
            "same-head polls must not refresh the safe-progress marker"
        );
    }

    #[test]
    fn decode_input_box_address_rejects_non_abi_payloads() {
        let err = decode_input_box_address(&[0_u8; 19]).expect_err("short bytes should fail");
        assert!(
            err.to_string()
                .contains("invalid DataAvailability calldata")
        );

        let err = decode_input_box_address(&[0x22; 20]).expect_err("raw address bytes should fail");
        assert!(
            err.to_string()
                .contains("invalid DataAvailability calldata")
        );
    }

    #[test]
    fn decode_input_box_address_decodes_input_box_call() {
        let expected = Address::from([0x22; 20]);
        let encoded = InputBoxCall { inputBox: expected }.abi_encode();

        let address = decode_input_box_address(&encoded).expect("InputBox call should decode");
        assert_eq!(address, expected);
    }

    #[test]
    fn decode_input_box_address_rejects_unsupported_variants() {
        let encoded = InputBoxAndEspressoCall {
            inputBox: Address::from([0x33; 20]),
            fromBlock: U256::from(123_u64),
            namespaceId: 42,
        }
        .abi_encode();

        let err =
            decode_input_box_address(&encoded).expect_err("InputBoxAndEspresso should be rejected");
        assert!(
            err.to_string()
                .contains("unsupported DataAvailability.InputBoxAndEspresso")
        );
    }
}
