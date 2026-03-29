// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use thiserror::Error;
use tracing::warn;

use crate::api::{self, ApiConfig};
use crate::batch_submitter::{BatchPosterConfig, EthereumBatchPoster};
use crate::batch_submitter::{BatchSubmitter, BatchSubmitterConfig, BatchSubmitterError};
use crate::config::{L1Config, RunConfig};
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
    #[error("batch submitter stopped unexpectedly")]
    BatchSubmitterStoppedUnexpectedly,
    #[error("batch submitter exited: {source}")]
    BatchSubmitter {
        #[source]
        source: BatchSubmitterError,
    },
    #[error("batch submitter join error: {source}")]
    BatchSubmitterJoin {
        #[source]
        source: tokio::task::JoinError,
    },
}

enum FirstExit {
    Signal(Option<RunError>),
    Server(RunError),
    InclusionLane(RunError),
    InputReader(RunError),
    BatchSubmitter(RunError),
}

pub async fn run<A>(app: A, config: RunConfig) -> Result<(), RunError>
where
    A: Application + 'static,
{
    let domain = config.build_domain();
    let shutdown = ShutdownSignal::default();

    // Ensure the data directory exists before any component tries to open the DB.
    std::fs::create_dir_all(&config.data_dir)?;
    let db_path = config.db_path();

    // Single L1/InputBox config shared by input reader and batch submitter (no duplicate RPC URL or addresses).
    let batch_submitter_private_key = config.resolve_private_key()?;

    let batch_submitter_address = {
        use alloy::signers::local::PrivateKeySigner;
        use std::str::FromStr;
        PrivateKeySigner::from_str(&batch_submitter_private_key)
            .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?
            .address()
    };
    // Bootstrap L1 config: try L1 first, fall back to DB cache if unreachable.
    // On first startup, L1 is required (no cache). On subsequent startups, the
    // cache allows the sequencer to start without L1 (e.g., during provider outages).
    let input_reader_config = InputReaderConfig {
        rpc_url: config.eth_rpc_url.clone(),
        app_address: config.app_address,
        poll_interval: INPUT_READER_POLL_INTERVAL,
        long_block_range_error_codes: config.long_block_range_error_codes.clone(),
    };

    let (mut input_reader, input_reader_genesis_block, l1_config) = match InputReader::new(
        db_path.clone(),
        shutdown.clone(),
        input_reader_config.clone(),
    )
    .await
    {
        Ok(reader) => {
            let genesis = reader.genesis_block();
            let input_box = reader.input_box_address();

            // Cache for future startups when L1 might be unreachable.
            if let Ok(mut s) = storage::Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA) {
                let _ = s.save_l1_bootstrap_cache(input_box, genesis, config.chain_id);
            }

            let l1 = L1Config {
                eth_rpc_url: config.eth_rpc_url.clone(),
                input_box_address: input_box,
                app_address: config.app_address,
                batch_submitter_private_key,
                batch_submitter_address,
            };
            (reader, genesis, l1)
        }
        Err(InputReaderError::Provider(e)) => {
            // L1 unreachable. Try the DB cache.
            tracing::error!(
                error = %e,
                "L1 unreachable during bootstrap — checking DB cache"
            );
            let cache_storage = storage::Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
            let cached = cache_storage
                .load_l1_bootstrap_cache()
                .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?;
            let Some((input_box, genesis, cached_chain_id)) = cached else {
                return Err(RunError::Io(std::io::Error::other(
                    "L1 unreachable and no bootstrap cache — \
                         L1 is required for first startup",
                )));
            };
            assert_eq!(
                cached_chain_id, config.chain_id,
                "cached chain ID {cached_chain_id} does not match --chain-id {}",
                config.chain_id
            );

            let reader = InputReader::from_parts(
                input_reader_config,
                input_box,
                genesis,
                db_path.clone(),
                shutdown.clone(),
            );
            let l1 = L1Config {
                eth_rpc_url: config.eth_rpc_url.clone(),
                input_box_address: input_box,
                app_address: config.app_address,
                batch_submitter_private_key,
                batch_submitter_address,
            };
            (reader, genesis, l1)
        }
        Err(source) => return Err(RunError::InputReader { source }),
    };
    // ── Startup config ──────────────────────────────────────────────
    assert!(
        config.preemptive_margin_blocks < sequencer_core::MAX_WAIT_BLOCKS,
        "preemptive_margin_blocks ({}) must be less than MAX_WAIT_BLOCKS ({})",
        config.preemptive_margin_blocks,
        sequencer_core::MAX_WAIT_BLOCKS,
    );
    let danger_threshold =
        sequencer_core::MAX_WAIT_BLOCKS.saturating_sub(config.preemptive_margin_blocks);

    tracing::info!(
        http_addr = %config.http_addr,
        data_dir = %config.data_dir,
        eth_rpc_url = %l1_config.eth_rpc_url,
        input_box_address = %l1_config.input_box_address,
        input_reader_genesis_block,
        chain_id = config.chain_id,
        app_address = %l1_config.app_address,
        batch_submitter_address = %l1_config.batch_submitter_address,
        max_wait_blocks = sequencer_core::MAX_WAIT_BLOCKS,
        preemptive_margin_blocks = config.preemptive_margin_blocks,
        danger_threshold,
        "sequencer startup"
    );

    // ── Preemptive recovery ────────────────────────────────────────
    // See docs/recovery/ for the full design and TLA+ spec.
    crate::recovery::run_preemptive_recovery(
        &db_path,
        &mut input_reader,
        l1_config.batch_submitter_address,
        &l1_config.eth_rpc_url,
        &l1_config.batch_submitter_private_key,
        sequencer_core::MAX_WAIT_BLOCKS,
        danger_threshold,
        config.seconds_per_block,
    )
    .await
    .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?;

    let storage = storage::Storage::open(&db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
    let (tx, mut inclusion_lane_handle) = InclusionLane::start(
        QUEUE_CAPACITY,
        shutdown.clone(),
        app,
        storage,
        InclusionLaneConfig::new(l1_config.batch_submitter_address),
    );
    let mut input_reader_handle = input_reader.start()?;

    // Batch submitter uses the same L1 config (InputBox address and RPC URL) as the input reader.
    let batch_submitter_config = BatchSubmitterConfig {
        idle_poll_interval_ms: config.batch_submitter_idle_poll_interval_ms,
        max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
        preemptive_margin_blocks: config.preemptive_margin_blocks,
        seconds_per_block: config.seconds_per_block,
    };
    let poster_config = BatchPosterConfig {
        l1_submit_address: l1_config.input_box_address,
        app_address: l1_config.app_address,
        batch_submitter_address: l1_config.batch_submitter_address,
        start_block: input_reader_genesis_block,
        confirmation_depth: config.batch_submitter_confirmation_depth,
        long_block_range_error_codes: config.long_block_range_error_codes,
    };
    let provider = build_batch_submitter_provider(&l1_config)?;

    // Validate that the RPC chain ID matches --chain-id (skip if L1 unreachable —
    // the cache already validated chain_id during bootstrap fallback above).
    {
        use alloy::providers::Provider;
        match provider.get_chain_id().await {
            Ok(rpc_chain_id) => {
                assert_eq!(
                    rpc_chain_id, config.chain_id,
                    "RPC chain ID {rpc_chain_id} does not match --chain-id {}",
                    config.chain_id
                );
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "could not validate RPC chain ID — L1 unreachable, trusting config"
                );
            }
        }
    }

    let poster = std::sync::Arc::new(EthereumBatchPoster::new(provider, poster_config));
    let submitter = BatchSubmitter::new(
        db_path.clone(),
        l1_config.batch_submitter_address,
        poster,
        shutdown.clone(),
        batch_submitter_config,
    );
    let mut batch_submitter_handle = submitter.start().map_err(RunError::OpenStorage)?;

    let tx_feed = L2TxFeed::new(
        db_path.clone(),
        shutdown.clone(),
        L2TxFeedConfig {
            batch_submitter_address: Some(l1_config.batch_submitter_address),
            ..L2TxFeedConfig::default()
        },
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
        submitter_result = &mut batch_submitter_handle => {
            FirstExit::BatchSubmitter(map_batch_submitter_exit(submitter_result))
        }
    };

    begin_runtime_shutdown(&shutdown);
    finish_runtime(
        first_exit,
        server_task,
        inclusion_lane_handle,
        input_reader_handle,
        batch_submitter_handle,
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
    batch_submitter_handle: tokio::task::JoinHandle<Result<(), BatchSubmitterError>>,
) -> Result<(), RunError> {
    wait_for_server_shutdown(server_task).await?;
    wait_for_lane_shutdown(inclusion_lane_handle).await?;
    wait_for_input_reader_shutdown(input_reader_handle).await?;
    wait_for_batch_submitter_shutdown(batch_submitter_handle).await?;
    Ok(())
}

