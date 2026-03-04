// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use thiserror::Error;
use tracing::warn;

use crate::api::{self, ApiConfig};
use crate::config::RunConfig;
use crate::inclusion_lane::{InclusionLane, InclusionLaneConfig, InclusionLaneError};
use crate::input_reader::{InputReader, InputReaderConfig, InputReaderError};
use crate::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use crate::shutdown::ShutdownSignal;
use crate::storage::{self, StorageOpenError};
use sequencer_core::application::Application;

const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";
const QUEUE_CAPACITY: usize = 8192;
const INPUT_READER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum RunError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("server stopped unexpectedly")]
    ServerStoppedUnexpectedly,
    #[error("server join error: {source}")]
    ServerJoin {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("inclusion lane stopped unexpectedly")]
    InclusionLaneStoppedUnexpectedly,
    #[error("inclusion lane exited: {source}")]
    InclusionLane {
        #[source]
        source: InclusionLaneError,
    },
    #[error("inclusion lane join error: {source}")]
    InclusionLaneJoin {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("input reader stopped unexpectedly")]
    InputReaderStoppedUnexpectedly,
    #[error("input reader exited: {source}")]
    InputReader {
        #[source]
        source: InputReaderError,
    },
    #[error("input reader join error: {source}")]
    InputReaderJoin {
        #[source]
        source: tokio::task::JoinError,
    },
}

enum FirstExit {
    Signal(Option<RunError>),
    Server(RunError),
    InclusionLane(RunError),
    InputReader(RunError),
}

pub async fn run<A>(app: A, config: RunConfig) -> Result<(), RunError>
where
    A: Application + 'static,
{
    let domain = config.build_domain();
    let shutdown = ShutdownSignal::default();
    let input_box_address =
        InputReader::discover_input_box(&config.eth_rpc_url, config.domain_verifying_contract)
            .await
            .map_err(|source| RunError::InputReader { source })?;
    let input_reader_genesis_block =
        InputReader::discover_input_box_deployment_block(&config.eth_rpc_url, input_box_address)
            .await
            .map_err(|source| RunError::InputReader { source })?;
    let input_reader_config =
        build_input_reader_config(&config, input_box_address, input_reader_genesis_block);
    InputReader::sync_to_current_safe_head(&config.db_path, input_reader_config.clone())
        .await
        .map_err(|source| RunError::InputReader { source })?;

    tracing::info!(
        http_addr = %config.http_addr,
        db_path = %config.db_path,
        eth_rpc_url = %config.eth_rpc_url,
        input_box_address = %input_box_address,
        input_reader_genesis_block,
        domain_chain_id = config.domain_chain_id,
        domain_verifying_contract = %config.domain_verifying_contract,
        "starting sequencer"
    );

    let storage = storage::Storage::open(&config.db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
    let (tx, mut inclusion_lane_handle) = InclusionLane::start(
        QUEUE_CAPACITY,
        shutdown.clone(),
        app,
        storage,
        InclusionLaneConfig::for_app::<A>(),
    );
    let mut input_reader_handle =
        InputReader::start(&config.db_path, input_reader_config, shutdown.clone())?;

    let tx_feed = L2TxFeed::new(
        config.db_path.clone(),
        shutdown.clone(),
        L2TxFeedConfig::default(),
    );

    let mut server_task = api::start(
        &config.http_addr,
        tx,
        domain,
        A::MAX_METHOD_PAYLOAD_BYTES,
        shutdown.clone(),
        tx_feed,
        ApiConfig::default(),
    )
    .await?;

    tracing::info!(address = %config.http_addr, "listening");

    let shutdown_signal = tokio::signal::ctrl_c();
    tokio::pin!(shutdown_signal);

    let first_exit = tokio::select! {
        signal_result = &mut shutdown_signal => {
            FirstExit::Signal(signal_result.err().map(RunError::from))
        }
        server_result = &mut server_task => {
            FirstExit::Server(map_server_exit(server_result))
        }
        lane_result = &mut inclusion_lane_handle => {
            FirstExit::InclusionLane(map_lane_exit(lane_result))
        }
        reader_result = &mut input_reader_handle => {
            FirstExit::InputReader(map_input_reader_exit(reader_result))
        }
    };

    begin_runtime_shutdown(&shutdown);
    finish_runtime(
        first_exit,
        server_task,
        inclusion_lane_handle,
        input_reader_handle,
    )
    .await
}

fn begin_runtime_shutdown(shutdown: &ShutdownSignal) {
    shutdown.request_shutdown();
}

async fn wait_for_clean_shutdown(
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
    inclusion_lane_handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
    input_reader_handle: tokio::task::JoinHandle<Result<(), InputReaderError>>,
) -> Result<(), RunError> {
    wait_for_server_shutdown(server_task).await?;
    wait_for_lane_shutdown(inclusion_lane_handle).await?;
    wait_for_input_reader_shutdown(input_reader_handle).await?;
    Ok(())
}

async fn finish_runtime(
    first_exit: FirstExit,
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
    inclusion_lane_handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
    input_reader_handle: tokio::task::JoinHandle<Result<(), InputReaderError>>,
) -> Result<(), RunError> {
    match first_exit {
        FirstExit::Signal(signal_error) => {
            let shutdown_result =
                wait_for_clean_shutdown(server_task, inclusion_lane_handle, input_reader_handle)
                    .await;
            match (signal_error, shutdown_result) {
                (Some(err), _) => Err(err),
                (None, Ok(())) => Ok(()),
                (None, Err(err)) => Err(err),
            }
        }
        FirstExit::Server(primary) => {
            log_cleanup_result(
                "inclusion lane",
                wait_for_lane_shutdown(inclusion_lane_handle).await,
            );
            log_cleanup_result(
                "input reader",
                wait_for_input_reader_shutdown(input_reader_handle).await,
            );
            Err(primary)
        }
        FirstExit::InclusionLane(primary) => {
            log_cleanup_result("server", wait_for_server_shutdown(server_task).await);
            log_cleanup_result(
                "input reader",
                wait_for_input_reader_shutdown(input_reader_handle).await,
            );
            Err(primary)
        }
        FirstExit::InputReader(primary) => {
            log_cleanup_result("server", wait_for_server_shutdown(server_task).await);
            log_cleanup_result(
                "inclusion lane",
                wait_for_lane_shutdown(inclusion_lane_handle).await,
            );
            Err(primary)
        }
    }
}

async fn wait_for_server_shutdown(
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
) -> Result<(), RunError> {
    match server_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(RunError::Io(source)),
        Err(source) => Err(RunError::ServerJoin { source }),
    }
}

