// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Process orchestration: bootstraps L1 state, opens storage, runs preemptive
//! recovery, then spawns the lane / input reader / batch submitter /
//! danger detector / feed / HTTP servers and awaits their completion.

pub mod clock;
pub mod config;
pub mod shutdown;

use std::time::Duration;

use thiserror::Error;
use tracing::warn;

use crate::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use crate::http::{self, ApiConfig};
use crate::ingress::inclusion_lane::{InclusionLane, InclusionLaneConfig, InclusionLaneError};
use crate::l1::reader::{InputReader, InputReaderConfig, InputReaderError};
use crate::l1::submitter::{BatchPosterConfig, EthereumBatchPoster};
use crate::l1::submitter::{
    BatchSubmitter, BatchSubmitterConfig, BatchSubmitterError, SubmitterExit,
};
use crate::recovery::{DangerDetector, DangerDetectorError, DetectorExit};
use crate::storage::{self, StorageOpenError};
use alloy_primitives::Address;
use config::{L1Config, RunConfig};
use sequencer_core::application::Application;
use sequencer_core::protocol::ProtocolConfig;
use shutdown::ShutdownSignal;

const QUEUE_CAPACITY: usize = 8192;
const INPUT_READER_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Danger detector cadence. Cheap DB-only check; re-running quickly bounds the
/// lag on entering the danger zone. The preemptive margin absorbs bounded lag.
const DANGER_DETECTOR_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
    #[error("danger detector exited: {source}")]
    DangerDetector {
        #[source]
        source: DangerDetectorError,
    },
    #[error("danger detector join error: {source}")]
    DangerDetectorJoin {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("danger detector stopped unexpectedly")]
    DangerDetectorStoppedUnexpectedly,
    /// Deliberate shutdown triggered by the danger detector. Not an error in
    /// the usual sense — the orchestrator is expected to respawn, at which
    /// point `run_preemptive_recovery` handles it.
    #[error("danger zone detected at batch {batch_index} — stopping for recovery")]
    DangerZoneDetected { batch_index: u64 },
    #[error("RPC chain ID {rpc} does not match --chain-id {config}")]
    ChainIdMismatch { rpc: u64, config: u64 },
}

enum FirstExit {
    Signal(Option<RunError>),
    Server(RunError),
    InclusionLane(RunError),
    InputReader(RunError),
    BatchSubmitter(RunError),
    DangerDetector(RunError),
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

    let batch_submitter_private_key = config.resolve_private_key()?;

    let batch_submitter_address =
        batch_submitter_address_from_private_key(batch_submitter_private_key.as_str())?;

    // One ProtocolConfig shared across the whole process: the input reader,
    // the danger detector, and startup recovery all mirror the same
    // scheduler-acceptance rules.
    let protocol = ProtocolConfig {
        batch_submitter: batch_submitter_address,
        max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
        preemptive_margin_blocks: config.preemptive_margin_blocks,
        seconds_per_block: config.seconds_per_block,
    };

    let input_reader_config = InputReaderConfig {
        rpc_url: config.eth_rpc_url.clone(),
        app_address: config.app_address,
        poll_interval: INPUT_READER_POLL_INTERVAL,
        long_block_range_error_codes: config.long_block_range_error_codes.clone(),
    };

    // Bootstrap L1 config: try L1 first, fall back to DB cache if unreachable.
    // On first startup, L1 is required (no cache). On subsequent startups, the
    // cache allows the sequencer to start without L1 (e.g., during provider outages).
    let (mut input_reader, input_reader_genesis_block, l1_config) = match InputReader::new(
        db_path.clone(),
        shutdown.clone(),
        input_reader_config.clone(),
        protocol,
    )
    .await
    {
        Ok(reader) => {
            let genesis = reader.genesis_block();
            let input_box = reader.input_box_address();

            // Validate chain ID early — before any DB writes.
            {
                use alloy::providers::Provider;
                let check_provider = crate::l1::provider::create_provider(&config.eth_rpc_url)
                    .map_err(|e| RunError::Io(std::io::Error::other(e)))?;
                match check_provider.get_chain_id().await {
                    Ok(rpc_chain_id) if rpc_chain_id != config.chain_id => {
                        return Err(RunError::ChainIdMismatch {
                            rpc: rpc_chain_id,
                            config: config.chain_id,
                        });
                    }
                    Ok(_) => {} // verified
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "could not validate RPC chain ID at bootstrap"
                        );
                    }
                }
            }