async fn finish_runtime(
    first_exit: FirstExit,
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
    inclusion_lane_handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
    input_reader_handle: tokio::task::JoinHandle<Result<(), InputReaderError>>,
    batch_submitter_handle: tokio::task::JoinHandle<Result<(), BatchSubmitterError>>,
) -> Result<(), RunError> {
    match first_exit {
        FirstExit::Signal(signal_error) => {
            let shutdown_result = wait_for_clean_shutdown(
                server_task,
                inclusion_lane_handle,
                input_reader_handle,
                batch_submitter_handle,
            )
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
            log_cleanup_result(
                "batch submitter",
                wait_for_batch_submitter_shutdown(batch_submitter_handle).await,
            );
            Err(primary)
        }
        FirstExit::InclusionLane(primary) => {
            log_cleanup_result("server", wait_for_server_shutdown(server_task).await);
            log_cleanup_result(
                "input reader",
                wait_for_input_reader_shutdown(input_reader_handle).await,
            );
            log_cleanup_result(
                "batch submitter",
                wait_for_batch_submitter_shutdown(batch_submitter_handle).await,
            );
            Err(primary)
        }
        FirstExit::InputReader(primary) => {
            log_cleanup_result("server", wait_for_server_shutdown(server_task).await);
            log_cleanup_result(
                "inclusion lane",
                wait_for_lane_shutdown(inclusion_lane_handle).await,
            );
            log_cleanup_result(
                "batch submitter",
                wait_for_batch_submitter_shutdown(batch_submitter_handle).await,
            );
            Err(primary)
        }
        FirstExit::BatchSubmitter(primary) => {
            log_cleanup_result("server", wait_for_server_shutdown(server_task).await);
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

async fn wait_for_batch_submitter_shutdown(
    batch_submitter_handle: tokio::task::JoinHandle<Result<(), BatchSubmitterError>>,
) -> Result<(), RunError> {
    match batch_submitter_handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(RunError::BatchSubmitter { source }),
        Err(source) => Err(RunError::BatchSubmitterJoin { source }),
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

fn map_batch_submitter_exit(
    result: Result<Result<(), BatchSubmitterError>, tokio::task::JoinError>,
) -> RunError {
    match result {
        Ok(Ok(())) => RunError::BatchSubmitterStoppedUnexpectedly,
        Ok(Err(source)) => RunError::BatchSubmitter { source },
        Err(source) => RunError::BatchSubmitterJoin { source },
    }
}

fn log_cleanup_result(component: &str, result: Result<(), RunError>) {
    if let Err(err) = result {
        warn!(component, error = %err, "component shutdown after primary failure also errored");
    }
}

fn build_batch_submitter_provider(
    l1: &L1Config,
) -> Result<alloy::providers::DynProvider, std::io::Error> {
    crate::provider::create_signer_provider(&l1.eth_rpc_url, &l1.batch_submitter_private_key)
        .map_err(std::io::Error::other)
}
