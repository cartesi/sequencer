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
//! - [`PreparedRuntime::admit`]: consume the controller's single-use durable
//!   admission witness.
//! - [`AdmittedRuntime::launch`]: launch all workers in one infallible,
//!   non-yielding step and return the owning struct.
//! - [`Workers::select_first_exit`]: race the workers + OS shutdown signal,
//!   return whichever fired first.
//! - [`Workers::finish`]: request shutdown, race all remaining components to
//!   completion (so a hung drain cannot hide a terminal exit), and surface the
//!   primary failure.
//!
//! Worker plumbing is intentionally explicit per-worker (6 fields, 6 spawn
//! statements, 6 select arms, 6 cleanup entries). Adding a seventh worker means
//! editing each of those four sites — but each edit is obvious and local.

use std::future::Future;
use std::marker::PhantomData;
use std::path::PathBuf;
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
    InclusionLane, InclusionLaneConfig, InclusionLaneError, dump_info, dump_info::delete_dump_dir,
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

#[cfg(test)]
static WORKER_LAUNCH_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static WORKER_LAUNCH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Which event ended the `select!` race in [`Workers::select_first_exit`].
pub(crate) enum FirstExit {
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
/// No genesis app instance: `setup` already registered the finalized genesis
/// snapshot, so the lane reloads via `A::from_dump`. The `domain` is built by
/// `run` from the pinned deployment identity.
pub(crate) struct WorkersConfig {
    pub run_config: RunConfig,
    pub l1_config: L1Config,
    pub timing: ProtocolTiming,
    pub input_reader: InputReader,
    pub domain: alloy_sol_types::Eip712Domain,
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

/// Fully prepared runtime state. Owning this value launches no tasks and
/// grants no sequencing authority.
pub(super) struct PreparedRuntime<A> {
    run_config: RunConfig,
    l1_config: L1Config,
    input_reader: InputReader,
    domain: alloy_sol_types::Eip712Domain,
    fee_oracle: Option<FeeOracle>,
    storage: crate::storage::Storage,
    dumps_dir: PathBuf,
    submitter: BatchSubmitter<EthereumBatchPoster>,
    detector: DangerDetector,
    tx_feed: L2TxFeed,
    listener: tokio::net::TcpListener,
    shutdown: RuntimeScope,
    shutdown_on_drop: ShutdownOnDrop,
    db_path: String,
    _application: PhantomData<fn() -> A>,
}

/// Single-use launch capability. Only a prepared runtime plus the
/// controller's durable-admission witness can construct it.
#[must_use = "an admitted runtime must be launched without yielding"]
pub(super) struct AdmittedRuntime<A> {
    prepared: PreparedRuntime<A>,
    _admission: crate::recovery::RuntimeAdmission,
}

/// Owns the runtime worker handles + the shutdown signal that drives them.
/// [`AdmittedRuntime::launch`] and teardown ([`Workers::finish`]) bracket the
/// worker lifecycle.
pub(crate) struct Workers {
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
            domain,
            fee_oracle,
            process_lock,
        } = cfg;

        // Derived values — kept inside `prepare` so `WorkersConfig` stays
        // minimal and these aren't computed twice in the caller.
        let db_path = run_config.db_path();
        let app_deployment_block = input_reader.app_deployment_block();

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

        // Inclusion lane: takes the app, returns the tx-sender the HTTP
        // ingress route will publish to.
        let mut storage = crate::storage::Storage::open(&db_path)?;
        let dumps_dir = std::path::Path::new(&run_config.data_dir).join("dumps");
        std::fs::create_dir_all(&dumps_dir)?;