            // Cache for future startups when L1 might be unreachable.
            if let Ok(mut s) = storage::Storage::open(&db_path) {
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
            let cache_storage = storage::Storage::open(&db_path)?;
            let cached = cache_storage
                .l1_bootstrap_cache()
                .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?;
            let Some((input_box, genesis, cached_chain_id)) = cached else {
                return Err(RunError::Io(std::io::Error::other(
                    "L1 unreachable and no bootstrap cache — \
                         L1 is required for first startup",
                )));
            };
            if cached_chain_id != config.chain_id {
                return Err(RunError::ChainIdMismatch {
                    rpc: cached_chain_id,
                    config: config.chain_id,
                });
            }

            let reader = InputReader::from_parts(
                input_reader_config,
                input_box,
                genesis,
                db_path.clone(),
                shutdown.clone(),
                protocol,
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

    tracing::info!(
        http_addr = %config.http_addr,
        data_dir = %config.data_dir,
        eth_rpc_url = %l1_config.eth_rpc_url,
        input_box_address = %l1_config.input_box_address,
        input_reader_genesis_block,
        chain_id = config.chain_id,
        app_address = %l1_config.app_address,
        batch_submitter_address = %l1_config.batch_submitter_address,
        max_wait_blocks = protocol.max_wait_blocks,
        preemptive_margin_blocks = protocol.preemptive_margin_blocks,
        danger_threshold = protocol.danger_threshold(),
        "sequencer startup"
    );

    // ── Preemptive recovery ────────────────────────────────────────
    // See docs/recovery/ for the full design and TLA+ spec.
    crate::recovery::run_preemptive_recovery(&db_path, &mut input_reader, &l1_config, &protocol)
        .await
        .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?;

    let storage = storage::Storage::open(&db_path)?;
    let (tx, mut inclusion_lane_handle) = InclusionLane::start(
        QUEUE_CAPACITY,
        shutdown.clone(),
        app,
        storage,
        InclusionLaneConfig::new(l1_config.batch_submitter_address),
    );
    let mut input_reader_handle = input_reader.start()?;

    let batch_submitter_config = BatchSubmitterConfig {
        idle_poll_interval_ms: config.batch_submitter_idle_poll_interval_ms,
    };
    let poster_config = BatchPosterConfig {
        l1_submit_address: l1_config.input_box_address,
        app_address: l1_config.app_address,
        batch_submitter_address: l1_config.batch_submitter_address,
        start_block: input_reader_genesis_block,
        confirmation_depth: config.batch_submitter_confirmation_depth,
        seconds_per_block: config.seconds_per_block,
        long_block_range_error_codes: config.long_block_range_error_codes,
    };
    let provider = build_batch_submitter_provider(&l1_config)?;

    let poster = std::sync::Arc::new(EthereumBatchPoster::new(provider, poster_config));
    let submitter = BatchSubmitter::new(
        db_path.clone(),
        poster,
        shutdown.clone(),
        batch_submitter_config,
    );
    let mut batch_submitter_handle = submitter.start().map_err(RunError::OpenStorage)?;

    let detector = DangerDetector::new(
        db_path.clone(),
        protocol,
        DANGER_DETECTOR_POLL_INTERVAL,
        shutdown.clone(),
    );
    let mut danger_detector_handle = detector.start().map_err(RunError::OpenStorage)?;

    let tx_feed = L2TxFeed::new(
        db_path.clone(),
        shutdown.clone(),
        L2TxFeedConfig {
            batch_submitter_address: Some(l1_config.batch_submitter_address),
            ..L2TxFeedConfig::default()
        },
    );

    let mut server_task = http::start(
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
        detector_result = &mut danger_detector_handle => {
            FirstExit::DangerDetector(map_danger_detector_exit(detector_result))
        }
    };

    begin_runtime_shutdown(&shutdown);
    finish_runtime(
        first_exit,
        server_task,
        inclusion_lane_handle,
        input_reader_handle,
        batch_submitter_handle,
        danger_detector_handle,
    )
    .await
}

fn batch_submitter_address_from_private_key(private_key: &str) -> Result<Address, RunError> {
    use alloy::signers::local::PrivateKeySigner;
    use std::str::FromStr;

    Ok(PrivateKeySigner::from_str(private_key)
        .map_err(|_| RunError::Io(std::io::Error::other("invalid private key")))?
        .address())
}

fn begin_runtime_shutdown(shutdown: &ShutdownSignal) {
    shutdown.request_shutdown();
}

async fn wait_for_clean_shutdown(
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
    inclusion_lane_handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
    input_reader_handle: tokio::task::JoinHandle<Result<(), InputReaderError>>,
    batch_submitter_handle: tokio::task::JoinHandle<Result<SubmitterExit, BatchSubmitterError>>,
    danger_detector_handle: tokio::task::JoinHandle<Result<DetectorExit, DangerDetectorError>>,
) -> Result<(), RunError> {
    wait_for_server_shutdown(server_task).await?;
    wait_for_lane_shutdown(inclusion_lane_handle).await?;
    wait_for_input_reader_shutdown(input_reader_handle).await?;
    wait_for_batch_submitter_shutdown(batch_submitter_handle).await?;
    wait_for_danger_detector_shutdown(danger_detector_handle).await?;
    Ok(())
}

async fn finish_runtime(
    first_exit: FirstExit,
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
    inclusion_lane_handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
    input_reader_handle: tokio::task::JoinHandle<Result<(), InputReaderError>>,
    batch_submitter_handle: tokio::task::JoinHandle<Result<SubmitterExit, BatchSubmitterError>>,
    danger_detector_handle: tokio::task::JoinHandle<Result<DetectorExit, DangerDetectorError>>,
) -> Result<(), RunError> {
    match first_exit {
        FirstExit::Signal(signal_error) => {
            let shutdown_result = wait_for_clean_shutdown(
                server_task,
                inclusion_lane_handle,
                input_reader_handle,
                batch_submitter_handle,
                danger_detector_handle,
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
            log_cleanup_result(
                "danger detector",
                wait_for_danger_detector_shutdown(danger_detector_handle).await,
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
            log_cleanup_result(
                "danger detector",
                wait_for_danger_detector_shutdown(danger_detector_handle).await,
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
            log_cleanup_result(
                "danger detector",
                wait_for_danger_detector_shutdown(danger_detector_handle).await,
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
            log_cleanup_result(
                "danger detector",
                wait_for_danger_detector_shutdown(danger_detector_handle).await,
            );
            Err(primary)
        }
        FirstExit::DangerDetector(primary) => {
            log_cleanup_result("server", wait_for_server_shutdown(server_task).await);
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
    batch_submitter_handle: tokio::task::JoinHandle<Result<SubmitterExit, BatchSubmitterError>>,
) -> Result<(), RunError> {
    match batch_submitter_handle.await {
        Ok(Ok(SubmitterExit::Shutdown)) => Ok(()),
        Ok(Err(source)) => Err(RunError::BatchSubmitter { source }),
        Err(source) => Err(RunError::BatchSubmitterJoin { source }),
    }
}

async fn wait_for_danger_detector_shutdown(
    danger_detector_handle: tokio::task::JoinHandle<Result<DetectorExit, DangerDetectorError>>,
) -> Result<(), RunError> {
    match danger_detector_handle.await {
        Ok(Ok(DetectorExit::Shutdown)) => Ok(()),
        Ok(Ok(DetectorExit::DangerZone { batch_index })) => {
            Err(RunError::DangerZoneDetected { batch_index })
        }
        Ok(Err(source)) => Err(RunError::DangerDetector { source }),
        Err(source) => Err(RunError::DangerDetectorJoin { source }),
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
    result: Result<Result<SubmitterExit, BatchSubmitterError>, tokio::task::JoinError>,
) -> RunError {
    match result {
        Ok(Ok(SubmitterExit::Shutdown)) => RunError::BatchSubmitterStoppedUnexpectedly,
        Ok(Err(source)) => RunError::BatchSubmitter { source },
        Err(source) => RunError::BatchSubmitterJoin { source },
    }
}

fn map_danger_detector_exit(
    result: Result<Result<DetectorExit, DangerDetectorError>, tokio::task::JoinError>,
) -> RunError {
    match result {
        Ok(Ok(DetectorExit::Shutdown)) => {
            // Shouldn't happen — detector Shutdown means its own shutdown signal
            // fired, which only happens after someone else triggered
            // runtime-wide shutdown. Treat this as a real exit only if nothing
            // else did first.
            RunError::DangerDetectorStoppedUnexpectedly
        }
        Ok(Ok(DetectorExit::DangerZone { batch_index })) => {
            RunError::DangerZoneDetected { batch_index }
        }
        Ok(Err(source)) => RunError::DangerDetector { source },
        Err(source) => RunError::DangerDetectorJoin { source },
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
    crate::l1::provider::create_signer_provider(&l1.eth_rpc_url, &l1.batch_submitter_private_key)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::{RunError, batch_submitter_address_from_private_key, map_danger_detector_exit};
    use crate::recovery::{DangerDetectorError, DetectorExit};
    use sequencer_core::MAX_WAIT_BLOCKS;
    use sequencer_core::protocol::ProtocolConfig;

    fn protocol_with_margin(preemptive_margin_blocks: u64) -> ProtocolConfig {
        ProtocolConfig {
            batch_submitter: alloy_primitives::Address::ZERO,
            max_wait_blocks: MAX_WAIT_BLOCKS,
            preemptive_margin_blocks,
            seconds_per_block: 12,
        }
    }

    // ── §8.4.1 preemptive_margin_blocks validation ────────────────────

    #[test]
    #[should_panic(expected = "preemptive_margin_blocks")]
    fn margin_equal_to_max_wait_panics() {
        let _ = protocol_with_margin(MAX_WAIT_BLOCKS).danger_threshold();
    }

    #[test]
    #[should_panic(expected = "preemptive_margin_blocks")]
    fn margin_greater_than_max_wait_panics() {
        let _ = protocol_with_margin(MAX_WAIT_BLOCKS + 1).danger_threshold();
    }

    #[test]
    fn margin_one_below_max_wait_yields_threshold_one() {
        assert_eq!(
            protocol_with_margin(MAX_WAIT_BLOCKS - 1).danger_threshold(),
            1
        );
    }

    #[test]
    fn zero_margin_yields_full_wait_window() {
        assert_eq!(protocol_with_margin(0).danger_threshold(), MAX_WAIT_BLOCKS);
    }

    #[test]
    fn default_margin_matches_production_setting() {
        // Default is 75 per `SEQ_PREEMPTIVE_MARGIN_BLOCKS`.
        assert_eq!(
            protocol_with_margin(75).danger_threshold(),
            MAX_WAIT_BLOCKS - 75
        );
    }

    #[test]
    fn invalid_private_key_error_does_not_echo_key_material() {
        let secret = "0xabc123SECRET";
        let err = batch_submitter_address_from_private_key(secret)
            .expect_err("invalid private key should be rejected");
        let message = err.to_string();

        assert_eq!(message, "invalid private key");
        assert!(
            !message.contains(secret),
            "private key material must not be reflected in startup errors"
        );
    }

    #[test]
    fn danger_detector_shutdown_maps_to_detector_specific_unexpected_exit() {
        let err = map_danger_detector_exit(Ok(Ok(DetectorExit::Shutdown)));
        assert!(matches!(err, RunError::DangerDetectorStoppedUnexpectedly));
    }

    #[test]
    fn danger_detector_danger_zone_maps_to_deliberate_runtime_exit() {
        let err = map_danger_detector_exit(Ok(Ok(DetectorExit::DangerZone { batch_index: 7 })));
        assert!(matches!(
            err,
            RunError::DangerZoneDetected { batch_index: 7 }
        ));
    }

    #[test]
    fn danger_detector_errors_preserve_source_category() {
        let err = map_danger_detector_exit(Ok(Err(DangerDetectorError::Join("boom".into()))));
        assert!(matches!(err, RunError::DangerDetector { .. }));
    }
}
