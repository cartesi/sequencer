// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Runtime worker lifecycle: spawn → run-until-first-exit → orderly cleanup.
//!
//! [`Workers`] owns the five runtime worker handles (server, lane, input
//! reader, batch submitter, danger detector) plus the shared shutdown signal.
//! Three methods describe its lifecycle:
//!
//! - [`Workers::spawn`]: build all configs, spawn workers, return owning struct.
//! - [`Workers::select_first_exit`]: race the workers + OS shutdown signal,
//!   return whichever fired first.
//! - [`Workers::finish`]: request shutdown, await each component (logging
//!   cleanup-time errors), surface the primary failure.
//!
//! Worker plumbing is intentionally explicit per-worker (5 fields, 5 spawn
//! statements, 5 select arms, 5 cleanup entries). Adding a sixth worker means
//! editing each of those four sites — but each edit is obvious and local.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use alloy::providers::DynProvider;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use crate::http::{self, ApiConfig};
use crate::ingress::inclusion_lane::{InclusionLane, InclusionLaneConfig, InclusionLaneError};
use crate::l1::reader::{InputReader, InputReaderError};
use crate::l1::submitter::{
    BatchPosterConfig, BatchSubmitter, BatchSubmitterConfig, BatchSubmitterError,
    EthereumBatchPoster, SubmitterExit,
};
use crate::recovery::{DangerDetector, DangerDetectorError, DetectorExit};
use crate::runtime::config::{L1Config, RunConfig};
use crate::runtime::error::{
    BatchSubmitterExit, DangerDetectorExit, InputReaderExit, LaneExit, RunError, ServerExit,
    WorkerExit,
};
use crate::runtime::shutdown::ShutdownSignal;
use sequencer_core::application::Application;
use sequencer_core::protocol::ProtocolTiming;

const QUEUE_CAPACITY: usize = 8192;
/// Danger detector cadence. Cheap DB-only check; re-running quickly bounds the
/// lag on entering the danger zone. The preemptive margin absorbs bounded lag.
const DANGER_DETECTOR_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Which event ended the `select!` race in [`Workers::select_first_exit`].
pub(crate) enum FirstExit {
    Signal(Option<RunError>),
    Worker(WorkerExit),
}

/// Inputs to [`Workers::spawn`]. Consumed entirely; the caller has nothing
/// further to do with these after the call.
///
/// Pure derivations (`db_path`, `domain`, `input_reader.genesis_block()`) are
/// computed inside `spawn` rather than threaded through here.
pub(crate) struct WorkersConfig<A: Application> {
    pub app: A,
    pub run_config: RunConfig,
    pub l1_config: L1Config,
    pub timing: ProtocolTiming,
    pub input_reader: InputReader,
}

/// Owns the five worker handles + the shutdown signal that drives all of them.
/// Construction (`spawn`) and teardown (`finish`) bracket the worker
/// lifecycle.
pub(crate) struct Workers {
    server: JoinHandle<std::io::Result<()>>,
    lane: JoinHandle<Result<(), InclusionLaneError>>,
    reader: JoinHandle<Result<(), InputReaderError>>,
    submitter: JoinHandle<Result<SubmitterExit, BatchSubmitterError>>,
    detector: JoinHandle<Result<DetectorExit, DangerDetectorError>>,
    shutdown: ShutdownSignal,
}

