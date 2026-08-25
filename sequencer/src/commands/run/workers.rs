// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Runtime worker lifecycle: prepare → admit → launch → orderly cleanup.
//!
//! [`Workers`] owns the core runtime worker handles plus an optional live
//! Uniswap fee-oracle worker; fixed pricing has no worker.
//! Runtime construction has one explicit authority boundary:
//!
//! - [`PreparedRuntime::prepare`]: prepare every fallible or awaited
//!   dependency while launching zero workers.
//! - [`PreparedRuntime::launch`]: consume the controller's single-use durable
//!   admission witness and launch all workers in one infallible,
//!   non-yielding step, returning the owning struct.
//! - [`Workers::select_first_exit`]: race the workers + OS shutdown signal,
//!   return whichever fired first.
//! - [`Workers::finish`]: request shutdown, race all remaining components to
//!   completion (so a hung drain cannot hide a terminal exit), and surface the
//!   primary failure.
//!
//! Worker plumbing is intentionally explicit per-worker (6 fields, 6 spawn
//! statements, 6 select arms, 6 cleanup entries). Adding a seventh worker
//! means editing each of those four sites, and each is compile-forced: the
//! `Workers` literal in `launch`, and the exhaustive `let Self { .. }`
//! destructures in `select_first_exit` and `finish` — where a bound-but-
//! unused field fails CI under `-D warnings`. Keep those destructures
//! exhaustive (no `..`): they are the enforcement, not style.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use alloy::providers::DynProvider;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::commands::config::RunConfig;
use crate::commands::error::{CommandError, WorkerExit, WorkerStop};
use crate::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use crate::http::{self, ApiConfig};
use crate::ingress::inclusion_lane::{
    InclusionLane, InclusionLaneConfig, InclusionLaneError, dump_info,
};
use crate::l1::L1Config;
use crate::l1::fee_oracle::FeeOracle;
use crate::l1::reader::{InputReader, InputReaderError};
use crate::l1::submitter::{
    BatchPosterConfig, BatchSubmitter, BatchSubmitterConfig, BatchSubmitterError,
    EthereumBatchPoster, SubmitterExit,
};
use crate::recovery::{DangerDetector, DangerDetectorError, DetectorExit};
use crate::runtime::process_lock::ProcessLock;
use crate::runtime::shutdown::RuntimeScope;
use sequencer_core::application::Application;
use sequencer_core::protocol::ProtocolTiming;

const QUEUE_CAPACITY: usize = 8192;
/// Danger detector cadence. Cheap DB-only check; re-running quickly bounds the
/// lag on entering the danger zone. The preemptive margin absorbs bounded lag.
const DANGER_DETECTOR_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Which event ended the `select!` race in [`Workers::select_first_exit`].
pub(super) enum FirstExit {
    Signal(Option<CommandError>),
    Worker(WorkerExit),
    /// A terminal fault was contained by a runtime component. The black-box
    /// terminal-cause row was attempted but is best-effort telemetry; the
    /// exit code and logs carry the verdict if it failed.
    Contained,
}

/// Inputs to [`PreparedRuntime::prepare`]. Consumed entirely; the caller has
/// nothing further to do with these after the call.
///
/// Everything here is built by `run` because the recovery reducer consumes
/// it or must run after it; anything the workers alone need is derived
/// inside `prepare`.
///
/// No genesis app instance: `setup` already registered the finalized genesis
/// snapshot, so the lane reloads via `A::from_dump`. No EIP-712 domain
/// either: `prepare` derives it from the pinned deployment identity that
/// `l1_config` carries verbatim.
pub(super) struct WorkersConfig {
    pub run_config: RunConfig,
    pub l1_config: L1Config,
    pub timing: ProtocolTiming,
    pub input_reader: InputReader,
    /// Launch-ready without source I/O: setup supplied the persisted price,
    /// and a Uniswap worker quotes on its first supervised iteration. Fixed
    /// pricing has no worker (`None`).
    pub fee_oracle: Option<FeeOracle>,
    /// Exclusive data-directory ownership, acquired before bootstrap and
    /// transferred into the runtime lifetime at worker admission.
    pub process_lock: ProcessLock,
}

/// Requests shutdown if construction or runtime ownership is dropped; a panic
/// instead enters terminal containment before unwind can strand runtime work.
/// Every spawned worker retains a [`RuntimeScope`] clone, which also retains
/// the process lock, so exclusivity remains held until the workers finish even
/// though Drop cannot join asynchronously.
struct ShutdownOnDrop(RuntimeScope);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // A panic in the admitted runtime controller is a trusted-code
            // failure just like a worker panic. Contain while the runtime
            // lifetime is still owned so the terminal watchdog bounds any
            // worker or detached blocking operation left behind by unwind.
            self.0
                .contain_storage_invariant_failure("runtime controller panicked");
        } else {
            self.0.request_shutdown();
        }
    }
}

/// Fully prepared runtime state: exactly the arguments `launch` hands to
/// the workers. Configuration is consumed by `prepare`; only launch-ready
/// values cross the authority boundary. Owning this value launches no tasks
/// and grants no sequencing authority.
pub(super) struct PreparedRuntime<A> {
    input_reader: InputReader,
    api_config: ApiConfig,
    fee_oracle: Option<FeeOracle>,
    storage: crate::storage::Storage,
    lane_config: InclusionLaneConfig,
    submitter: BatchSubmitter<EthereumBatchPoster>,
    detector: DangerDetector,
    tx_feed: L2TxFeed,
    listener: tokio::net::TcpListener,
    bound_addr: std::net::SocketAddr,
    snapshot_state: http::SnapshotState,
    shutdown: RuntimeScope,
    shutdown_on_drop: ShutdownOnDrop,
    /// No `A` *value* is ever held — `setup` registered the genesis snapshot
    /// and the lane reloads via `A::from_dump`. The parameter feeds the
    /// lane's `start`, the payload bound, and the snapshot path hook.
    _application: PhantomData<fn() -> A>,
}