        // Authority-neutral runtime preparation, in this order:
        //
        // 1. Reset stale leases. A crashed previous run may have left
        //    `lease_count > 0` on dumps that aren't being read by
        //    anyone now; without this, GC would skip them forever.
        // 2. Require the finalized snapshot (always-load invariant). `setup`
        //    registered the genesis snapshot and `run` gated on atomic setup
        //    completion, so it must be present — a missing one
        //    is a terminal incomplete-setup, not a cold-start to paper over
        //    (run holds no genesis app instance).
        // 3. GC SQLite-side: drop any rows now unreferenced after
        //    promotions or invalidations that finalized just before
        //    the previous shutdown.
        // 4. Orphan FS sweep: remove directories under `dumps_dir`
        //    that aren't tracked by SQLite (crash-during-create_dump
        //    or crash-during-GC-after-row-delete artifacts).
        storage.reset_dump_leases()?;
        require_finalized_snapshot(&mut storage)?;
        restamp_finalized_promotion(&mut storage)?;
        let gc_removed = snapshot_gc_at_startup::<A>(&mut storage)?;
        let sweep_removed = sweep_orphan_dumps::<A>(&mut storage, &dumps_dir)?;
        tracing::debug!(
            gc_removed,
            sweep_removed,
            "snapshot startup cleanup complete",
        );

        // Prepare every remaining fallible or awaited dependency before the
        // authority boundary. Cancellation observes zero workers.
        input_reader.preflight_storage()?;

        let poster_config = BatchPosterConfig {
            l1_submit_address: l1_config.input_box_address,
            app_address: l1_config.app_address,
            batch_submitter_address: l1_config.batch_submitter_address,
            start_block: app_deployment_block,
            confirmation_depth: run_config.batch_submitter_confirmation_depth,
            seconds_per_block: run_config.timing.seconds_per_block,
            long_block_range_error_codes: run_config.long_block_range_error_codes.clone(),
            expected_chain_id: l1_config.chain_id,
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
            L2TxFeedConfig {
                batch_submitter_address: Some(l1_config.batch_submitter_address),
                ..L2TxFeedConfig::default()
            },
        );
        let listener = tokio::net::TcpListener::bind(&run_config.http_addr).await?;

        Ok(Self {
            run_config,
            l1_config,
            input_reader,
            domain,
            fee_oracle,
            storage,
            dumps_dir,
            submitter,
            detector,
            tx_feed,
            listener,
            shutdown,
            shutdown_on_drop,
            db_path,
            _application: PhantomData,
        })
    }

    /// Consume the admission witness and seal the prepared state into the
    /// only capability from which workers can be launched.
    pub(super) fn admit(self, admission: crate::recovery::RuntimeAdmission) -> AdmittedRuntime<A> {
        AdmittedRuntime {
            prepared: self,
            _admission: admission,
        }
    }
}