async fn wait_for_lane_shutdown(
    inclusion_lane_handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
) -> Result<(), RunError> {
    match inclusion_lane_handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(RunError::InclusionLane { source }),
        Err(source) => Err(RunError::InclusionLaneJoin { source }),
    }
}

async fn wait_for_input_reader_shutdown(
    input_reader_handle: tokio::task::JoinHandle<Result<(), InputReaderError>>,
) -> Result<(), RunError> {
    match input_reader_handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(RunError::InputReader { source }),
        Err(source) => Err(RunError::InputReaderJoin { source }),
    }
}

fn map_server_exit(result: Result<std::io::Result<()>, tokio::task::JoinError>) -> RunError {
    match result {
        Ok(Ok(())) => RunError::ServerStoppedUnexpectedly,
        Ok(Err(source)) => RunError::Io(source),
        Err(source) => RunError::ServerJoin { source },
    }
}

fn map_lane_exit(
    result: Result<Result<(), InclusionLaneError>, tokio::task::JoinError>,
) -> RunError {
    match result {
        Ok(Ok(())) => RunError::InclusionLaneStoppedUnexpectedly,
        Ok(Err(source)) => RunError::InclusionLane { source },
        Err(source) => RunError::InclusionLaneJoin { source },
    }
}

fn map_input_reader_exit(
    result: Result<Result<(), InputReaderError>, tokio::task::JoinError>,
) -> RunError {
    match result {
        Ok(Ok(())) => RunError::InputReaderStoppedUnexpectedly,
        Ok(Err(source)) => RunError::InputReader { source },
        Err(source) => RunError::InputReaderJoin { source },
    }
}

fn build_input_reader_config(
    config: &RunConfig,
    input_box_address: alloy_primitives::Address,
    genesis_block: u64,
) -> InputReaderConfig {
    InputReaderConfig {
        rpc_url: config.eth_rpc_url.clone(),
        input_box_address,
        app_address_filter: config.domain_verifying_contract,
        genesis_block,
        poll_interval: INPUT_READER_POLL_INTERVAL,
        long_block_range_error_codes: config.long_block_range_error_codes.clone(),
    }
}

fn log_cleanup_result(component: &str, result: Result<(), RunError>) {
    if let Err(err) = result {
        warn!(component, error = %err, "component shutdown after primary failure also errored");
    }
}