/// Owns the runtime worker handles + the shutdown signal that drives them.
/// [`PreparedRuntime::launch`] and teardown ([`Workers::finish`]) bracket the
/// worker lifecycle.
pub(super) struct Workers {
    server: JoinHandle<std::io::Result<()>>,
    lane: JoinHandle<Result<(), InclusionLaneError>>,
    reader: JoinHandle<Result<(), InputReaderError>>,
    submitter: JoinHandle<Result<SubmitterExit, BatchSubmitterError>>,
    detector: JoinHandle<Result<DetectorExit, DangerDetectorError>>,
    fee_oracle: Option<JoinHandle<Result<(), crate::l1::fee_oracle::worker::FeeOracleError>>>,
    shutdown: RuntimeScope,
    _shutdown_on_drop: ShutdownOnDrop,
}

impl<A: Application + Clone + Sync + 'static> PreparedRuntime<A> {
    /// Prepare every fallible or awaited runtime dependency while launching
    /// zero tasks. Durable admission remains the controller's responsibility.
    pub(super) async fn prepare(cfg: WorkersConfig) -> Result<Self, CommandError> {
        let WorkersConfig {
            run_config,
            l1_config,
            timing,
            input_reader,
            fee_oracle,
            process_lock,
        } = cfg;

        // Derived values — kept inside `prepare` so `WorkersConfig` stays
        // minimal and these aren't computed twice in the caller.
        let db_path = run_config.db_path();
        // The EIP-712 domain is the signature-verification boundary, so it
        // is derived from exactly one source: the pinned deployment identity
        // that `l1_config` carries verbatim out of `load_setup_identity`.
        let domain = sequencer_core::build_input_domain(
            l1_config.identity.chain_id,
            l1_config.identity.app_address,
        );

        // The scope is the runtime-lifetime capability: every worker
        // receives a clone, and every clone keeps the process lock alive.
        // The drop guard requests shutdown on any partial-construction `?`,
        // panic unwind, or cancellation of the owning `run` future.
        let shutdown = RuntimeScope::new(process_lock);
        let shutdown_on_drop = ShutdownOnDrop(shutdown.clone());
        // Durable terminal-fault recorder: best-effort telemetry. A
        // successful write appends the black-box terminal-cause row; a
        // failed write loses only the black-box copy — the exit code and
        // logs still carry the verdict, and a persistent fault re-detects
        // fail-loud on the next boot that reads it.
        install_terminal_fault_recorder(&shutdown, db_path.clone());

        let mut storage = crate::storage::Storage::open(&db_path)?;
        let dumps_dir = std::path::Path::new(&run_config.data_dir).join("dumps");
        std::fs::create_dir_all(&dumps_dir)?;

        // Authority-neutral snapshot repair before the boundary; the five
        // order-critical steps are documented in `startup_hygiene`.
        super::startup_hygiene::run_snapshot_hygiene::<A>(&mut storage, &dumps_dir)?;

        // Prepare every remaining fallible or awaited dependency before the
        // authority boundary. Cancellation observes zero workers.
        input_reader.preflight_storage()?;

        let poster_config = BatchPosterConfig {
            l1_submit_address: l1_config.identity.input_box_address,
            app_address: l1_config.identity.app_address,
            batch_submitter_address: l1_config.identity.batch_submitter_address,
            start_block: l1_config.identity.app_deployment_block,
            confirmation_depth: run_config.batch_submitter_confirmation_depth,
            seconds_per_block: timing.seconds_per_block,
            long_block_range_error_codes: run_config.long_block_range_error_codes.clone(),
            expected_chain_id: l1_config.identity.chain_id,
        };
        let provider = build_batch_submitter_provider(&l1_config)?;
        let poster = Arc::new(EthereumBatchPoster::new(
            provider,
            poster_config,
            shutdown.clone(),
        ));
        let submitter_config = BatchSubmitterConfig {
            idle_poll_interval_ms: run_config.batch_submitter_idle_poll_interval_ms,
        };
        let submitter = BatchSubmitter::new(
            db_path.clone(),
            poster,
            submitter_config,
            shutdown.process_lock(),
        );
        submitter.preflight_storage()?;

        let detector = DangerDetector::new(
            db_path.clone(),
            timing,
            DANGER_DETECTOR_POLL_INTERVAL,
            shutdown.process_lock(),
        );
        detector.preflight_storage()?;

        let tx_feed = L2TxFeed::new(
            db_path.clone(),
            shutdown.clone(),
            L2TxFeedConfig::new(l1_config.identity.batch_submitter_address),
        );

        // Configuration ends here: the remaining values are exactly what
        // `launch` hands to the workers, so the config structs never cross
        // the authority boundary.
        let lane_config =
            InclusionLaneConfig::new(l1_config.identity.batch_submitter_address, dumps_dir)
                .with_max_batch_open(run_config.max_batch_open());
        let api_config = ApiConfig::new(domain, A::MAX_METHOD_PAYLOAD_BYTES);
        let listener = tokio::net::TcpListener::bind(&run_config.http_addr).await?;
        let bound_addr = listener.local_addr()?;
        let snapshot_state = http::SnapshotState {
            db_path,
            // The DB row stores the dump *directory*; the app's state
            // file lives under its `state` subtree.
            state_file_in_dump: |dump_dir| A::state_file_in_dump(&dump_info::app_prefix(dump_dir)),
        };

        Ok(Self {
            input_reader,
            api_config,
            fee_oracle,
            storage,
            lane_config,
            submitter,
            detector,
            tx_feed,
            listener,
            bound_addr,
            snapshot_state,
            shutdown,
            shutdown_on_drop,
            _application: PhantomData,
        })
    }

    /// Launch all workers in one infallible, non-async, non-yielding step.
    /// Consuming the [`crate::recovery::RuntimeAdmission`] witness here is
    /// what gates launching on the reducer's fresh clean decision — the
    /// witness's sole constructor is `admit_runtime`, and launch uses it up.
    pub(super) fn launch(self, _admission: crate::recovery::RuntimeAdmission) -> Workers {
        let Self {
            input_reader,
            api_config,
            fee_oracle,
            storage,
            lane_config,
            submitter,
            detector,
            tx_feed,
            listener,
            bound_addr,
            snapshot_state,
            shutdown,
            shutdown_on_drop,
            _application: _,
        } = self;

        let (tx, lane) =
            InclusionLane::<A>::start(QUEUE_CAPACITY, shutdown.clone(), storage, lane_config);
        let reader = input_reader.start_preflighted(shutdown.clone());
        let submitter = submitter.start_preflighted(shutdown.clone());
        let detector = detector.start_preflighted(shutdown.clone());
        let fee_oracle = fee_oracle.map(|oracle| oracle.start(shutdown.clone()));
        // HTTP server (ingress /tx + egress /ws/subscribe + /health, currently merged).
        let server = http::start_on_listener(
            listener,
            tx,
            shutdown.clone(),
            tx_feed,
            api_config,
            snapshot_state,
        );
        tracing::info!(address = %bound_addr, "listening");

        Workers {
            server,
            lane,
            reader,
            submitter,
            detector,
            fee_oracle,
            shutdown,
            _shutdown_on_drop: shutdown_on_drop,
        }
    }
}

