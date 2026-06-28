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
use crate::ingress::inclusion_lane::{
    InclusionLane, InclusionLaneConfig, InclusionLaneError, dump_info, dump_info::delete_dump_dir,
};
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
/// No genesis app instance: `setup` already registered the finalized genesis
/// snapshot, so the lane reloads via `A::from_dump`. The `domain` is built by
/// `run` from the pinned deployment identity.
pub(crate) struct WorkersConfig {
    pub run_config: RunConfig,
    pub l1_config: L1Config,
    pub timing: ProtocolTiming,
    pub input_reader: InputReader,
    pub domain: alloy_sol_types::Eip712Domain,
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
    pub(crate) async fn spawn<A: Application + Clone + Sync + 'static>(
        cfg: WorkersConfig,
    ) -> Result<Self, RunError> {
        let WorkersConfig {
            run_config,
            l1_config,
            timing,
            input_reader,
            domain,
        } = cfg;

        // Derived values — kept inside `spawn` so `WorkersConfig` stays
        // minimal and these aren't computed twice in the caller.
        let db_path = run_config.db_path();
        let input_reader_genesis_block = input_reader.genesis_block();

        let shutdown = ShutdownSignal::default();

        // Inclusion lane: takes the app, returns the tx-sender the HTTP
        // ingress route will publish to.
        let mut storage = crate::storage::Storage::open(&db_path)?;
        let dumps_dir = std::path::Path::new(&run_config.data_dir).join("dumps");
        std::fs::create_dir_all(&dumps_dir)?;

        // Structural startup, in this order:
        //
        // 1. Reset stale leases. A crashed previous run may have left
        //    `lease_count > 0` on dumps that aren't being read by
        //    anyone now; without this, GC would skip them forever.
        // 2. Require the finalized snapshot (always-load invariant). `setup`
        //    registered the genesis snapshot and `run` gated on the
        //    `setup_complete` marker, so it must be present — a missing one
        //    is a terminal incomplete-setup, not a cold-start to paper over
        //    (run holds no genesis app instance).
        // 3. Ensure an open Tip exists (tip-existence invariant). Opens the
        //    genesis Tip on a fresh DB, no-op otherwise. The lane loads the
        //    head itself after catch-up; this step only establishes the
        //    invariant. Runs after preemptive recovery has synced the safe
        //    head, so the genesis frame's `safe_block` dates to startup (full
        //    landing budget), not to an earlier, possibly stale view. (This
        //    is a B-time quantity — it stays in `run`, never in `setup`.)
        // 4. GC SQLite-side: drop any rows now unreferenced after
        //    promotions or invalidations that finalized just before
        //    the previous shutdown.
        // 5. Orphan FS sweep: remove directories under `dumps_dir`
        //    that aren't tracked by SQLite (crash-during-create_dump
        //    or crash-during-GC-after-row-delete artifacts).
        storage.reset_dump_leases()?;
        require_finalized_snapshot(&mut storage)?;
        restamp_finalized_promotion(&mut storage)?;
        storage.ensure_open_tip()?;
        let gc_removed = snapshot_gc_at_startup::<A>(&mut storage)?;
        let sweep_removed = sweep_orphan_dumps::<A>(&mut storage, &dumps_dir)?;
        tracing::debug!(
            gc_removed,
            sweep_removed,
            "snapshot startup cleanup complete",
        );

        let (tx, lane) = InclusionLane::<A>::start(
            QUEUE_CAPACITY,
            shutdown.clone(),
            storage,
            InclusionLaneConfig::new(l1_config.batch_submitter_address, dumps_dir)
                .with_max_batch_open(run_config.max_batch_open()),
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
            seconds_per_block: run_config.timing.seconds_per_block,
            long_block_range_error_codes: run_config.long_block_range_error_codes.clone(),
            expected_chain_id: l1_config.chain_id,
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
        let components: [(WorkerId, ComponentShutdown); 5] = [
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

        // Two completion modes:
        // - Worker-failure: we already have the primary; await the OTHER
        //   components for orderly cleanup, log any cleanup errors, surface
        //   the primary (wrapped to RunError).
        // - Signal-driven shutdown: an OS signal triggered shutdown. Wait for
        //   everything to drain; the signal handler's own error (if any)
        //   takes priority over any subsequent component shutdown error.
        let (worker_failure, signal_error): (Option<(WorkerId, WorkerExit)>, Option<RunError>) =
            match first_exit {
                FirstExit::Signal(err) => (None, err),
                FirstExit::Worker(exit) => {
                    let id = exit.worker_id();
                    (Some((id, exit)), None)
                }
            };

        if let Some((failed, primary_exit)) = worker_failure {
            for (id, fut) in components {
                if id == failed {
                    // Drop the primary's future without awaiting — its task
                    // is already done (it's what tripped the select), and
                    // we'll surface its error directly below.
                    drop(fut);
                    continue;
                }
                log_cleanup_result(id.label(), fut.await);
            }
            return Err(RunError::Worker(primary_exit));
        }

        // Signal path: await EVERY component so each worker's JoinHandle is
        // joined and its task fully drains. A `break` here would drop the
        // remaining components' futures un-awaited, which DETACHES those tasks
        // (only `JoinHandle::abort()` cancels a dropped handle) — they'd be
        // killed mid-drain at runtime teardown, the exact abrupt-write case the
        // startup snapshot hygiene (sweep/gc/re-stamp) exists to clean up after.
        // Keep the first error to surface; log every error (the signal handler's
        // own error, if any, still takes priority below).
        let mut shutdown_error: Option<WorkerExit> = None;
        for (id, fut) in components {
            if let Err(e) = fut.await {
                warn!(component = id.label(), error = %e, "component errored during signal-driven shutdown");
                if shutdown_error.is_none() {
                    shutdown_error = Some(e);
                }
            }
        }
        match (signal_error, shutdown_error) {
            (Some(err), _) => Err(err),
            (None, Some(exit)) => Err(RunError::Worker(exit)),
            (None, None) => Ok(()),
        }
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
            WorkerExit::DangerDetector(_) => WorkerId::DangerDetector,
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

// Built once at worker spawn (sync, raw `create_signer_provider`). The submitter
// is long-lived, so a one-shot spawn-time chain-id check would go stale; the
// keyed-write guard instead lives in `EthereumBatchPoster::submit_batches`,
// which re-confirms the chain id immediately before every productive send.
fn build_batch_submitter_provider(l1: &L1Config) -> Result<DynProvider, std::io::Error> {
    crate::l1::provider::create_signer_provider(&l1.eth_rpc_url, &l1.batch_submitter_private_key)
        .map_err(std::io::Error::other)
}

/// Require the finalized snapshot the lane will `from_dump` against. `setup`
/// registers the genesis snapshot and `run` gates on the `setup_complete`
/// marker, so by the time the lane starts the snapshot must exist. A missing
/// one means the DB's setup is incomplete/corrupt — terminal
/// `SetupNotComplete` (re-run `setup`), not a cold-start to silently heal.
fn require_finalized_snapshot(storage: &mut crate::storage::Storage) -> Result<(), RunError> {
    if storage.finalized_dump()?.is_none() {
        return Err(RunError::Bootstrap(
            crate::runtime::error::BootstrapError::SetupNotComplete,
        ));
    }
    Ok(())
}

/// Re-stamp `B` into the finalized dump's `info.toml` from the
/// authoritative DB row. Idempotent; closes the crash window between a
/// promotion's commit and the lane's in-place stamp.
fn restamp_finalized_promotion(storage: &mut crate::storage::Storage) -> Result<(), RunError> {
    if let Some(finalized) = storage.finalized_dump()? {
        dump_info::stamp_promoted_inclusion_block(
            &finalized.dump.prefix,
            finalized.inclusion_block,
        )?;
    }
    Ok(())
}

/// Drop any dump rows that are now unreferenced (no pending, no
/// finalized, no leases). The companion `sweep_orphan_dumps` then
/// catches anything on disk that this leaves behind, plus
/// crash-during-create_dump orphans the SQLite layer never saw.
fn snapshot_gc_at_startup<A: Application + 'static>(
    storage: &mut crate::storage::Storage,
) -> Result<usize, RunError> {
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
) -> Result<usize, RunError> {
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

    // ── Snapshot startup hygiene ────────────────────────────────────
    //
    // `sweep_orphan_dumps` removes filesystem entries under `dumps_dir`
    // that aren't tracked in SQLite. Catches crash-during-create_dump
    // (file exists, no row) and crash-during-GC (row deleted, file
    // remains). Must NOT touch directories that ARE in `dumps` —
    // those are the genesis dump, finalized, and any pending the
    // lane is still working with.

    use crate::runtime::test_support::{SweepTestApp, create_structured_dump};
    use crate::storage::Storage;
    use crate::storage::test_helpers::temp_db;

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
            FirstExit::from(result),
            FirstExit::Worker(WorkerExit::DangerDetector(DangerDetectorExit::Source(_)))
        ));
    }
}