impl<A: Application + Clone + Sync + 'static> AdmittedRuntime<A> {
    /// Launch all workers in one infallible, non-async, non-yielding step.
    pub(super) fn launch(self) -> Workers {
        let Self {
            prepared:
                PreparedRuntime {
                    run_config,
                    l1_config,
                    input_reader,
                    domain,
                    fee_oracle,
                    storage,
                    dumps_dir,
                    submitter,
                    detector,
                    tx_feed,
                    listener,
                    shutdown,
                    shutdown_on_drop,
                    db_path,
                    _application: _,
                },
            _admission: _,
        } = self;

        #[cfg(test)]
        WORKER_LAUNCH_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, lane) = InclusionLane::<A>::start(
            QUEUE_CAPACITY,
            shutdown.clone(),
            storage,
            InclusionLaneConfig::new(l1_config.batch_submitter_address, dumps_dir)
                .with_max_batch_open(run_config.max_batch_open()),
        );
        let reader = input_reader.start_preflighted(shutdown.clone());
        let submitter = submitter.start_preflighted(shutdown.clone());
        let detector = detector.start_preflighted(shutdown.clone());
        let fee_oracle = fee_oracle.map(|oracle| oracle.start(shutdown.clone()));
        // HTTP server (ingress /tx + egress /ws/subscribe + /health, currently merged).
        let server = http::start_on_listener(
            listener,
            tx,
            domain,
            A::MAX_METHOD_PAYLOAD_BYTES,
            shutdown.clone(),
            tx_feed,
            ApiConfig::default(),
            http::SnapshotState {
                db_path: db_path.clone(),
                // The DB row stores the dump *directory*; the app's state
                // file lives under its `state` subtree.
                state_file_in_dump: |dump_dir| {
                    A::state_file_in_dump(&crate::ingress::inclusion_lane::dump_info::app_prefix(
                        dump_dir,
                    ))
                },
            },
        );
        tracing::info!(address = %run_config.http_addr, "listening");

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
    pub(crate) async fn select_first_exit(&mut self) -> FirstExit {
        let shutdown_signal = os_shutdown_signal();
        tokio::pin!(shutdown_signal);
        tokio::select! {
            biased;
            _ = self.shutdown.wait_for_shutdown() => {
                if self.shutdown.is_storage_invariant_contained() {
                    FirstExit::Contained
                } else {
                    // Externally requested shutdown without a contained
                    // fault: treated like a signal-driven drain.
                    FirstExit::Signal(None)
                }
            }
            signal_result = &mut shutdown_signal => FirstExit::signal(signal_result),
            server_result = &mut self.server =>
                FirstExit::worker(WorkerExit::Server(WorkerStop::from_select(server_result))),
            lane_result = &mut self.lane =>
                FirstExit::worker(WorkerExit::Lane(WorkerStop::from_select(lane_result))),
            reader_result = &mut self.reader =>
                FirstExit::worker(WorkerExit::InputReader(WorkerStop::from_select(reader_result))),
            submitter_result = &mut self.submitter =>
                FirstExit::worker(WorkerExit::BatchSubmitter(WorkerStop::from_select(
                    // A worker returning `Shutdown` outside a real shutdown
                    // means it stopped on its own — the unexpected case.
                    submitter_result.map(|r| r.map(|SubmitterExit::Shutdown| ())),
                ))),
            detector_result = &mut self.detector => FirstExit::detector(detector_result),
            fee_oracle_result = async {
                match self.fee_oracle.as_mut() {
                    Some(handle) => Some(handle.await),
                    None => std::future::pending().await,
                }
            } => FirstExit::worker(WorkerExit::FeeOracle(WorkerStop::from_select(
                fee_oracle_result.expect("fixed mode does not select an oracle exit"),
            ))),
        }
    }

    /// Drive orderly cleanup: request shutdown, poll all workers concurrently
    /// to completion, and surface the primary failure. A sticky storage
    /// invariant fault or terminal cleanup error always takes precedence over
    /// an earlier nonterminal worker/signal result. Concurrent polling matters:
    /// a hung drain must not hide a terminal exit that arms the hard watchdog.
    pub(crate) async fn finish(self, first_exit: FirstExit) -> Result<(), CommandError> {
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

        // Two completion modes:
        // - Worker-failure: we already have the primary; await the OTHER
        //   components for orderly cleanup, log any cleanup errors, surface
        //   the primary (wrapped to CommandError).
        // - Signal-driven shutdown: an OS signal triggered shutdown. Wait for
        //   everything to drain; the signal handler's own error (if any)
        //   takes priority over any subsequent component shutdown error.
        let (worker_failure, signal_error): (Option<(WorkerId, WorkerExit)>, Option<CommandError>) =
            match first_exit {
                FirstExit::Signal(err) => (None, err),
                FirstExit::Worker(exit) => {
                    let id = exit.worker_id();
                    (Some((id, exit)), None)
                }
                FirstExit::Contained => (None, None),
            };

        if let Some((failed, primary_exit)) = worker_failure {
            let failed_index = components
                .iter()
                .position(|(id, _)| *id == failed)
                .expect("primary worker must be present in the cleanup set");
            // Drop the primary's future without awaiting — its task is
            // already done (it tripped the select), and its typed exit is
            // surfaced directly below.
            drop(components.swap_remove(failed_index));

            while let Some((id, result)) = next_component_shutdown(&mut components).await {
                if let Err(exit) = result {
                    warn!(
                        component = id.label(),
                        error = %exit,
                        "component shutdown after primary failure also errored"
                    );
                    if exit.is_terminal() {
                        shutdown.contain_storage_invariant_failure(format!(
                            "cleanup-time {} worker exit: {exit}",
                            id.label()
                        ));
                    }
                }
            }
            if let Some(err) = contained_verdict(&shutdown) {
                return Err(err);
            }
            return Err(CommandError::Worker(primary_exit));
        }

        // Signal path: await EVERY component so each worker's JoinHandle is
        // joined and its task fully drains. A `break` here would drop the
        // remaining components' futures un-awaited, which DETACHES those tasks
        // (only `JoinHandle::abort()` cancels a dropped handle) — they'd be
        // killed mid-drain at runtime teardown, the exact abrupt-write case the
        // startup snapshot hygiene (sweep/gc/re-stamp) exists to clean up after.
        // Keep the first ordinary error to surface and separately retain the
        // first terminal error; log every failure.
        let mut shutdown_error: Option<WorkerExit> = None;
        while let Some((id, result)) = next_component_shutdown(&mut components).await {
            if let Err(e) = result {
                warn!(component = id.label(), error = %e, "component errored during signal-driven shutdown");
                if e.is_terminal() {
                    shutdown.contain_storage_invariant_failure(format!(
                        "shutdown-time {} worker exit: {e}",
                        id.label()
                    ));
                    continue;
                }
                if shutdown_error.is_none() {
                    shutdown_error = Some(e);
                }
            }
        }
        if let Some(err) = contained_verdict(&shutdown) {
            return Err(err);
        }
        match (signal_error, shutdown_error) {
            (Some(err), _) => Err(err),
            (None, Some(exit)) => Err(CommandError::Worker(exit)),
            (None, None) => Ok(()),
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
    fn worker(exit: WorkerExit) -> Self {
        FirstExit::Worker(exit)
    }

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
    let (id, completed) = components.swap_remove(ready_index);
    drop(completed);
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
        &l1.batch_submitter_private_key,
        l1.allow_insecure_rpc,
    )
    .map_err(|message| {
        CommandError::Bootstrap(crate::commands::error::BootstrapError::SignerMisconfig { message })
    })
}