impl Workers {
    /// Race an OS shutdown signal against each worker's join handle. The first to complete
    /// produces the [`FirstExit`].
    pub(super) async fn select_first_exit(&mut self) -> FirstExit {
        // Exhaustive destructure (no `..`): a new worker field fails to
        // compile here, so it cannot be forgotten in the race below — the
        // same forcing `finish`'s destructure provides for cleanup.
        let Self {
            server,
            lane,
            reader,
            submitter,
            detector,
            fee_oracle,
            shutdown,
            _shutdown_on_drop: _,
        } = self;
        let shutdown_signal = os_shutdown_signal();
        tokio::pin!(shutdown_signal);
        tokio::select! {
            biased;
            _ = shutdown.wait_for_shutdown() => {
                if shutdown.is_storage_invariant_contained() {
                    FirstExit::Contained
                } else {
                    // Externally requested shutdown without a contained
                    // fault: treated like a signal-driven drain.
                    FirstExit::Signal(None)
                }
            }
            signal_result = &mut shutdown_signal => FirstExit::signal(signal_result),
            server_result = &mut *server =>
                FirstExit::Worker(WorkerExit::Server(WorkerStop::from_select(server_result))),
            lane_result = &mut *lane =>
                FirstExit::Worker(WorkerExit::Lane(WorkerStop::from_select(lane_result))),
            reader_result = &mut *reader =>
                FirstExit::Worker(WorkerExit::InputReader(WorkerStop::from_select(reader_result))),
            submitter_result = &mut *submitter =>
                FirstExit::Worker(WorkerExit::BatchSubmitter(WorkerStop::from_select(
                    // A worker returning `Shutdown` outside a real shutdown
                    // means it stopped on its own — the unexpected case.
                    submitter_result.map(|r| r.map(|SubmitterExit::Shutdown| ())),
                ))),
            detector_result = &mut *detector => FirstExit::detector(detector_result),
            fee_oracle_result = async {
                match fee_oracle.as_mut() {
                    Some(handle) => handle.await,
                    // Fixed mode: no oracle worker, so this arm never resolves.
                    None => std::future::pending().await,
                }
            } => FirstExit::Worker(WorkerExit::FeeOracle(WorkerStop::from_select(
                fee_oracle_result,
            ))),
        }
    }

    /// Drive orderly cleanup: request shutdown, poll all workers concurrently
    /// to completion, and surface the primary failure. A sticky storage
    /// invariant fault or terminal cleanup error always takes precedence over
    /// an earlier nonterminal worker/signal result. Concurrent polling matters:
    /// a hung drain must not hide a terminal exit that arms the hard watchdog.
    pub(super) async fn finish(self, first_exit: FirstExit) -> Result<(), CommandError> {
        match &first_exit {
            // Already contained by the raising component; its best-effort
            // terminal-cause journal append was attempted there.
            FirstExit::Contained => {}
            FirstExit::Worker(exit) if exit.is_terminal() => {
                // Log the typed exit here: the terminal return path below
                // reports the invariant-violation class, and the cause must
                // not be flattened out of the operator's view.
                tracing::error!(
                    component = exit.worker_id().label(),
                    error = %exit,
                    "terminal worker exit; containing runtime"
                );
                self.shutdown.contain_storage_invariant_failure(format!(
                    "terminal {} worker exit: {exit}",
                    exit.worker_id().label()
                ));
            }
            FirstExit::Signal(_) | FirstExit::Worker(_) => {
                self.shutdown.request_shutdown();
            }
        }

        let Self {
            server,
            lane,
            reader,
            submitter,
            detector,
            fee_oracle,
            shutdown,
            _shutdown_on_drop,
        } = self;
        let mut components: Vec<(WorkerId, ComponentShutdown)> = vec![
            (WorkerId::Server, Box::pin(wait_for_server_shutdown(server))),
            (WorkerId::Lane, Box::pin(wait_for_lane_shutdown(lane))),
            (
                WorkerId::InputReader,
                Box::pin(wait_for_input_reader_shutdown(reader)),
            ),
            (
                WorkerId::BatchSubmitter,
                Box::pin(wait_for_batch_submitter_shutdown(submitter)),
            ),
            (
                WorkerId::DangerDetector,
                Box::pin(wait_for_danger_detector_shutdown(detector)),
            ),
        ];
        if let Some(fee_oracle) = fee_oracle {
            components.push((
                WorkerId::FeeOracle,
                Box::pin(wait_for_fee_oracle_shutdown(fee_oracle)),
            ));
        }

        // One drain, two phases:
        // - "cleanup-time" (worker failure): the primary is already in hand;
        //   every OTHER component is awaited for orderly cleanup.
        // - "shutdown-time" (signal or already-contained): everything drains;
        //   the signal handler's own error outranks any later component error.
        let (worker_failure, signal_error): (Option<(WorkerId, WorkerExit)>, Option<CommandError>) =
            match first_exit {
                FirstExit::Signal(err) => (None, err),
                FirstExit::Worker(exit) => {
                    let id = exit.worker_id();
                    (Some((id, exit)), None)
                }
                FirstExit::Contained => (None, None),
            };

        if let Some((failed, _)) = &worker_failure {
            let failed_index = components
                .iter()
                .position(|(id, _)| id == failed)
                .expect("primary worker must be present in the cleanup set");
            // Drop the primary's future without awaiting — its task is
            // already done (it tripped the select) and re-polling a completed
            // JoinHandle panics. Its typed exit is surfaced by the precedence
            // match below.
            drop(components.swap_remove(failed_index));
        }

        let phase = if worker_failure.is_some() {
            "cleanup-time"
        } else {
            "shutdown-time"
        };

        // Await EVERY remaining component — in both phases — so each worker's
        // JoinHandle is joined and its task fully drains. A `break` here would
        // drop the remaining components' futures un-awaited, which DETACHES
        // those tasks (only `JoinHandle::abort()` cancels a dropped handle) —
        // they'd be killed mid-drain at runtime teardown, the exact
        // abrupt-write case the startup snapshot hygiene (sweep/gc/re-stamp)
        // exists to clean up after. The primary removed above is the one
        // deliberate exception: its task already completed, so awaiting it
        // would panic rather than drain anything. Keep the first ordinary
        // error to surface; terminal errors contain instead.
        let mut drain_error: Option<WorkerExit> = None;
        while let Some((id, result)) = next_component_shutdown(&mut components).await {
            if let Err(e) = result {
                warn!(
                    component = id.label(),
                    phase,
                    error = %e,
                    "component errored during runtime drain"
                );
                if e.is_terminal() {
                    shutdown.contain_storage_invariant_failure(format!(
                        "{phase} {} worker exit: {e}",
                        id.label()
                    ));
                } else if drain_error.is_none() {
                    drain_error = Some(e);
                }
            }
        }

        // The single precedence site: contained > primary > signal > first
        // drain error.
        match (
            contained_verdict(&shutdown),
            worker_failure,
            signal_error,
            drain_error,
        ) {
            (Some(contained), ..) => Err(contained),
            (None, Some((_, primary_exit)), _, _) => Err(CommandError::Worker(primary_exit)),
            (None, None, Some(signal_err), _) => Err(signal_err),
            (None, None, None, Some(exit)) => Err(CommandError::Worker(exit)),
            (None, None, None, None) => Ok(()),
        }
    }
}