impl Workers {
    /// Build the worker configs, spawn each worker, return the owning struct.
    /// Logs `listening` once the HTTP server is bound.
    pub(crate) async fn spawn<A: Application + 'static>(
        cfg: WorkersConfig<A>,
    ) -> Result<Self, RunError> {
        let WorkersConfig {
            app,
            run_config,
            l1_config,
            timing,
            input_reader,
        } = cfg;

        // Derived values — kept inside `spawn` so `WorkersConfig` stays
        // minimal and these aren't computed twice in the caller.
        let db_path = run_config.db_path();
        let domain = run_config.build_domain();
        let input_reader_genesis_block = input_reader.genesis_block();

        let shutdown = ShutdownSignal::default();

        // Inclusion lane: takes the app, returns the tx-sender the HTTP
        // ingress route will publish to.
        let storage = crate::storage::Storage::open(&db_path)?;
        let dumps_dir = std::path::Path::new(&run_config.data_dir).join("dumps");
        std::fs::create_dir_all(&dumps_dir)?;
        let (tx, lane) = InclusionLane::start(
            QUEUE_CAPACITY,
            shutdown.clone(),
            app,
            storage,
            InclusionLaneConfig::new(l1_config.batch_submitter_address, dumps_dir),
        );

        // Input reader: produces safe-input rows from L1.
        let reader = input_reader.start(shutdown.clone())?;

        // Batch submitter: posts closed batches to L1.
        let poster_config = BatchPosterConfig {
            l1_submit_address: l1_config.input_box_address,
            app_address: l1_config.app_address,
            batch_submitter_address: l1_config.batch_submitter_address,
            start_block: input_reader_genesis_block,
            confirmation_depth: run_config.batch_submitter_confirmation_depth,
            seconds_per_block: run_config.seconds_per_block,
            long_block_range_error_codes: run_config.long_block_range_error_codes.clone(),
        };
        let provider = build_batch_submitter_provider(&l1_config)?;
        let poster = Arc::new(EthereumBatchPoster::new(provider, poster_config));
        let submitter_config = BatchSubmitterConfig {
            idle_poll_interval_ms: run_config.batch_submitter_idle_poll_interval_ms,
        };
        let submitter = BatchSubmitter::new(db_path.clone(), poster, submitter_config)
            .start(shutdown.clone())?;

        // Danger detector: trips startup recovery on bad DB/L1 state.
        let detector = DangerDetector::new(db_path.clone(), timing, DANGER_DETECTOR_POLL_INTERVAL)
            .start(shutdown.clone())?;

        // HTTP server (ingress /tx + egress /ws/subscribe + /health, currently merged).
        let tx_feed = L2TxFeed::new(
            db_path.clone(),
            shutdown.clone(),
            L2TxFeedConfig {
                batch_submitter_address: Some(l1_config.batch_submitter_address),
                ..L2TxFeedConfig::default()
            },
        );
        let server = http::start(
            &run_config.http_addr,
            tx,
            domain,
            A::MAX_METHOD_PAYLOAD_BYTES,
            shutdown.clone(),
            tx_feed,
            ApiConfig::default(),
        )
        .await?;
        tracing::info!(address = %run_config.http_addr, "listening");

        Ok(Self {
            server,
            lane,
            reader,
            submitter,
            detector,
            shutdown,
        })
    }

    /// Race ctrl_c against each worker's join handle. The first to complete
    /// produces the [`FirstExit`].
    pub(crate) async fn select_first_exit(&mut self) -> FirstExit {
        let shutdown_signal = tokio::signal::ctrl_c();
        tokio::pin!(shutdown_signal);
        tokio::select! {
            signal_result = &mut shutdown_signal => signal_result.into(),
            server_result = &mut self.server => server_result.into(),
            lane_result = &mut self.lane => lane_result.into(),
            reader_result = &mut self.reader => reader_result.into(),
            submitter_result = &mut self.submitter => submitter_result.into(),
            detector_result = &mut self.detector => detector_result.into(),
        }
    }

    /// Drive orderly cleanup: request shutdown, await each worker (logging
    /// cleanup-time errors), surface the primary failure (or the signal-
    /// handler error, which takes priority over component errors observed
    /// during shutdown).
    pub(crate) async fn finish(self, first_exit: FirstExit) -> Result<(), RunError> {
        self.shutdown.request_shutdown();

        let Self {
            server,
            lane,
            reader,
            submitter,
            detector,
            shutdown: _,
        } = self;
        let components: [(&'static str, ComponentShutdown); 5] = [
            ("server", Box::pin(wait_for_server_shutdown(server))),
            ("inclusion lane", Box::pin(wait_for_lane_shutdown(lane))),
            (
                "input reader",
                Box::pin(wait_for_input_reader_shutdown(reader)),
            ),
            (
                "batch submitter",
                Box::pin(wait_for_batch_submitter_shutdown(submitter)),
            ),
            (
                "danger detector",
                Box::pin(wait_for_danger_detector_shutdown(detector)),
            ),
        ];

        // Two completion modes:
        // - Worker-failure: we already have the primary; await the OTHER
        //   components for orderly cleanup, log any cleanup errors, surface
        //   the primary (wrapped to RunError).
        // - Signal-driven shutdown: an OS signal triggered shutdown. Wait for
        //   everything to drain; the signal handler's own error (if any)
        //   takes priority over any subsequent component shutdown error.
        let (worker_failure, signal_error): (Option<(&'static str, WorkerExit)>, Option<RunError>) =
            match first_exit {
                FirstExit::Signal(err) => (None, err),
                FirstExit::Worker(exit) => {
                    let name = exit.component_name();
                    (Some((name, exit)), None)
                }
            };

        if let Some((failed, primary_exit)) = worker_failure {
            for (name, fut) in components {
                if name == failed {
                    // Drop the primary's future without awaiting — its task
                    // is already done (it's what tripped the select), and
                    // we'll surface its error directly below.
                    drop(fut);
                    continue;
                }
                log_cleanup_result(name, fut.await);
            }
            return Err(RunError::Worker(primary_exit));
        }

        // Signal path: short-circuit on first shutdown error.
        let mut shutdown_error: Option<WorkerExit> = None;
        for (_, fut) in components {
            if let Err(e) = fut.await {
                shutdown_error = Some(e);
                break;
            }
        }
        match (signal_error, shutdown_error) {
            (Some(err), _) => Err(err),
            (None, Some(exit)) => Err(RunError::Worker(exit)),
            (None, None) => Ok(()),
        }
    }
}

impl WorkerExit {
    /// Human-readable component label, matching the names used in the
    /// `Workers::finish` component list.
    fn component_name(&self) -> &'static str {
        match self {
            WorkerExit::Server(_) => "server",
            WorkerExit::Lane(_) => "inclusion lane",
            WorkerExit::InputReader(_) => "input reader",
            WorkerExit::BatchSubmitter(_) => "batch submitter",
            WorkerExit::DangerDetector(_) => "danger detector",
        }
    }
}

// ── `From<JoinResult>` for FirstExit ──────────────────────────────────
//
// Each `select!` arm awaits a future and converts the result into a
// `FirstExit`. We dispatch via these `From` impls so the select arms read as
// uniform one-liners (`result.into()`); the worker-specific mapping logic
// lives here, with each input type uniquely identifying its worker.

/// ctrl_c shutdown signal: `Ok(())` = clean signal, `Err(io)` = signal-handler
/// installation failed.
impl From<Result<(), std::io::Error>> for FirstExit {
    fn from(result: Result<(), std::io::Error>) -> Self {
        FirstExit::Signal(result.err().map(RunError::from))
    }
}

impl From<Result<std::io::Result<()>, tokio::task::JoinError>> for FirstExit {
    fn from(result: Result<std::io::Result<()>, tokio::task::JoinError>) -> Self {
        FirstExit::Worker(WorkerExit::Server(match result {
            Ok(Ok(())) => ServerExit::StoppedUnexpectedly,
            Ok(Err(source)) => ServerExit::Source(source),
            Err(source) => ServerExit::Join(source),
        }))
    }
}

impl From<Result<Result<(), InclusionLaneError>, tokio::task::JoinError>> for FirstExit {
    fn from(result: Result<Result<(), InclusionLaneError>, tokio::task::JoinError>) -> Self {
        FirstExit::Worker(WorkerExit::Lane(match result {
            Ok(Ok(())) => LaneExit::StoppedUnexpectedly,
            Ok(Err(source)) => LaneExit::Source(source),
            Err(source) => LaneExit::Join(source),
        }))
    }
}

impl From<Result<Result<(), InputReaderError>, tokio::task::JoinError>> for FirstExit {
    fn from(result: Result<Result<(), InputReaderError>, tokio::task::JoinError>) -> Self {
        FirstExit::Worker(WorkerExit::InputReader(match result {
            Ok(Ok(())) => InputReaderExit::StoppedUnexpectedly,
            Ok(Err(source)) => InputReaderExit::Source(source),
            Err(source) => InputReaderExit::Join(source),
        }))
    }
}

impl From<Result<Result<SubmitterExit, BatchSubmitterError>, tokio::task::JoinError>>
    for FirstExit
{
    fn from(
        result: Result<Result<SubmitterExit, BatchSubmitterError>, tokio::task::JoinError>,
    ) -> Self {
        FirstExit::Worker(WorkerExit::BatchSubmitter(match result {
            // Worker returning `Shutdown` outside of a real shutdown means it
            // stopped on its own — treat as unexpected.
            Ok(Ok(SubmitterExit::Shutdown)) => BatchSubmitterExit::StoppedUnexpectedly,
            Ok(Err(source)) => BatchSubmitterExit::Source(source),
            Err(source) => BatchSubmitterExit::Join(source),
        }))
    }
}

impl From<Result<Result<DetectorExit, DangerDetectorError>, tokio::task::JoinError>> for FirstExit {
    fn from(
        result: Result<Result<DetectorExit, DangerDetectorError>, tokio::task::JoinError>,
    ) -> Self {
        FirstExit::Worker(WorkerExit::DangerDetector(match result {
            // Detector Shutdown means its own shutdown signal fired, which
            // only happens after runtime-wide shutdown was triggered. Treat
            // as unexpected if it wins the select.
            Ok(Ok(DetectorExit::Shutdown)) => DangerDetectorExit::StoppedUnexpectedly,
            Ok(Ok(DetectorExit::RecoveryRequired { status })) => {
                DangerDetectorExit::DangerDetected { status }
            }
            Ok(Err(source)) => DangerDetectorExit::Source(source),
            Err(source) => DangerDetectorExit::Join(source),
        }))
    }
}

// ── Shutdown waiters ───────────────────────────────────────────────────
//
// Each waiter awaits a worker's JoinHandle and converts via the per-worker
// `*Exit::from_shutdown` constructor (which knows `Ok(())` is graceful).
// Same shape per worker; kept explicit for readability.

type ComponentShutdown = Pin<Box<dyn Future<Output = Result<(), WorkerExit>> + Send>>;

async fn wait_for_server_shutdown(
    server_task: JoinHandle<std::io::Result<()>>,
) -> Result<(), WorkerExit> {
    ServerExit::from_shutdown(server_task.await).map_err(Into::into)
}

async fn wait_for_lane_shutdown(
    handle: JoinHandle<Result<(), InclusionLaneError>>,
) -> Result<(), WorkerExit> {
    LaneExit::from_shutdown(handle.await).map_err(Into::into)
}

async fn wait_for_input_reader_shutdown(
    handle: JoinHandle<Result<(), InputReaderError>>,
) -> Result<(), WorkerExit> {
    InputReaderExit::from_shutdown(handle.await).map_err(Into::into)
}

async fn wait_for_batch_submitter_shutdown(
    handle: JoinHandle<Result<SubmitterExit, BatchSubmitterError>>,
) -> Result<(), WorkerExit> {
    BatchSubmitterExit::from_shutdown(handle.await).map_err(Into::into)
}

async fn wait_for_danger_detector_shutdown(
    handle: JoinHandle<Result<DetectorExit, DangerDetectorError>>,
) -> Result<(), WorkerExit> {
    DangerDetectorExit::from_shutdown(handle.await).map_err(Into::into)
}

fn log_cleanup_result(component: &str, result: Result<(), WorkerExit>) {
    if let Err(err) = result {
        warn!(component, error = %err, "component shutdown after primary failure also errored");
    }
}

fn build_batch_submitter_provider(l1: &L1Config) -> Result<DynProvider, std::io::Error> {
    crate::l1::provider::create_signer_provider(&l1.eth_rpc_url, &l1.batch_submitter_private_key)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::DangerDetectorError;
    use crate::storage::DangerStatus;

    // ── select!-arm `From<JoinResult>` conversions ──────────────────
    //
    // The detector arm is the interesting one (DangerDetected vs Shutdown vs
    // Source vs Join). The other workers follow a uniform 3-way mapping
    // covered by the type system.

    type DetectorJoinResult =
        Result<Result<DetectorExit, DangerDetectorError>, tokio::task::JoinError>;

    #[test]
    fn detector_shutdown_in_select_maps_to_stopped_unexpectedly() {
        let result: DetectorJoinResult = Ok(Ok(DetectorExit::Shutdown));
        assert!(matches!(
            FirstExit::from(result),
            FirstExit::Worker(WorkerExit::DangerDetector(
                DangerDetectorExit::StoppedUnexpectedly
            ))
        ));
    }

    #[test]
    fn detector_recovery_required_maps_to_danger_detected() {
        let result: DetectorJoinResult = Ok(Ok(DetectorExit::RecoveryRequired {
            status: DangerStatus::ClosedBatchInDanger(7),
        }));
        assert!(matches!(
            FirstExit::from(result),
            FirstExit::Worker(WorkerExit::DangerDetector(
                DangerDetectorExit::DangerDetected {
                    status: DangerStatus::ClosedBatchInDanger(7)
                }
            ))
        ));
    }

    #[test]
    fn detector_inner_error_maps_to_source_variant() {
        let result: DetectorJoinResult = Ok(Err(DangerDetectorError::Join("boom".into())));
        assert!(matches!(
            FirstExit::from(result),
            FirstExit::Worker(WorkerExit::DangerDetector(DangerDetectorExit::Source(_)))
        ));
    }
}