/// Require the finalized snapshot the lane will `from_dump` against. `setup`
/// registers the genesis snapshot and `run` gates on atomic setup completion,
/// so by the time the lane starts the snapshot must exist. A missing
/// one means the DB's setup is incomplete/corrupt — terminal
/// `SetupNotComplete` (re-run `setup`), not a cold-start to silently heal.
fn require_finalized_snapshot(storage: &mut crate::storage::Storage) -> Result<(), CommandError> {
    if storage.finalized_dump()?.is_none() {
        return Err(CommandError::Bootstrap(
            crate::commands::error::BootstrapError::SetupNotComplete,
        ));
    }
    Ok(())
}

/// Re-stamp `B` into the finalized dump's `info.toml` from the
/// authoritative DB row. Idempotent; closes the crash window between a
/// promotion's commit and the lane's in-place stamp.
fn restamp_finalized_promotion(storage: &mut crate::storage::Storage) -> Result<(), CommandError> {
    if let Some(finalized) = storage.finalized_dump()? {
        let path = finalized.dump.prefix;
        dump_info::stamp_promoted_inclusion_block(&path, finalized.inclusion_block)
            .map_err(|source| CommandError::ReferencedSnapshotArtifact { path, source })?;
    }
    Ok(())
}

/// Drop any dump rows that are now unreferenced (no pending, no
/// finalized, no leases). The companion `sweep_orphan_dumps` then
/// catches anything on disk that this leaves behind, plus
/// crash-during-create_dump orphans the SQLite layer never saw.
fn snapshot_gc_at_startup<A: Application + 'static>(
    storage: &mut crate::storage::Storage,
) -> Result<usize, CommandError> {
    let removed = storage.gc_unreferenced_dumps()?;
    for row in &removed {
        if let Err(err) = delete_dump_dir::<A>(&row.prefix) {
            tracing::warn!(
                error = %err,
                prefix = ?row.prefix,
                "startup GC: filesystem delete failed; orphan left for sweep",
            );
        }
    }
    Ok(removed.len())
}