fn install_terminal_fault_recorder(shutdown: &RuntimeScope, db_path: String) {
    shutdown.set_fault_recorder(Arc::new(move |cause: &str| {
        match crate::storage::Storage::open_writer(&db_path) {
            Ok(mut storage) => {
                if let Err(err) =
                    storage.record_terminal_fault(crate::storage::LifecycleCommand::Run, cause)
                {
                    tracing::warn!(error = %err, "terminal cause not recorded in the black box; the exit code and this log carry the verdict");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "terminal cause not recorded in the black box (storage open failed); the exit code and this log carry the verdict");
            }
        }
    }));
}

async fn os_shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Stable identity of each long-lived worker. The `finish` worker-failure path
/// skips the already-exited worker by matching on this enum, not on a label
/// string that could silently drift from the component-array order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerId {
    Server,
    Lane,
    InputReader,
    BatchSubmitter,
    DangerDetector,
    FeeOracle,
}

impl WorkerId {
    /// Human-readable label for logs, matching the `Workers::finish` list.
    fn label(self) -> &'static str {
        match self {
            WorkerId::Server => "server",
            WorkerId::Lane => "inclusion lane",
            WorkerId::InputReader => "input reader",
            WorkerId::BatchSubmitter => "batch submitter",
            WorkerId::DangerDetector => "danger detector",
            WorkerId::FeeOracle => "fee oracle",
        }
    }
}

impl WorkerExit {
    /// Which worker produced this exit.
    fn worker_id(&self) -> WorkerId {
        match self {
            WorkerExit::Server(_) => WorkerId::Server,
            WorkerExit::Lane(_) => WorkerId::Lane,
            WorkerExit::InputReader(_) => WorkerId::InputReader,
            WorkerExit::BatchSubmitter(_) => WorkerId::BatchSubmitter,
            WorkerExit::DangerDetector(_) | WorkerExit::DangerDetected { .. } => {
                WorkerId::DangerDetector
            }
            WorkerExit::FeeOracle(_) => WorkerId::FeeOracle,
        }
    }
}

// ── FirstExit constructors ────────────────────────────────────────────
//
// Named constructors, not `From` impls keyed on the worker's result type:
// two workers sharing a result type would silently misroute through blanket
// dispatch, and the select arms should say which worker they map.

impl FirstExit {
    /// ctrl_c shutdown signal: `Ok(())` = clean signal, `Err(io)` =
    /// signal-handler installation failed.
    fn signal(result: Result<(), std::io::Error>) -> Self {
        FirstExit::Signal(result.err().map(CommandError::from))
    }

    /// The detector's `RecoveryRequired` trip is a first-class exit, not an
    /// error; `Shutdown` outside a real shutdown means it stopped on its own.
    fn detector(
        result: Result<Result<DetectorExit, DangerDetectorError>, tokio::task::JoinError>,
    ) -> Self {
        let stop = match result {
            Ok(Ok(DetectorExit::RecoveryRequired { status })) => {
                return FirstExit::Worker(WorkerExit::DangerDetected { status });
            }
            Ok(Ok(DetectorExit::Shutdown)) => WorkerStop::StoppedUnexpectedly,
            Ok(Err(source)) => WorkerStop::Source(source),
            Err(source) => WorkerStop::Join(source),
        };
        FirstExit::Worker(WorkerExit::DangerDetector(stop))
    }
}

// ── Shutdown waiters ───────────────────────────────────────────────────
//
// Each component future awaits a worker's JoinHandle and converts via
// `WorkerStop::from_shutdown` (which knows `Ok` is the graceful drain).

type ComponentShutdown = Pin<Box<dyn Future<Output = Result<(), WorkerExit>> + Send>>;

/// Observe whichever remaining component finishes next. Cleanup must poll all
/// workers concurrently: otherwise one hung component can hide a terminal
/// panic/invariant exit from a later slot forever, preventing containment and
/// its hard abort bound from ever arming.
///
/// No `.await` may separate the inner `Poll::Ready` from the `swap_remove` —
/// a cancellation in that window would leave a completed future in the set,
/// and the next poll of it panics. `swap_remove` also reorders the set, so
/// completion order among concurrently-ready components is unspecified; only
/// terminal exits carry precedence.
async fn next_component_shutdown(
    components: &mut Vec<(WorkerId, ComponentShutdown)>,
) -> Option<(WorkerId, Result<(), WorkerExit>)> {
    if components.is_empty() {
        return None;
    }

    let (ready_index, result) = std::future::poll_fn(|cx| {
        for (index, (_, component)) in components.iter_mut().enumerate() {
            if let Poll::Ready(result) = component.as_mut().poll(cx) {
                return Poll::Ready((index, result));
            }
        }
        Poll::Pending
    })
    .await;
    let (id, _) = components.swap_remove(ready_index);
    Some((id, result))
}