/// Walk `dumps_dir` and delete any dump directory that isn't in
/// `Storage::list_dump_rows`. Catches:
///
/// - **crash-during-create**: a dump dir exists on disk (possibly
///   without its app subtree or `info.toml`) but no SQLite row was
///   ever written for it.
/// - **crash-during-GC**: SQLite row was deleted but the filesystem
///   delete either wasn't reached or failed.
///
/// Filesystem-only — no SQLite writes here. Failures log and
/// continue (the next startup retries). The post-`ensure_finalized`
/// ordering matters: the genesis dump's dir is in
/// `list_dump_rows` by the time this runs, so we never delete it.
fn sweep_orphan_dumps<A: Application + 'static>(
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<usize, CommandError> {
    let known: std::collections::HashSet<std::path::PathBuf> = storage
        .list_dump_rows()?
        .into_iter()
        .map(|row| row.prefix)
        .collect();
    let mut removed = 0;
    for entry in std::fs::read_dir(dumps_dir)? {
        let entry = entry?;
        let path = entry.path();
        if known.contains(&path) {
            continue;
        }
        match delete_dump_dir::<A>(&path) {
            Ok(()) => removed += 1,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    ?path,
                    "orphan dump sweep: delete failed; will retry next startup",
                );
            }
        }
    }
    Ok(removed)
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

        let app_address: alloy_primitives::Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .expect("app address");
        let input_box_address: alloy_primitives::Address =
            "0x2222222222222222222222222222222222222222"
                .parse()
                .expect("input box address");
        let input_reader = InputReader::from_parts(
            crate::l1::reader::InputReaderConfig {
                rpc_url: run_config.eth_rpc_url.clone(),
                allow_insecure_rpc: false,
                app_address,
                poll_interval: crate::commands::INPUT_READER_POLL_INTERVAL,
                long_block_range_error_codes: run_config.long_block_range_error_codes.clone(),
                expected_chain_id: 31337,
            },
            input_box_address,
            0,
            db_path.clone(),
            submitter_address,
            timing,
            ProcessLock::test(),
        );
        let l1_config = L1Config {
            eth_rpc_url: run_config.eth_rpc_url.clone(),
            input_box_address,
            app_address,
            batch_submitter_private_key: KEY.to_string(),
            batch_submitter_address: submitter_address,
            chain_id: 31337,
            allow_insecure_rpc: false,
        };
        let domain = sequencer_core::build_input_domain(31337, app_address);
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
                domain,
                fee_oracle: None,
                process_lock,
            },
        )
    }

    #[tokio::test]
    async fn occupied_http_port_fails_before_any_worker_launches() {
        let _launch_test = WORKER_LAUNCH_TEST_LOCK.lock().await;
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied listener");
        let http_addr = occupied.local_addr().expect("listener address").to_string();
        let (_dir, data_dir, _db_path, workers_config) = startup_workers_config(http_addr);

        WORKER_LAUNCH_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
        let result = PreparedRuntime::<StartupProbeApp>::prepare(workers_config).await;
        let err = match result {
            Ok(_) => panic!("occupied listener must refuse preparation"),
            Err(err) => err,
        };
        assert!(
            matches!(&err, CommandError::Io(source) if source.kind() == std::io::ErrorKind::AddrInUse),
            "expected AddrInUse, got {err:?}"
        );
        assert_eq!(
            WORKER_LAUNCH_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "worker launch began before preparation completed"
        );
        ProcessLock::acquire(&data_dir)
            .expect("failed preparation must release ownership with zero live workers");
    }

    #[tokio::test]
    async fn launch_requires_a_fresh_admission_after_preparation() {
        let _launch_test = WORKER_LAUNCH_TEST_LOCK.lock().await;
        let (_dir, data_dir, db_path, workers_config) =
            startup_workers_config("127.0.0.1:0".to_string());

        WORKER_LAUNCH_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
        let timing = workers_config.timing;
        let prepared = PreparedRuntime::<StartupProbeApp>::prepare(workers_config)
            .await
            .expect("runtime prepares");
        assert_eq!(
            WORKER_LAUNCH_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "preparation must launch zero workers"
        );
        let admission = crate::recovery::admit_runtime(&db_path, &timing)
            .expect("the reducer admits over clean facts and returns its witness");
        let workers = prepared.admit(admission).launch();
        assert_eq!(
            WORKER_LAUNCH_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "launch must cross the worker boundary exactly once"
        );

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
        let _launch_test = WORKER_LAUNCH_TEST_LOCK.lock().await;
        let (_dir, _data_dir, db_path, workers_config) =
            startup_workers_config("127.0.0.1:0".to_string());
        let timing = workers_config.timing;

        WORKER_LAUNCH_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
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
        assert_eq!(
            WORKER_LAUNCH_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
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

    use crate::commands::test_support::{SweepTestApp, create_structured_dump};
    use crate::storage::Storage;
    use crate::storage::test_helpers::temp_db;

    #[test]
    fn startup_restamp_rejects_missing_referenced_snapshot_as_terminal() {
        let db = temp_db("restamp-missing-snapshot");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let root = tempfile::tempdir().expect("snapshot parent");
        let missing = root.path().join("missing");
        storage
            .insert_finalized_dump(&missing, 7, 0)
            .expect("register missing fixture");

        let err = restamp_finalized_promotion(&mut storage)
            .expect_err("a durable DB reference cannot point at a missing artifact");

        assert!(matches!(
            &err,
            CommandError::ReferencedSnapshotArtifact { .. }
        ));
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
    }

    #[test]
    fn startup_restamp_rejects_corrupt_referenced_snapshot_as_terminal() {
        let db = temp_db("restamp-corrupt-snapshot");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let root = tempfile::tempdir().expect("snapshot parent");
        let corrupt = root.path().join("corrupt");
        std::fs::create_dir(&corrupt).expect("create snapshot directory");
        std::fs::write(corrupt.join("info.toml"), "not = valid = toml")
            .expect("write corrupt metadata");
        storage
            .insert_finalized_dump(&corrupt, 7, 0)
            .expect("register corrupt fixture");

        let err = restamp_finalized_promotion(&mut storage)
            .expect_err("corrupt durable metadata cannot be retried as operational I/O");

        assert!(matches!(
            &err,
            CommandError::ReferencedSnapshotArtifact { .. }
        ));
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
    }

    #[test]
    fn sweep_orphan_dumps_removes_directories_not_in_storage() {
        let db = temp_db("sweep-orphans");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");

        // Tracked dump (in SQLite).
        let tracked = dumps_dir.path().join("tracked");
        create_structured_dump(&tracked);
        storage
            .insert_finalized_dump(&tracked, 0, 0)
            .expect("register tracked");

        // Two orphans (NOT in SQLite). One is fully formed; the other
        // mimics a crash between dir creation and the app dump (no
        // `state` subtree) — the sweep must remove both.
        let orphan_a = dumps_dir.path().join("orphan-a");
        let orphan_b = dumps_dir.path().join("orphan-b");
        create_structured_dump(&orphan_a);
        std::fs::create_dir(&orphan_b).expect("orphan b dir");

        let removed = sweep_orphan_dumps::<SweepTestApp>(&mut storage, dumps_dir.path()).unwrap();
        assert_eq!(removed, 2);
        assert!(tracked.exists(), "tracked dump must survive");
        assert!(!orphan_a.exists());
        assert!(!orphan_b.exists());
    }

    #[test]
    fn sweep_orphan_dumps_on_empty_directory_is_noop() {
        let db = temp_db("sweep-empty");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");

        let removed = sweep_orphan_dumps::<SweepTestApp>(&mut storage, dumps_dir.path()).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn snapshot_gc_at_startup_removes_unreferenced_rows() {
        let db = temp_db("gc-startup");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");

        // Two dumps: superseded + finalized.
        let superseded = dumps_dir.path().join("superseded");
        let finalized = dumps_dir.path().join("finalized");
        create_structured_dump(&superseded);
        create_structured_dump(&finalized);
        storage
            .insert_pending_dump(&superseded, 0, 0)
            .expect("pending 0");
        storage.promote_finalized(0, 0).expect("promote 0");
        storage
            .insert_pending_dump(&finalized, 1, 0)
            .expect("pending 1");
        storage.promote_finalized(1, 0).expect("promote 1");
        // `superseded`'s row is now unreferenced (replaced by
        // finalized's promotion), but the directory is still on disk.

        let removed = snapshot_gc_at_startup::<SweepTestApp>(&mut storage).unwrap();
        assert_eq!(removed, 1);
        assert!(!superseded.exists(), "GC removed the superseded directory");
        assert!(finalized.exists(), "current finalized survived");
    }

    #[test]
    fn detector_inner_error_maps_to_source_variant() {
        let result: DetectorJoinResult = Ok(Err(DangerDetectorError::Join("boom".into())));
        assert!(matches!(
            FirstExit::detector(result),
            FirstExit::Worker(WorkerExit::DangerDetector(WorkerStop::Source(_)))
        ));
    }
}