/// The single post-cleanup containment check: if a terminal fault was
/// contained anywhere (primary, cleanup, or a non-worker component), surface
/// the terminal class. The cause is present whenever containment reads true
/// (they are one `OnceLock`); recorder failure loses only the journal's
/// telemetry copy of the cause, never the sticky in-process verdict or the
/// terminal exit class.
fn contained_verdict(shutdown: &RuntimeScope) -> Option<CommandError> {
    shutdown
        .containment_cause()
        .map(|cause| CommandError::StorageInvariantViolation {
            cause: cause.to_string(),
        })
}

async fn wait_for_server_shutdown(
    server_task: JoinHandle<std::io::Result<()>>,
) -> Result<(), WorkerExit> {
    WorkerStop::from_shutdown(server_task.await).map_err(WorkerExit::Server)
}

async fn wait_for_lane_shutdown(
    handle: JoinHandle<Result<(), InclusionLaneError>>,
) -> Result<(), WorkerExit> {
    WorkerStop::from_shutdown(handle.await).map_err(WorkerExit::Lane)
}

async fn wait_for_input_reader_shutdown(
    handle: JoinHandle<Result<(), InputReaderError>>,
) -> Result<(), WorkerExit> {
    WorkerStop::from_shutdown(handle.await).map_err(WorkerExit::InputReader)
}

async fn wait_for_batch_submitter_shutdown(
    handle: JoinHandle<Result<SubmitterExit, BatchSubmitterError>>,
) -> Result<(), WorkerExit> {
    WorkerStop::from_shutdown(handle.await.map(|r| r.map(|SubmitterExit::Shutdown| ())))
        .map_err(WorkerExit::BatchSubmitter)
}

/// Detector `Shutdown` is the graceful drain; a `RecoveryRequired` trip
/// during drain still surfaces as the first-class danger exit.
async fn wait_for_danger_detector_shutdown(
    handle: JoinHandle<Result<DetectorExit, DangerDetectorError>>,
) -> Result<(), WorkerExit> {
    match handle.await {
        Ok(Ok(DetectorExit::Shutdown)) => Ok(()),
        Ok(Ok(DetectorExit::RecoveryRequired { status })) => {
            Err(WorkerExit::DangerDetected { status })
        }
        Ok(Err(source)) => Err(WorkerExit::DangerDetector(WorkerStop::Source(source))),
        Err(source) => Err(WorkerExit::DangerDetector(WorkerStop::Join(source))),
    }
}

async fn wait_for_fee_oracle_shutdown(
    handle: JoinHandle<Result<(), crate::l1::fee_oracle::worker::FeeOracleError>>,
) -> Result<(), WorkerExit> {
    WorkerStop::from_shutdown(handle.await).map_err(WorkerExit::FeeOracle)
}

// Built once during preparation (sync, raw `create_signer_provider`). The
// submitter is long-lived, so a one-shot startup chain-id check would go stale; the
// keyed-write guard instead lives in `EthereumBatchPoster::submit_batches`,
// which re-confirms the chain id immediately before every productive send.
// The key was already verified against the pinned submitter, so a build
// failure here is a bad RPC URL / client misconfiguration — the same
// deterministic terminal class as every signer misconfig.
fn build_batch_submitter_provider(l1: &L1Config) -> Result<DynProvider, CommandError> {
    crate::l1::provider::create_signer_provider(
        &l1.eth_rpc_url,
        l1.batch_submitter_private_key.expose_secret(),
        l1.allow_insecure_rpc,
    )
    .map_err(|message| {
        CommandError::Bootstrap(crate::commands::error::BootstrapError::SignerMisconfig { message })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::DangerDetectorError;
    use crate::storage::DangerStatus;
    use clap::Parser;

    #[derive(Clone, Default)]
    struct StartupProbeApp {
        progress: sequencer_core::application::ApplicationProgress,
    }

    impl Application for StartupProbeApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = 0;

        fn validate_user_op(
            &self,
            _sender: alloy_primitives::Address,
            _user_op: &sequencer_core::user_op::UserOp,
            _current_fee: u16,
        ) -> Result<(), sequencer_core::application::InvalidReason> {
            Ok(())
        }

        fn apply_valid_user_op(
            &mut self,
            _capability: sequencer_core::application::ApplyInputCapability<'_>,
            _user_op: &sequencer_core::l2_tx::ValidUserOp,
            _safe_block: u64,
        ) -> Result<sequencer_core::application::AppOutputs, sequencer_core::application::AppError>
        {
            Ok(Vec::new())
        }

        fn apply_direct_input(
            &mut self,
            _capability: sequencer_core::application::ApplyInputCapability<'_>,
            _input: &sequencer_core::l2_tx::DirectInput,
        ) -> Result<sequencer_core::application::AppOutputs, sequencer_core::application::AppError>
        {
            Ok(Vec::new())
        }

        fn execution_progress(&self) -> &sequencer_core::application::ApplicationProgress {
            &self.progress
        }

        fn execution_progress_mut(
            &mut self,
            _capability: sequencer_core::application::ProgressCommitCapability<'_>,
        ) -> &mut sequencer_core::application::ApplicationProgress {
            &mut self.progress
        }

        fn from_dump(
            _prefix: &std::path::Path,
        ) -> Result<Self, sequencer_core::application::AppError> {
            Ok(Self::default())
        }

        fn create_dump(
            &self,
            prefix: &std::path::Path,
        ) -> Result<(), sequencer_core::application::AppError> {
            std::fs::create_dir(prefix)?;
            std::fs::write(prefix.join("state"), [])?;
            Ok(())
        }

        fn delete_dump(
            prefix: &std::path::Path,
        ) -> Result<(), sequencer_core::application::AppError> {
            std::fs::remove_dir_all(prefix)?;
            Ok(())
        }

        fn state_file_in_dump(prefix: &std::path::Path) -> std::path::PathBuf {
            prefix.join("state")
        }
    }

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
            FirstExit::detector(result),
            FirstExit::Worker(WorkerExit::DangerDetector(WorkerStop::StoppedUnexpectedly))
        ));
    }

    #[test]
    fn detector_recovery_required_maps_to_danger_detected() {
        let result: DetectorJoinResult = Ok(Ok(DetectorExit::RecoveryRequired {
            status: DangerStatus::ClosedBatchInDanger(7),
        }));
        assert!(matches!(
            FirstExit::detector(result),
            FirstExit::Worker(WorkerExit::DangerDetected {
                status: DangerStatus::ClosedBatchInDanger(7)
            })
        ));
    }

    #[test]
    fn production_recorder_poison_is_durable_and_first_writer_wins() {
        let db = temp_db("runtime-lifecycle-recorder");
        let mut storage =
            Storage::initialize_for_command(&db.path, crate::storage::LifecycleCommand::Setup)
                .expect("initialize setup");
        storage
            .insert_initial_finalized_dump(&db._dir.path().join("finalized"), 0, 0, 0, 0)
            .expect("register finalized snapshot");
        storage.complete_setup().expect("complete setup");
        drop(storage);
        let shutdown = RuntimeScope::default();
        install_terminal_fault_recorder(&shutdown, db.path.clone());

        shutdown.contain_storage_invariant_failure("first terminal cause");
        shutdown.contain_storage_invariant_failure("echo");

        let fault = Storage::open_read_only(&db.path)
            .expect("reopen")
            .latest_terminal_fault()
            .expect("read")
            .expect("recorded fault");
        assert_eq!(fault.command, crate::storage::LifecycleCommand::Run);
        assert_eq!(fault.cause, "first terminal cause");
    }

    /// Workers whose tasks idle until shutdown. The shared signal retains the
    /// first cause independently of the best-effort durable recorder.
    fn waiting_workers(shutdown: &RuntimeScope) -> (Workers, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("workers tempdir");
        let server = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                shutdown.wait_for_shutdown().await;
                Ok(())
            }
        });
        let lane = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                shutdown.wait_for_shutdown().await;
                Ok(())
            }
        });
        let reader = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                shutdown.wait_for_shutdown().await;
                Ok(())
            }
        });
        let submitter = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                shutdown.wait_for_shutdown().await;
                Ok(SubmitterExit::Shutdown)
            }
        });
        let detector = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                shutdown.wait_for_shutdown().await;
                Ok(DetectorExit::Shutdown)
            }
        });
        (
            Workers {
                server,
                lane,
                reader,
                submitter,
                detector,
                fee_oracle: None,
                shutdown: shutdown.clone(),
                _shutdown_on_drop: ShutdownOnDrop(shutdown.clone()),
            },
            dir,
        )
    }

    #[tokio::test]
    async fn dropped_runtime_scope_keeps_lock_until_detached_worker_stops() {
        let dir = tempfile::tempdir().expect("runtime data dir");
        let data_dir = dir.path().to_str().expect("utf8 path");
        let process_lock = ProcessLock::acquire(data_dir).expect("acquire runtime lock");
        let shutdown = RuntimeScope::new(process_lock);
        let shutdown_on_drop = ShutdownOnDrop(shutdown.clone());
        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        // Model a worker spawned before a later startup step fails. Its join
        // handle is dropped (Tokio detaches it), but its signal clone keeps
        // process ownership until it has observed shutdown and finished.
        let worker_shutdown = shutdown.clone();
        let worker = tokio::spawn(async move {
            worker_shutdown.wait_for_shutdown().await;
            let _ = release_rx.await;
            drop(worker_shutdown);
            let _ = stopped_tx.send(());
        });
        drop(worker);

        drop(shutdown_on_drop);
        drop(shutdown);
        let refused = ProcessLock::acquire(data_dir)
            .expect_err("a detached but live worker must retain process ownership");
        assert!(matches!(
            refused,
            crate::runtime::process_lock::ProcessLockError::Locked { .. }
        ));

        release_tx.send(()).expect("release worker");
        stopped_rx.await.expect("worker stopped");
        ProcessLock::acquire(data_dir).expect("lock releases after the last worker exits");
    }

    #[test]
    fn controller_panic_is_contained_before_scope_unwind_finishes() {
        let shutdown = RuntimeScope::default();
        let guard = ShutdownOnDrop(shutdown.clone());

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("controller panic probe");
        }));

        assert!(panic.is_err());
        assert!(shutdown.is_storage_invariant_contained());
        assert!(shutdown.is_shutdown_requested());
        assert_eq!(
            shutdown.containment_cause(),
            Some("runtime controller panicked")
        );
    }

    fn startup_workers_config(
        http_addr: String,
    ) -> (tempfile::TempDir, String, String, WorkersConfig) {
        const KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

        let dir = tempfile::tempdir().expect("runtime data dir");
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();
        let cli = crate::harness::Cli::try_parse_from(vec![
            "sequencer".to_string(),
            "run".to_string(),
            "--http-addr".to_string(),
            http_addr,
            "--data-dir".to_string(),
            data_dir.clone(),
            "--eth-rpc-url".to_string(),
            "http://127.0.0.1:8545".to_string(),
            "--batch-submitter-private-key".to_string(),
            KEY.to_string(),
        ])
        .expect("parse run config");
        let crate::harness::Command::Run(run_config) = cli.command else {
            panic!("expected run config");
        };
        let run_config = *run_config;
        let db_path = run_config.db_path();
        let timing = run_config.protocol_timing().expect("valid timing");
        let submitter_address =
            crate::commands::batch_submitter_address_from_private_key(KEY).expect("key address");
        let dumps_dir = dir.path().join("dumps");
        std::fs::create_dir(&dumps_dir).expect("create dumps dir");
        let finalized = dumps_dir.join("genesis");
        create_structured_dump(&finalized);
        let mut storage = crate::storage::Storage::initialize_for_command(
            &db_path,
            crate::storage::LifecycleCommand::Setup,
        )
        .expect("open storage");
        storage
            .insert_initial_finalized_dump(&finalized, 0, 0, 0, 0)
            .expect("register finalized dump");
        storage
            .append_safe_inputs(0, &[], submitter_address, &timing)
            .expect("initialize safe head");
        storage
            .initialize_open_state(0, crate::storage::SafeInputRange::empty_at(0))
            .expect("initialize Tip");
        storage.complete_setup().expect("complete setup");
        drop(storage);

        // One identity literal feeds both the reader and the L1 bundle, so
        // the fixture cannot drift the way hand-copied fields could.
        let identity = crate::storage::DeploymentIdentity {
            chain_id: 31337,
            app_address: "0x1111111111111111111111111111111111111111"
                .parse()
                .expect("app address"),
            input_box_address: "0x2222222222222222222222222222222222222222"
                .parse()
                .expect("input box address"),
            app_deployment_block: 0,
            batch_submitter_address: submitter_address,
            fee_oracle: crate::storage::FeeOracleIdentity::Fixed { log_gas_price: 0 },
        };
        let input_reader = InputReader::from_parts(
            crate::l1::reader::InputReaderConfig {
                rpc_url: run_config.eth_rpc_url.clone(),
                allow_insecure_rpc: false,
                app_address: identity.app_address,
                poll_interval: crate::commands::INPUT_READER_POLL_INTERVAL,
                long_block_range_error_codes: run_config.long_block_range_error_codes.clone(),
                expected_chain_id: identity.chain_id,
            },
            identity.input_box_address,
            identity.app_deployment_block,
            db_path.clone(),
            identity.batch_submitter_address,
            timing,
            ProcessLock::test(),
        );
        let l1_config = L1Config {
            identity,
            eth_rpc_url: run_config.eth_rpc_url.clone(),
            batch_submitter_private_key: crate::l1::SubmitterKey::new(KEY.to_string()),
            allow_insecure_rpc: false,
        };
        let process_lock = ProcessLock::acquire(&data_dir).expect("acquire runtime lock");

        (
            dir,
            data_dir,
            db_path,
            WorkersConfig {
                run_config,
                l1_config,
                timing,
                input_reader,
                fee_oracle: None,
                process_lock,
            },
        )
    }

    #[tokio::test]
    async fn occupied_http_port_fails_before_any_worker_launches() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied listener");
        let http_addr = occupied.local_addr().expect("listener address").to_string();
        let (_dir, data_dir, _db_path, workers_config) = startup_workers_config(http_addr);

        let result = PreparedRuntime::<StartupProbeApp>::prepare(workers_config).await;
        let err = match result {
            Ok(_) => panic!("occupied listener must refuse preparation"),
            Err(err) => err,
        };
        assert!(
            matches!(&err, CommandError::Io(source) if source.kind() == std::io::ErrorKind::AddrInUse),
            "expected AddrInUse, got {err:?}"
        );
        // The boundary check: a worker spawned during preparation would
        // retain a `RuntimeScope` clone and keep the process lock held, so
        // this acquire succeeding proves zero workers launched.
        ProcessLock::acquire(&data_dir)
            .expect("failed preparation must release ownership with zero live workers");
    }

    #[tokio::test]
    async fn launch_requires_a_fresh_admission_after_preparation() {
        let (_dir, data_dir, db_path, workers_config) =
            startup_workers_config("127.0.0.1:0".to_string());

        let timing = workers_config.timing;
        let prepared = PreparedRuntime::<StartupProbeApp>::prepare(workers_config)
            .await
            .expect("runtime prepares");
        let admission = crate::recovery::admit_runtime(&db_path, &timing)
            .expect("the reducer admits over clean facts and returns its witness");
        let workers = prepared.launch(admission);

        workers
            .finish(FirstExit::Signal(None))
            .await
            .expect("workers drain cleanly");
        let _released = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ProcessLock::acquire(&data_dir) {
                    Ok(lock) => break lock,
                    Err(crate::runtime::process_lock::ProcessLockError::Locked { .. }) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("unexpected lock acquisition failure: {error}"),
                }
            }
        })
        .await
        .expect("nested runtime work releases process ownership after clean drain");
    }

    #[tokio::test]
    async fn preparation_outliving_clean_facts_cannot_launch() {
        let (_dir, _data_dir, db_path, workers_config) =
            startup_workers_config("127.0.0.1:0".to_string());
        let timing = workers_config.timing;

        let prepared = PreparedRuntime::<StartupProbeApp>::prepare(workers_config)
            .await
            .expect("runtime prepares");

        // Simulate time moving behind the persisted local-progress baseline
        // while preparation was in flight. The final reducer invocation must
        // classify the new clock refusal before durable Live or launch.
        let future_ms = i64::try_from(crate::clock::unix_now_ms()).expect("clock fits")
            + i64::try_from(timing.seconds_per_block * 2_000).expect("offset fits");
        crate::storage::Storage::open_connection(&db_path)
            .expect("open raw test connection")
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [future_ms],
            )
            .expect("move progress baseline into the future");

        let error = crate::recovery::admit_runtime(&db_path, &timing)
            .expect_err("aged clean decision must not admit");
        assert!(matches!(error, crate::recovery::RecoveryError::Retry(_)));
        drop(prepared);
    }

    #[tokio::test]
    async fn contained_component_fault_surfaces_first_cause() {
        // A non-worker component contains a fault: the select yields
        // Contained, and finish's verdict carries the cause read back from
        // the in-memory first-winner record.
        let shutdown = RuntimeScope::default();
        let (mut workers, _dir) = waiting_workers(&shutdown);

        shutdown.contain_storage_invariant_failure("egress component fault: dangling dump row");
        let first_exit = workers.select_first_exit().await;
        assert!(matches!(first_exit, FirstExit::Contained));

        let err = workers
            .finish(first_exit)
            .await
            .expect_err("contained fault must fail run");
        let CommandError::StorageInvariantViolation { cause } = &err else {
            panic!("expected terminal invariant violation, got {err:?}");
        };
        assert_eq!(cause, "egress component fault: dangling dump row");
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
    }

    #[tokio::test]
    async fn containment_overrides_an_already_selected_nonterminal_worker_exit() {
        let shutdown = RuntimeScope::default();
        let (workers, _dir) = waiting_workers(&shutdown);

        shutdown.contain_storage_invariant_failure("fault raced the worker exit");
        let err = workers
            .finish(FirstExit::Worker(WorkerExit::Server(
                WorkerStop::StoppedUnexpectedly,
            )))
            .await
            .expect_err("containment must outrank the nonterminal exit");
        assert!(matches!(
            err,
            CommandError::StorageInvariantViolation { .. }
        ));
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
    }

    #[tokio::test]
    async fn terminal_primary_worker_exit_contains_with_typed_cause() {
        let shutdown = RuntimeScope::default();
        let (workers, _dir) = waiting_workers(&shutdown);

        let err = workers
            .finish(FirstExit::Worker(WorkerExit::DangerDetected {
                status: DangerStatus::CanonicalDivergence(7),
            }))
            .await
            .expect_err("canonical divergence must fail run");
        let CommandError::StorageInvariantViolation { cause } = &err else {
            panic!("expected terminal invariant violation, got {err:?}");
        };
        assert!(
            cause.contains("terminal") && cause.contains("worker exit"),
            "a terminal primary exit must surface its typed cause, got: {cause}"
        );
        assert!(shutdown.is_storage_invariant_contained());
    }

    #[tokio::test]
    async fn panicked_primary_contains_terminal_fault() {
        let shutdown = RuntimeScope::default();
        let (mut workers, _dir) = waiting_workers(&shutdown);
        workers.lane = tokio::spawn(async { panic!("lane task panicked in test") });

        let first_exit = workers.select_first_exit().await;
        let err = workers
            .finish(first_exit)
            .await
            .expect_err("panicked worker must fail run");
        assert!(matches!(
            err,
            CommandError::StorageInvariantViolation { .. }
        ));
        assert!(shutdown.is_storage_invariant_contained());
    }

    #[tokio::test]
    async fn recovery_primary_uses_ordinary_shutdown_without_containment() {
        let shutdown = RuntimeScope::default();
        let (workers, _dir) = waiting_workers(&shutdown);

        let err = workers
            .finish(FirstExit::Worker(WorkerExit::DangerDetected {
                status: DangerStatus::TipInDanger(3),
            }))
            .await
            .expect_err("danger exit must fail run");
        assert!(matches!(err, CommandError::Worker(_)));
        assert!(!shutdown.is_storage_invariant_contained());
    }

    #[tokio::test]
    async fn transient_primary_uses_ordinary_shutdown_without_containment() {
        let shutdown = RuntimeScope::default();
        let (workers, _dir) = waiting_workers(&shutdown);

        let err = workers
            .finish(FirstExit::Worker(WorkerExit::Server(
                WorkerStop::StoppedUnexpectedly,
            )))
            .await
            .expect_err("unexpected server stop must fail run");
        assert!(matches!(err, CommandError::Worker(_)));
        assert!(!shutdown.is_storage_invariant_contained());
    }

    /// The oracle's cleanup entry exists only when the worker does, and the
    /// primary-removal expect relies on that coupling — pin the `Some` limb,
    /// which no production-path test exercises (every fixture is fixed-mode).
    #[tokio::test]
    async fn fee_oracle_primary_exit_drains_through_its_conditional_entry() {
        let shutdown = RuntimeScope::default();
        let (mut workers, _dir) = waiting_workers(&shutdown);
        workers.fee_oracle = Some(tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                shutdown.wait_for_shutdown().await;
                Ok(())
            }
        }));

        let err = workers
            .finish(FirstExit::Worker(WorkerExit::FeeOracle(
                WorkerStop::StoppedUnexpectedly,
            )))
            .await
            .expect_err("unexpected oracle stop must fail run");
        assert!(matches!(
            err,
            CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::StoppedUnexpectedly))
        ));
        assert!(!shutdown.is_storage_invariant_contained());
    }

    #[tokio::test]
    async fn terminal_cleanup_error_overrides_nonterminal_primary_exit() {
        let shutdown = RuntimeScope::default();
        let (mut workers, _dir) = waiting_workers(&shutdown);
        workers.reader = tokio::spawn(async {
            Err(InputReaderError::StorageTaskPanicked {
                operation: "reading corrupt state during cleanup",
            })
        });

        let err = workers
            .finish(FirstExit::Worker(WorkerExit::Server(
                WorkerStop::StoppedUnexpectedly,
            )))
            .await
            .expect_err("terminal cleanup error must outrank nonterminal primary");
        let CommandError::StorageInvariantViolation { cause } = &err else {
            panic!("expected terminal invariant violation, got {err:?}");
        };
        assert!(
            cause.contains("cleanup-time")
                && cause.contains("reading corrupt state during cleanup"),
            "a cleanup-time terminal exit must surface its typed cause, got: {cause}"
        );
        assert!(shutdown.is_storage_invariant_contained());
    }

    #[tokio::test]
    async fn terminal_cleanup_is_observed_while_another_component_is_still_draining() {
        let shutdown = RuntimeScope::default();
        let (mut workers, _dir) = waiting_workers(&shutdown);
        let (release_server, server_released) = tokio::sync::oneshot::channel();
        workers.server = tokio::spawn(async move {
            let _ = server_released.await;
            Ok(())
        });
        workers.reader = tokio::spawn(async {
            Err(InputReaderError::StorageTaskPanicked {
                operation: "terminal cleanup behind a draining server",
            })
        });

        let finish = tokio::spawn(workers.finish(FirstExit::Worker(WorkerExit::Lane(
            WorkerStop::StoppedUnexpectedly,
        ))));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !shutdown.is_storage_invariant_contained() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a draining earlier component must not hide a terminal cleanup exit");

        release_server.send(()).expect("release server drain");
        let err = finish
            .await
            .expect("finish task")
            .expect_err("terminal cleanup must fail the run");
        let CommandError::StorageInvariantViolation { cause } = err else {
            panic!("expected terminal invariant violation");
        };
        assert!(cause.contains("terminal cleanup behind a draining server"));
    }

    #[tokio::test]
    async fn signal_cleanup_contains_terminal_fault() {
        let shutdown = RuntimeScope::default();
        let (mut workers, _dir) = waiting_workers(&shutdown);
        workers.reader = tokio::spawn(async {
            Err(InputReaderError::StorageTaskPanicked {
                operation: "reading corrupt state during signal drain",
            })
        });

        let err = workers
            .finish(FirstExit::Signal(None))
            .await
            .expect_err("terminal fault during signal drain must fail run");
        let CommandError::StorageInvariantViolation { cause } = &err else {
            panic!("expected terminal invariant violation, got {err:?}");
        };
        assert!(cause.contains("shutdown-time"), "got: {cause}");
        assert!(shutdown.is_storage_invariant_contained());
    }

    use crate::commands::test_support::create_structured_dump;
    use crate::storage::Storage;
    use crate::storage::test_helpers::temp_db;

    #[test]
    fn detector_inner_error_maps_to_source_variant() {
        let result: DetectorJoinResult = Ok(Err(DangerDetectorError::Join("boom".into())));
        assert!(matches!(
            FirstExit::detector(result),
            FirstExit::Worker(WorkerExit::DangerDetector(WorkerStop::Source(_)))
        ));
    }
}
