// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Command error taxonomy: [`BootstrapError`] and [`IdentityError`] before workers
//! start, [`WorkerExit`] for runtime worker exits, and [`CommandError`] on top.

use thiserror::Error;

use crate::commands::config::key_file_io_is_terminal;
use crate::ingress::inclusion_lane::{
    InclusionLaneError, dump_info::referenced_artifact_io_is_terminal,
};
use crate::l1::fee_oracle::worker::FeeOracleError;
use crate::l1::reader::InputReaderError;
use crate::l1::submitter::BatchSubmitterError;
use crate::recovery::{DangerDetectorError, RecoveryError};
use crate::storage::{
    DangerStatus, DeploymentIdentity, LifecycleError, StorageOpenError,
    is_persistent_storage_error, is_persistent_storage_open_error,
};
use sequencer_core::application::AppError;
use sequencer_core::protocol::ProtocolTimingError;

// ── Top-level CommandError ────────────────────────────────────────────────

/// Top-level command error. Grouped by phase:
///
/// - `Bootstrap`: startup failures before runtime workers come up.
/// - `Worker`: one of the runtime workers exited (server, inclusion lane,
///   input reader, batch submitter, danger detector, fee oracle).
/// - `Io` / `Storage`: generic catch-alls used widely; not worth nesting.
#[derive(Debug, Error)]
pub enum CommandError {
    #[error("bootstrap failed: {0}")]
    Bootstrap(#[from] BootstrapError),
    #[error("worker exited: {0}")]
    Worker(#[from] WorkerExit),
    #[error("DB-referenced snapshot artifact {path:?} failed: {source}")]
    ReferencedSnapshotArtifact {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("storage operation failed: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error("application bootstrap failed: {0}")]
    AppBootstrap(#[from] AppError),
}

// ── Exit-code projection ────────────────────────────────────────────────
//
// README "Running" owns the exit-code contract; `CommandFailureVerdict` is the
// one classification it projects. Also reserved: 2 (clap usage) and 101 (a
// panic before the command harness). Diagnosed terminal runtime faults bypass
// this projection with `abort()` (SIGABRT, 134).

/// Restart; a recovery boot follows and may take 15+ min (flush plus
/// safe-finality wait), so startup probes must allow for it.
pub const EXIT_RESTART_EXPECT_RECOVERY: u8 = 10;
/// Restart with backoff; the refusal clears when the L1 view freshens.
pub const EXIT_RESTART_TRANSIENT: u8 = 20;
/// Do not restart; page an operator.
pub const EXIT_TERMINAL: u8 = 30;
/// Do not restart: setup's escape check found the old instance's work past
/// the stop block. Stop the old instance, discard the data directory and run
/// setup again.
pub const EXIT_SETUP_ESCAPE: u8 = 40;
/// Unclassified operational failure; restart with backoff.
pub const EXIT_UNCLASSIFIED: u8 = 1;

/// Semantic disposition of a failed command.
///
/// This is the single classification boundary shared by the terminal-fault
/// black box and the process exit-code projection: only [`Self::Terminal`]
/// best-effort records a cause (and exits 30: do not restart, page).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandFailureVerdict {
    ExpectedRecovery,
    Retryable,
    Terminal,
    SetupEscape,
    Unclassified,
}

impl CommandFailureVerdict {
    /// Whether the black box records a terminal cause (and the process
    /// exits 30: do not restart, page an operator).
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }

    /// Exit-line severity: ERROR when the cause is a fault or unknown, WARN
    /// when the verdict names a planned procedure (a recovery boot, a retry,
    /// a setup refusal). Alert on the exit code, not the level.
    pub(crate) const fn logs_at_error(self) -> bool {
        matches!(self, Self::Terminal | Self::Unclassified)
    }

    /// The supervisor action this verdict asks for, named on the exit line.
    pub(crate) const fn supervisor_action(self) -> &'static str {
        match self {
            Self::ExpectedRecovery => "restart; a recovery boot follows",
            Self::Retryable => "restart with backoff; alert if it persists",
            Self::Terminal => "do not restart; page an operator",
            Self::SetupEscape => {
                "do not restart; stop the old instance, discard this data directory, run setup again"
            }
            Self::Unclassified => "restart with backoff",
        }
    }

    const fn exit_code(self) -> u8 {
        match self {
            Self::ExpectedRecovery => EXIT_RESTART_EXPECT_RECOVERY,
            Self::Retryable => EXIT_RESTART_TRANSIENT,
            Self::Terminal => EXIT_TERMINAL,
            Self::SetupEscape => EXIT_SETUP_ESCAPE,
            Self::Unclassified => EXIT_UNCLASSIFIED,
        }
    }
}

impl CommandError {
    /// Classify this failure once for both the black-box settlement write and the
    /// supervisor-facing projection.
    pub(crate) fn failure_verdict(&self) -> CommandFailureVerdict {
        match self {
            CommandError::Worker(WorkerExit::DangerDetected { status }) => {
                danger_failure_verdict(status)
            }
            // Admission-fact refusals are terminal: the wrong command for
            // this database, the absorbing divergence, a malformed black
            // box. A plain storage failure underneath a lifecycle operation
            // classifies by persistence like every other storage error — a
            // transient SQLITE_BUSY on a black-box write must not page.
            CommandError::Lifecycle(
                LifecycleError::NotAdmissible { .. }
                | LifecycleError::CanonicalDivergence { .. }
                | LifecycleError::Malformed(_),
            ) => CommandFailureVerdict::Terminal,
            CommandError::Lifecycle(LifecycleError::Storage(source))
                if is_persistent_storage_error(source) =>
            {
                CommandFailureVerdict::Terminal
            }
            CommandError::Lifecycle(LifecycleError::Storage(_)) => {
                CommandFailureVerdict::Unclassified
            }
            CommandError::Storage(source) if is_persistent_storage_error(source) => {
                CommandFailureVerdict::Terminal
            }
            CommandError::AppBootstrap(AppError::Internal { .. }) => {
                CommandFailureVerdict::Terminal
            }
            CommandError::AppBootstrap(AppError::Io(source))
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::InvalidData
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::IsADirectory
                        | std::io::ErrorKind::NotADirectory
                ) =>
            {
                // An absent option or a missing or corrupt genesis dump needs
                // operator repair. PermissionDenied stays operational, as for
                // referenced artifacts (`referenced_artifact_io_is_terminal`):
                // it may clear once permissions are fixed, and the C engine
                // seam carries no kind for it (IO_ERROR becomes Other), so
                // native and C engines agree on exit 1. The key file differs
                // (`key_file_io_is_terminal`) because it never crosses the seam.
                CommandFailureVerdict::Terminal
            }
            CommandError::ReferencedSnapshotArtifact { source, .. }
                if referenced_artifact_io_is_terminal(source) =>
            {
                CommandFailureVerdict::Terminal
            }
            CommandError::Worker(exit) if exit.is_terminal() => CommandFailureVerdict::Terminal,
            CommandError::Bootstrap(error) => bootstrap_failure_verdict(error),
            // Worker crashes, provider errors, IO/storage/app catch-alls.
            CommandError::Worker(_)
            | CommandError::ReferencedSnapshotArtifact { .. }
            | CommandError::Io(_)
            | CommandError::Storage(_)
            | CommandError::AppBootstrap(_) => CommandFailureVerdict::Unclassified,
        }
    }

    /// Project this error onto the exit-code contract. Clean shutdown
    /// (exit 0) is handled by the caller — a `CommandError` is always a failure.
    pub fn exit_code(&self) -> u8 {
        self.failure_verdict().exit_code()
    }
}

// Per-worker terminality lives as `is_terminal_invariant` on each worker's
// own error type, beside its enum; `WorkerExit::is_terminal` below
// composes them, and takes danger exits from `danger_failure_verdict`.

fn danger_failure_verdict(status: &DangerStatus) -> CommandFailureVerdict {
    match status {
        // A doomed closed batch / aging Tip: the next boot legitimately runs
        // a slow recovery (flush + cascade).
        DangerStatus::ClosedBatchInDanger(_) | DangerStatus::TipInDanger(_) => {
            CommandFailureVerdict::ExpectedRecovery
        }
        // View-dependent refusals that self-heal once the provider recovers.
        DangerStatus::L1ViewStale | DangerStatus::EstimatedBatchInDanger(_) => {
            CommandFailureVerdict::Retryable
        }
        // The only genuinely terminal danger: canonical divergence.
        DangerStatus::CanonicalDivergence(_) => CommandFailureVerdict::Terminal,
        // `Safe` is never a detector exit. If it ever reaches here the danger
        // classification is self-contradicting — page an operator (EXIT_TERMINAL)
        // rather than silently restart-loop with backoff (EXIT_UNCLASSIFIED).
        DangerStatus::Safe => {
            debug_assert!(
                false,
                "danger_failure_verdict called with DangerStatus::Safe"
            );
            CommandFailureVerdict::Terminal
        }
    }
}

fn bootstrap_failure_verdict(err: &BootstrapError) -> CommandFailureVerdict {
    match err {
        BootstrapError::OpenStorage(source) if is_persistent_storage_open_error(source) => {
            CommandFailureVerdict::Terminal
        }
        BootstrapError::KeyFile { source, .. } if key_file_io_is_terminal(source) => {
            CommandFailureVerdict::Terminal
        }
        BootstrapError::Flush(source) if source.is_terminal_invariant() => {
            CommandFailureVerdict::Terminal
        }
        // The recovery controller owns classification. No raw provider,
        // storage, flush, or phase error crosses this boundary.
        BootstrapError::Recovery(source) => {
            if source.is_retryable() {
                CommandFailureVerdict::Retryable
            } else {
                CommandFailureVerdict::Terminal
            }
        }
        // Setup's reader failures take the startup polarity.
        BootstrapError::SetupInputReader(source) => {
            if source.is_retryable_at_startup() {
                CommandFailureVerdict::Retryable
            } else {
                CommandFailureVerdict::Terminal
            }
        }
        // Transient: self-heal when the L1 view / provider recovers — or,
        // for the data-dir lock, when the previous owner finishes dying.
        // `FeeOracleTransient` is documented "may self-heal", which is this
        // class's definition.
        BootstrapError::ChainIdRpc { .. }
        | BootstrapError::Identity(
            IdentityError::FirstBootRequiresL1 | IdentityError::DeploymentNotYetSafe { .. },
        )
        | BootstrapError::WalletNonceRead { .. }
        | BootstrapError::DataDirLocked { .. }
        | BootstrapError::FeeOracleTransient { .. }
        | BootstrapError::Flush(_) => CommandFailureVerdict::Retryable,

        // Terminal: needs an operator (wrong config, divergence, or a DB that
        // was never set up).
        BootstrapError::ChainIdMismatch { .. }
        | BootstrapError::RpcRedirect { .. }
        | BootstrapError::InvalidProtocolTiming(_)
        | BootstrapError::BatchOpenPastDanger { .. }
        | BootstrapError::FeeOracleMisconfig { .. }
        | BootstrapError::FeeOracleFatal { .. }
        | BootstrapError::SignerMisconfig { .. }
        | BootstrapError::SetupNotComplete { .. }
        | BootstrapError::Setup(_)
        | BootstrapError::Identity(IdentityError::Mismatch { .. }) => {
            CommandFailureVerdict::Terminal
        }

        BootstrapError::SetupRefuse(_) => CommandFailureVerdict::SetupEscape,
        BootstrapError::OpenStorage(_) | BootstrapError::KeyFile { .. } => {
            CommandFailureVerdict::Unclassified
        }
    }
}

// ── Bootstrap-phase errors ─────────────────────────────────────────────

/// Anything that can go wrong before runtime workers start: config validation,
/// deployment-identity guards, startup recovery, initial DB open.
#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error("RPC chain ID {rpc} does not match the expected chain ID {config}")]
    ChainIdMismatch { rpc: u64, config: u64 },
    /// `eth_chainId` failed. It is setup's first L1 read, so an unreachable
    /// RPC on first boot lands here. The command fails rather than
    /// warn-and-continue: proceeding with an unverified chain id
    /// would pin a possibly-wrong deployment identity and poison subsequent
    /// L1-unreachable boots, in addition to issuing soft confirmations
    /// against the wrong chain's state. Operator should retry.
    #[error("could not query chain ID from RPC: {message}")]
    ChainIdRpc { message: String },
    /// `eth_chainId` answered with a redirect. The client follows none
    /// (`l1/provider.rs`), so every retry gets the same answer until the
    /// operator configures the provider's final URL.
    #[error(
        "the RPC endpoint answered eth_chainId with HTTP {status}; redirects are not \
         followed, so configure the provider's final URL"
    )]
    RpcRedirect { status: u16 },
    /// Protocol-level config (`preemptive_margin_blocks` vs `max_wait_blocks`,
    /// `l1_read_stale_after_blocks` vs `danger_threshold`) failed validation.
    /// See [`ProtocolTimingError`].
    #[error(transparent)]
    InvalidProtocolTiming(#[from] ProtocolTimingError),
    /// `max_batch_open_seconds`, in blocks, leaves less than the submission
    /// margin below the danger threshold, so a low-volume Tip would reach
    /// danger before it can be sealed, submitted and safe-accepted.
    #[error(
        "max_batch_open_seconds ({max_batch_open_seconds}) spans {open_blocks} blocks at \
         seconds_per_block ({seconds_per_block}); it must be at most danger_threshold \
         ({danger_threshold}) minus the {margin_blocks}-block submission margin"
    )]
    BatchOpenPastDanger {
        max_batch_open_seconds: u64,
        seconds_per_block: u64,
        open_blocks: u64,
        danger_threshold: u64,
        margin_blocks: u64,
    },
    /// Setup-pinned fee oracle configuration or validation is invalid.
    #[error("fee oracle misconfiguration: {message}")]
    FeeOracleMisconfig { message: String },
    /// A live quote/transport failed while bootstrapping. It may self-heal.
    #[error("fee oracle bootstrap transient failure: {message}")]
    FeeOracleTransient { message: String },
    /// Trusted-code join failures while setup persists the first quote. These
    /// need operator attention but are not source misconfiguration.
    #[error("fatal fee oracle bootstrap failure: {message}")]
    FeeOracleFatal { message: String },
    /// The keyed signer provider could not be built (bad RPC URL or private key).
    /// Deterministic operator misconfiguration: a rerun fails identically, so it
    /// is terminal in every command, like [`Self::ChainIdMismatch`].
    #[error("signer provider misconfiguration: {message}")]
    SignerMisconfig { message: String },
    /// The batch-submitter key file could not be read. Classified by kind
    /// (`config::key_file_io_is_terminal`): a missing, unreadable, or
    /// malformed file is deterministic operator misconfiguration — terminal,
    /// like [`Self::SignerMisconfig`] for bad key *content* — while an
    /// environmental I/O failure stays operational. The message names the
    /// path and never the contents.
    #[error("batch-submitter key file {path:?} could not be read: {source}")]
    KeyFile {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Startup recovery (or refusal) failed before runtime workers started.
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
    /// An input-reader failure in setup's L1 discovery (the chain-id check's
    /// client, the safe-head sample, the reader) or initial sync, classified by
    /// [`InputReaderError::is_retryable_at_startup`].
    #[error("setup L1 input reader: {0}")]
    SetupInputReader(InputReaderError),
    /// Deployment-identity guards — see [`IdentityError`].
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// `run` (or `flush-mempool`) was invoked against a DB where `setup`
    /// has not completed — its era is absent (setup never ran, or
    /// crashed midway), or its outputs are incomplete. The
    /// operator must run `setup` first; restarting `run` cannot self-heal.
    #[error("setup has not completed for data dir {data_dir} — run `setup` first")]
    SetupNotComplete { data_dir: String },
    /// Another live process holds the exclusive data-directory lock
    /// (`process_lock`). Retry-safe: the lock is what prevents concurrent
    /// harm, and an orchestrated restart racing the previous owner's drain
    /// resolves on its own. Two replicas pointed at one data dir show up as
    /// this error repeating.
    #[error(
        "another process holds the data-directory lock ({path}); \
         refusing to run concurrently"
    )]
    DataDirLocked { path: String },
    /// The `flush-mempool` subcommand's flush failed (provider/transport).
    #[error("mempool flush failed: {0}")]
    Flush(#[from] crate::l1::flusher::FlushError),
    /// `setup` could not read the batch-submitter wallet nonce for its escape
    /// check. The sync before it proved L1 reachable, so this is a hiccup; but
    /// the data directory already holds the sync, so the operator discards it
    /// and runs setup again.
    #[error("setup could not read the submitter wallet nonce from RPC: {message}")]
    WalletNonceRead { message: String },
    /// `setup`'s escape check found the old instance's work past the stop
    /// block. A restart cannot clear it.
    #[error(transparent)]
    SetupRefuse(#[from] SetupRefuse),
    /// `setup` refused its directory, its base, or the history it rebuilt.
    #[error(transparent)]
    Setup(Box<SetupError>),
}

/// `setup`'s escape check, after the flush and the sync through the stop
/// block: either finding means a transaction escaped the flush or the old
/// instance still runs, so the history setup would publish may be wrong. Exit
/// [`EXIT_SETUP_ESCAPE`]: stop the old instance and run setup again in an
/// empty directory.
#[derive(Debug, Error)]
pub enum SetupRefuse {
    /// The submitter's wallet nonce is not settled at the synced safe block
    /// (`pending > safe`) on the local provider. Local view only: a zombie
    /// transaction dropped from this provider's pool evades it.
    #[error(
        "the submitter's wallet is still unsettled after the flush (pending nonce {pending} > \
         safe nonce {safe}); stop the old instance, discard this data directory, run setup again"
    )]
    WalletUnsettledAfterFlush { pending: u64, safe: u64 },
    /// A submitter input landed past the stop block.
    #[error(
        "a submitter input (index {safe_input_index}, block {found_block}) landed past the stop \
         block {stop_block}; stop the old instance, discard this data directory, run setup again"
    )]
    SubmitterInputPastStop {
        stop_block: u64,
        found_block: u64,
        safe_input_index: u64,
    },
}

/// Why `setup` refuses, each terminal: the operator fixes the cause and runs
/// setup again in an empty directory.
#[derive(Debug, Error)]
pub enum SetupError {
    /// Setup starts only in an empty directory: nothing already there can be
    /// trusted to fit this run, a completed setup or a failed one.
    #[error(
        "data directory {path} is not empty (it holds {entry}); setup runs only in an empty \
         directory"
    )]
    DataDirNotEmpty { path: String, entry: String },
    #[error("cannot read the base machine at {path}: {source}")]
    BaseMachine {
        path: std::path::PathBuf,
        #[source]
        source: crate::machine::MachineExportError,
    },
    #[error("cannot build the application from the base machine's state at {path}: {source}")]
    BaseApplication {
        path: std::path::PathBuf,
        #[source]
        source: AppError,
    },
    /// The base consumed inputs past the stop block, so its state is not at
    /// or below the era's.
    #[error(
        "the base consumed {l1_input_count} L1 inputs, but only {inputs_through_stop} lie at or \
         below the stop block {stop_block}"
    )]
    BaseAheadOfStop {
        l1_input_count: u64,
        inputs_through_stop: u64,
        stop_block: u64,
    },
    /// The export's queued directs are not the admitted directs above its
    /// clock: the machine is not canonical, or its scheduler drained wrongly.
    #[error(
        "the base's export disagrees with L1: among its {l1_input_count} inputs, {admitted} \
         admitted directs lie above its clock {clock}, the first at {first_admitted:?}, but it \
         queues {fridge_len} from input {first_queued_input}"
    )]
    ExportDisagrees {
        l1_input_count: u64,
        clock: u64,
        fridge_len: u64,
        first_queued_input: u64,
        admitted: u64,
        first_admitted: Option<u64>,
    },
    /// The scheduler refused the export with its queued range from L1.
    #[error("the base's export disagrees with L1: {0}")]
    ResumeRefused(#[from] sequencer_core::scheduler::ResumeError),
    #[error("the rebuilt scheduler's clock {clock} is past the stop block {stop_block}")]
    ClockPastStop { clock: u64, stop_block: u64 },
    /// The safe head the sync reached is below the stop block the flush saw:
    /// a provider behind its own earlier answer.
    #[error(
        "the sync reached safe block {synced_safe_block}, below the stop block {stop_block}; \
         discard this data directory and run setup again"
    )]
    SyncBehindStopBlock {
        synced_safe_block: u64,
        stop_block: u64,
    },
    /// The application failed on a rebuilt transaction: the `position`-th
    /// the scheduler handed out for input `l1_input_index`, skipped ones
    /// included. Replaying a trusted machine to that input says whether the
    /// application is dead on chain.
    #[error(
        "the application failed on the rebuilt transaction at input {l1_input_index}, \
         position {position} ({transaction}): {source}"
    )]
    HistoryExecution {
        l1_input_index: u64,
        position: u64,
        /// A direct's input index, or a user op's sender and nonce.
        transaction: String,
        #[source]
        source: AppError,
    },
}

/// Deployment-identity failure modes. The sequencer pins itself to a specific
/// (chain_id, app_address, input_box_address, app_deployment_block,
/// batch_submitter_address, fee_oracle) tuple on first successful boot, then
/// refuses to run under a different identity to prevent silently associating
/// state from one deployment with another.
#[derive(Debug, Error)]
pub enum IdentityError {
    /// An L1 read after the chain-id check (the safe-head sample or discovery)
    /// failed AND there is no cached identity in the DB. We need at least one
    /// (live L1 query OR a prior boot's pinned identity) to safely bind this
    /// sequencer to a deployment. Operator: bring up L1 and retry.
    #[error("first boot requires L1: no cached deployment identity and L1 is unreachable")]
    FirstBootRequiresL1,
    /// The application's deployment block is above the L1 safe head sampled
    /// before discovery read it, so a reorg could still move it. Setup pins
    /// only a safe deployment. Operator: retry once the safe head reaches it.
    #[error(
        "application deployment block {app_deployment_block} is above the L1 safe head \
         {safe_block} sampled before discovery; setup pins only a safe deployment, retry once \
         the safe head reaches it"
    )]
    DeploymentNotYetSafe {
        app_deployment_block: u64,
        safe_block: u64,
    },
    /// The pinned identity doesn't match the current config.
    ///
    /// `stored` and `expected` are boxed to keep `CommandError` under clippy's
    /// `result_large_err` limit; the allocation is on the cold error path.
    #[error("deployment identity mismatch ({fields}); stored={stored:?}; expected={expected:?}")]
    Mismatch {
        fields: String,
        stored: Box<DeploymentIdentity>,
        expected: Box<DeploymentIdentity>,
    },
}

// ── Worker exits ───────────────────────────────────────────────────────

/// Which runtime worker exited, and why: one generic stop shape per worker, plus
/// the danger detector's deliberate `RecoveryRequired` trip, which is not an error
/// but exits the runtime so the orchestrator respawns into startup recovery.
#[derive(Debug, Error)]
pub enum WorkerExit {
    #[error("server: {0}")]
    Server(WorkerStop<std::io::Error>),
    #[error("inclusion lane: {0}")]
    Lane(WorkerStop<InclusionLaneError>),
    #[error("input reader: {0}")]
    InputReader(WorkerStop<InputReaderError>),
    #[error("batch submitter: {0}")]
    BatchSubmitter(WorkerStop<BatchSubmitterError>),
    #[error("danger detector: {0}")]
    DangerDetector(WorkerStop<DangerDetectorError>),
    #[error("fee oracle: {0}")]
    FeeOracle(WorkerStop<FeeOracleError>),
    #[error("danger detector: danger detected ({status:?}) — stopping for startup recovery")]
    DangerDetected { status: DangerStatus },
}

impl WorkerExit {
    /// Whether this exit poisons the run (terminal, exit 30) rather than
    /// restarting. Terminality is a method on each worker's own error type,
    /// beside its enum — so adding a variant fails to compile there, not
    /// silently classifying non-terminal through a distant wildcard.
    /// An outer join failure is terminal exactly when it was a panic:
    /// trusted sequencer/application code violated its execution contract
    /// (fail-loud self-trust policy).
    pub(crate) fn is_terminal(&self) -> bool {
        match self {
            // Listener/accept IO is environmental; restart.
            WorkerExit::Server(stop) => stop.is_terminal_with(|_| false),
            WorkerExit::Lane(stop) => {
                stop.is_terminal_with(InclusionLaneError::is_terminal_invariant)
            }
            WorkerExit::InputReader(stop) => {
                stop.is_terminal_with(InputReaderError::is_terminal_invariant)
            }
            WorkerExit::BatchSubmitter(stop) => {
                stop.is_terminal_with(BatchSubmitterError::is_terminal_invariant)
            }
            WorkerExit::DangerDetector(stop) => {
                stop.is_terminal_with(DangerDetectorError::is_terminal_invariant)
            }
            WorkerExit::FeeOracle(stop) => {
                stop.is_terminal_with(FeeOracleError::is_terminal_invariant)
            }
            // One home with the exit code, so the abort decision cannot drift.
            WorkerExit::DangerDetected { status } => danger_failure_verdict(status).is_terminal(),
        }
    }
}

/// Generic worker stop shape: the task ended without runtime shutdown, ended
/// with its typed error, or failed to join.
#[derive(Debug, Error)]
pub enum WorkerStop<E: std::error::Error> {
    #[error("stopped unexpectedly")]
    StoppedUnexpectedly,
    #[error("{0}")]
    Source(E),
    #[error("join error: {0}")]
    Join(tokio::task::JoinError),
}

impl<E: std::error::Error> WorkerStop<E> {
    /// Select-arm mapping: the runtime is live, so a clean `Ok(())` return
    /// means the worker stopped on its own — unexpected.
    pub(crate) fn from_select(result: Result<Result<(), E>, tokio::task::JoinError>) -> Self {
        match result {
            Ok(Ok(())) => Self::StoppedUnexpectedly,
            Ok(Err(source)) => Self::Source(source),
            Err(source) => Self::Join(source),
        }
    }

    /// Shutdown-path mapping: runtime-wide shutdown was already requested,
    /// so `Ok(())` is the expected graceful drain. Distinct from
    /// [`Self::from_select`], where the same `Ok(())` is unexpected.
    pub(crate) fn from_shutdown(
        result: Result<Result<(), E>, tokio::task::JoinError>,
    ) -> Result<(), Self> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(source)) => Err(Self::Source(source)),
            Err(source) => Err(Self::Join(source)),
        }
    }

    fn is_terminal_with(&self, source_is_terminal: impl FnOnce(&E) -> bool) -> bool {
        match self {
            Self::StoppedUnexpectedly => false,
            Self::Source(source) => source_is_terminal(source),
            Self::Join(source) => source.is_panic(),
        }
    }
}

// ── Chained `From` impls so `?` works at the top-level CommandError ────────
//
// thiserror's `#[from]` is one-level; nested propagation needs manual
// impls. Each leaf error type that can bubble up through `?` in `run()`
// gets a direct From<Leaf> for CommandError.

impl From<StorageOpenError> for CommandError {
    fn from(e: StorageOpenError) -> Self {
        CommandError::Bootstrap(e.into())
    }
}

/// The lock substrate keeps its own error type so `runtime/` never imports
/// this command-layer taxonomy; the classification (DataDirLocked is
/// retry-safe) lives here with its verdict.
impl From<crate::runtime::process_lock::ProcessLockError> for CommandError {
    fn from(e: crate::runtime::process_lock::ProcessLockError) -> Self {
        use crate::runtime::process_lock::ProcessLockError;
        match e {
            ProcessLockError::Locked { path } => {
                CommandError::Bootstrap(BootstrapError::DataDirLocked { path })
            }
            ProcessLockError::Io { path, source } => {
                let kind = source.kind();
                CommandError::Io(std::io::Error::new(
                    kind,
                    ProcessLockError::Io { path, source },
                ))
            }
        }
    }
}

/// One shared classification for the keyed signer-provider constructor, so
/// `setup`, `flush-mempool` and any future keyed command agree. Run recovery
/// keeps its own Retry/Refuse polarity map, but its terminal/transient split must
/// agree with this one.
impl From<crate::l1::provider::VerifiedSignerProviderError> for BootstrapError {
    fn from(e: crate::l1::provider::VerifiedSignerProviderError) -> Self {
        use crate::l1::provider::VerifiedSignerProviderError as E;
        match e {
            E::ChainIdMismatch { rpc, expected } => BootstrapError::ChainIdMismatch {
                rpc,
                config: expected,
            },
            E::ChainIdRpc(message) => BootstrapError::ChainIdRpc { message },
            E::Create(message) => BootstrapError::SignerMisconfig { message },
        }
    }
}

impl From<crate::l1::provider::VerifiedSignerProviderError> for CommandError {
    fn from(e: crate::l1::provider::VerifiedSignerProviderError) -> Self {
        CommandError::Bootstrap(e.into())
    }
}

impl From<ProtocolTimingError> for CommandError {
    fn from(e: ProtocolTimingError) -> Self {
        CommandError::Bootstrap(e.into())
    }
}

impl From<RecoveryError> for CommandError {
    fn from(e: RecoveryError) -> Self {
        CommandError::Bootstrap(e.into())
    }
}

impl From<IdentityError> for CommandError {
    fn from(e: IdentityError) -> Self {
        CommandError::Bootstrap(e.into())
    }
}

impl From<crate::l1::flusher::FlushError> for CommandError {
    fn from(e: crate::l1::flusher::FlushError) -> Self {
        CommandError::Bootstrap(BootstrapError::Flush(e))
    }
}

impl From<SetupRefuse> for CommandError {
    fn from(e: SetupRefuse) -> Self {
        CommandError::Bootstrap(BootstrapError::SetupRefuse(e))
    }
}

impl From<SetupError> for CommandError {
    fn from(e: SetupError) -> Self {
        CommandError::Bootstrap(BootstrapError::Setup(Box::new(e)))
    }
}

impl From<FeeOracleError> for CommandError {
    fn from(error: FeeOracleError) -> Self {
        match error {
            FeeOracleError::OpenStorage(error) => CommandError::from(error),
            FeeOracleError::Storage(error) => CommandError::Storage(error),
            FeeOracleError::Transient(message) => {
                CommandError::Bootstrap(BootstrapError::FeeOracleTransient { message })
            }
            // Named arm so the message isn't double-prefixed through the
            // variant's own Display ("fee-oracle misconfiguration: ...").
            FeeOracleError::Misconfig(message) => {
                CommandError::Bootstrap(BootstrapError::FeeOracleMisconfig { message })
            }
            FeeOracleError::Join(message) => {
                CommandError::Bootstrap(BootstrapError::FeeOracleFatal {
                    message: FeeOracleError::Join(message).to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l1::fee_oracle::worker::FeeOracleError;
    use crate::l1::flusher::FlushError;
    use crate::l1::provider::VerifiedSignerProviderError;
    use crate::l1::submitter::BatchPosterError;
    use crate::l1::watermark::WalletNonceWatermarkError;
    use crate::recovery::{
        RecoveryError, RecoveryFailure, RecoveryRefusalReason, RecoveryRetryReason,
    };
    use crate::storage::DeploymentIdentity;
    use sequencer_core::protocol::ProtocolTimingError;

    fn danger(status: DangerStatus) -> CommandError {
        CommandError::Worker(WorkerExit::DangerDetected { status })
    }

    fn dummy_identity() -> DeploymentIdentity {
        use alloy_primitives::Address;
        DeploymentIdentity {
            chain_id: 1,
            app_address: Address::repeat_byte(0x11),
            input_box_address: Address::repeat_byte(0x22),
            app_deployment_block: 0,
            batch_submitter_address: Address::repeat_byte(0x33),
            max_direct_payload_bytes: 1024,
            max_wait_blocks: 1200,
            fee_oracle: crate::storage::FeeOracleIdentity::Fixed { log_gas_price: 0 },
        }
    }

    fn sqlite(code: rusqlite::ffi::ErrorCode, extended_code: std::ffi::c_int) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code,
            },
            None,
        )
    }

    fn busy() -> rusqlite::Error {
        sqlite(rusqlite::ffi::ErrorCode::DatabaseBusy, 5)
    }

    fn persistent_watermark() -> WalletNonceWatermarkError {
        WalletNonceWatermarkError::Storage(rusqlite::Error::QueryReturnedNoRows)
    }

    fn key_file(source: std::io::Error) -> CommandError {
        CommandError::Bootstrap(BootstrapError::KeyFile {
            path: "/etc/sequencer/submitter.key".into(),
            source,
        })
    }

    fn recovery_retry(failure: impl Into<RecoveryFailure>) -> CommandError {
        CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::retry(failure)))
    }

    fn recovery_refuse(failure: impl Into<RecoveryFailure>) -> CommandError {
        CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::refuse(failure)))
    }

    fn input_reader_worker(error: InputReaderError) -> CommandError {
        CommandError::Worker(WorkerExit::InputReader(WorkerStop::Source(error)))
    }

    fn submitter_worker(error: BatchSubmitterError) -> CommandError {
        CommandError::Worker(WorkerExit::BatchSubmitter(WorkerStop::Source(error)))
    }

    fn detector_worker(error: DangerDetectorError) -> CommandError {
        CommandError::Worker(WorkerExit::DangerDetector(WorkerStop::Source(error)))
    }

    fn fee_oracle_worker(error: FeeOracleError) -> CommandError {
        CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::Source(error)))
    }

    /// One row of the exit-code table: a failure shape and the label the
    /// assertion prints when its projection moves.
    type Row = (CommandError, &'static str);

    /// Class 10: restart; the next boot legitimately runs a slow recovery.
    fn expect_recovery() -> Vec<Row> {
        vec![
            (
                danger(DangerStatus::ClosedBatchInDanger(
                    crate::storage::BatchId::from_raw(0),
                )),
                "a doomed closed batch",
            ),
            (
                danger(DangerStatus::TipInDanger(
                    crate::storage::BatchId::from_raw(3),
                )),
                "an aging Tip",
            ),
        ]
    }

    fn setup_reader(error: InputReaderError) -> CommandError {
        CommandError::Bootstrap(BootstrapError::SetupInputReader(error))
    }

    /// Class 20: restart; a transient refusal that self-heals when the L1
    /// view or the provider recovers.
    fn transient() -> Vec<Row> {
        vec![
            (
                setup_reader(InputReaderError::Provider("offline".into())),
                "an unreachable provider during setup's L1 sync",
            ),
            (
                setup_reader(InputReaderError::InconsistentL1Response("gap".into())),
                "an inconsistent L1 response during setup's L1 sync",
            ),
            (
                setup_reader(InputReaderError::Storage(busy())),
                "busy storage during setup's L1 sync",
            ),
            (
                danger(DangerStatus::L1ViewStale),
                "a stale L1 view at runtime",
            ),
            (
                danger(DangerStatus::EstimatedBatchInDanger(
                    crate::storage::BatchId::from_raw(2),
                )),
                "an estimated batch in danger",
            ),
            (
                recovery_retry(RecoveryRetryReason::L1ViewStale),
                "a stale L1 view at startup recovery",
            ),
            (
                recovery_retry(RecoveryRetryReason::ResyncBehindFlushView {
                    resynced_safe_block: 1,
                    flush_observed_safe_block: 2,
                }),
                "a resync behind the flush's observed view",
            ),
            (
                CommandError::Bootstrap(BootstrapError::Identity(
                    IdentityError::FirstBootRequiresL1,
                )),
                "a first boot without L1",
            ),
            (
                CommandError::Bootstrap(BootstrapError::Identity(
                    IdentityError::DeploymentNotYetSafe {
                        app_deployment_block: 10,
                        safe_block: 9,
                    },
                )),
                "a setup before the deployment is safe",
            ),
            (
                CommandError::Bootstrap(BootstrapError::WalletNonceRead {
                    message: "provider unavailable".into(),
                }),
                "the escape check's wallet nonce read failing",
            ),
            (
                CommandError::Bootstrap(BootstrapError::ChainIdRpc {
                    message: "provider unavailable".into(),
                }),
                "the chain-id read failing",
            ),
            (
                CommandError::from(FlushError::Provider("x".into())),
                "the flush's provider",
            ),
            // A flush failure surfaced via startup recovery lands in the same
            // class as the flush-mempool subcommand's `FlushError`.
            (
                recovery_retry(RecoveryFailure::Flush(FlushError::Provider("x".into()))),
                "the same flush provider failure under startup recovery",
            ),
            // `bootstrap_failure_verdict` classifies `Recovery` on its
            // retry/refuse wrapper alone; which wrapper a provider failure
            // gets is pinned where it is built (`classify_signer_provider`).
            (
                recovery_retry(RecoveryFailure::ProviderUnreachable("timeout".into())),
                "the recovery flush's signer provider unreachable",
            ),
            // Documented "may self-heal": the definition of the retryable class.
            (
                CommandError::from(FeeOracleError::Transient("RPC unavailable".into())),
                "a transient fee-oracle failure at bootstrap",
            ),
            (
                CommandError::from(FlushError::Watermark(WalletNonceWatermarkError::Storage(
                    busy(),
                ))),
                "operational watermark contention remains retryable",
            ),
            (
                recovery_retry(RecoveryFailure::OpenStorage(StorageOpenError::Sqlite(
                    busy(),
                ))),
                "a busy open during the startup sync remains restartable",
            ),
        ]
    }

    /// Class 30: do not restart; page. The state cannot self-heal, so the
    /// black box records the cause (`is_terminal`) for exactly these rows.
    fn terminal() -> Vec<Row> {
        let mut rows: Vec<Row> = vec![
            (
                setup_reader(InputReaderError::Bootstrap("invalid RPC URL".into())),
                "a malformed RPC URL or wrong InputBox at setup",
            ),
            (
                setup_reader(InputReaderError::ChainIdMismatch {
                    rpc: 1,
                    expected: 2,
                }),
                "a wrong-chain RPC at setup",
            ),
            (
                setup_reader(InputReaderError::Join("lost".into())),
                "a lost sync task at setup",
            ),
            (
                setup_reader(InputReaderError::Storage(
                    rusqlite::Error::QueryReturnedNoRows,
                )),
                "persistent storage failure during setup's L1 sync",
            ),
        ];
        // A key file that is missing, unreadable, not a file, or not text is
        // the same operator mistake as bad key content one call later, and
        // must not differ from it by 29 in the exit code.
        rows.extend(
            [
                std::io::ErrorKind::NotFound,
                std::io::ErrorKind::PermissionDenied,
                std::io::ErrorKind::InvalidData,
                std::io::ErrorKind::IsADirectory,
                std::io::ErrorKind::NotADirectory,
            ]
            .map(|kind| {
                (
                    key_file(std::io::Error::from(kind)),
                    "a deterministic misconfiguration of the key file",
                )
            }),
        );
        rows.extend(
            [
                std::io::ErrorKind::InvalidInput,
                std::io::ErrorKind::NotFound,
                std::io::ErrorKind::InvalidData,
                std::io::ErrorKind::UnexpectedEof,
                std::io::ErrorKind::IsADirectory,
                std::io::ErrorKind::NotADirectory,
            ]
            .map(|kind| {
                (
                    CommandError::AppBootstrap(AppError::Io(std::io::Error::from(kind))),
                    "a missing option or unusable application genesis dump",
                )
            }),
        );
        rows.extend([
            // A deterministic signer-construction misconfig (bad RPC URL or
            // private key) is terminal in every command, matching
            // ChainIdMismatch.
            (
                CommandError::from(VerifiedSignerProviderError::Create("bad key".into())),
                "a signer-construction misconfig",
            ),
            // Retry/refuse is the wrapper's, pinned at `classify_signer_provider`.
            (
                recovery_refuse(RecoveryFailure::SignerMisconfig("bad key".into())),
                "the same signer misconfig under startup recovery",
            ),
            (
                CommandError::ReferencedSnapshotArtifact {
                    path: "/durable/snapshot".into(),
                    source: std::io::Error::from(std::io::ErrorKind::NotFound),
                },
                "a missing DB-referenced snapshot cannot self-heal on restart",
            ),
            (
                CommandError::Storage(rusqlite::Error::QueryReturnedNoRows),
                "a mandatory durable row disappearing cannot self-heal on restart",
            ),
            (
                detector_worker(DangerDetectorError::Storage(rusqlite::Error::QueryReturnedNoRows)),
                "typed persistent storage errors retain terminal classification through workers",
            ),
            (
                fee_oracle_worker(FeeOracleError::Storage(rusqlite::Error::QueryReturnedNoRows)),
                "the newer fee-oracle worker retains persistent storage classification",
            ),
            (
                fee_oracle_worker(FeeOracleError::Join("blocking storage task panicked".into())),
                "a fee-oracle blocking-task panic is a trusted-code failure",
            ),
            (
                input_reader_worker(InputReaderError::StorageTaskPanicked {
                    operation: "reading corrupt state",
                }),
                "an input-reader storage-task panic",
            ),
            (
                submitter_worker(BatchSubmitterError::StorageTaskPanicked {
                    operation: "reading corrupt state",
                }),
                "a submitter storage-task panic",
            ),
            (
                detector_worker(DangerDetectorError::StorageTaskPanicked),
                "a detector storage-task panic",
            ),
            (
                submitter_worker(BatchSubmitterError::Poster(BatchPosterError::Watermark(
                    persistent_watermark(),
                ))),
                "persistent write-before-broadcast storage failure must not retry as a provider error",
            ),
            (
                CommandError::from(FlushError::Watermark(persistent_watermark())),
                "the flush's persistent watermark failure",
            ),
            (
                CommandError::from(FlushError::UnsignableTransaction("no signer".into())),
                "a flush no-op the submit provider cannot sign",
            ),
            (
                recovery_refuse(RecoveryFailure::Flush(FlushError::UnsignableTransaction(
                    "no signer".into(),
                ))),
                "the same unsignable no-op under startup recovery",
            ),
            (
                submitter_worker(BatchSubmitterError::Poster(
                    BatchPosterError::UnsignableTransaction("no signer".into()),
                )),
                "a batch transaction the submit provider cannot sign",
            ),
            (
                submitter_worker(BatchSubmitterError::Poster(
                    BatchPosterError::UndecodableOwnBatch {
                        input_index: alloy_primitives::U256::from(7),
                        transaction_hash: None,
                        reason: "x".into(),
                    },
                )),
                "an undecodable batch from our own submitter key",
            ),
            (
                recovery_refuse(RecoveryFailure::Flush(FlushError::Watermark(
                    persistent_watermark(),
                ))),
                "the same watermark failure under startup recovery",
            ),
            (
                danger(DangerStatus::CanonicalDivergence(0)),
                "canonical divergence at runtime",
            ),
            (
                recovery_refuse(RecoveryRefusalReason::CanonicalDivergence { nonce: 0 }),
                "canonical divergence at startup recovery",
            ),
            (
                CommandError::Bootstrap(BootstrapError::SetupNotComplete {
                    data_dir: "/data".to_string(),
                }),
                "a data directory whose setup never completed",
            ),
            (
                CommandError::Bootstrap(BootstrapError::ChainIdMismatch { rpc: 1, config: 2 }),
                "a chain-id mismatch at boot",
            ),
            (
                CommandError::Bootstrap(BootstrapError::RpcRedirect { status: 308 }),
                "an RPC endpoint that redirects setup's chain-id read",
            ),
            (
                CommandError::Bootstrap(BootstrapError::InvalidProtocolTiming(
                    ProtocolTimingError::MarginNotLessThanMaxWait {
                        margin: 1200,
                        max_wait: 1200,
                    },
                )),
                "invalid protocol timing",
            ),
            (
                CommandError::Bootstrap(BootstrapError::BatchOpenPastDanger {
                    max_batch_open_seconds: 9601,
                    seconds_per_block: 12,
                    open_blocks: 801,
                    danger_threshold: 900,
                    margin_blocks: 100,
                }),
                "a batch-open window past the danger budget",
            ),
            (
                CommandError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch {
                    fields: "chain_id".into(),
                    stored: Box::new(dummy_identity()),
                    expected: Box::new(dummy_identity()),
                })),
                "a pinned-identity mismatch",
            ),
            // Setup's refusals of its directory, base and rebuild are
            // terminal (30), never the escape class (40): a fresh setup in
            // another directory does not clear them.
            (
                CommandError::from(SetupError::DataDirNotEmpty {
                    path: "/data".into(),
                    entry: "sequencer.db".into(),
                }),
                "a setup in a data directory that holds anything",
            ),
            (
                CommandError::from(SetupError::BaseApplication {
                    path: "/machines/base".into(),
                    source: AppError::Internal {
                        reason: "count mismatch".into(),
                    },
                }),
                "an application that cannot be built from the base's state",
            ),
            (
                CommandError::from(SetupError::BaseAheadOfStop {
                    l1_input_count: 9,
                    inputs_through_stop: 8,
                    stop_block: 100,
                }),
                "a base past the stop block",
            ),
            (
                CommandError::from(SetupError::ExportDisagrees {
                    l1_input_count: 9,
                    clock: 40,
                    fridge_len: 2,
                    first_queued_input: 5,
                    admitted: 1,
                    first_admitted: Some(6),
                }),
                "an export that disagrees with L1",
            ),
            (
                CommandError::from(SetupError::ResumeRefused(
                    sequencer_core::scheduler::ResumeError::FirstQueuedNotADirect { index: 5 },
                )),
                "an export whose queued range the scheduler refuses",
            ),
            (
                CommandError::from(SetupError::ClockPastStop {
                    clock: 101,
                    stop_block: 100,
                }),
                "a rebuilt clock past the stop block",
            ),
            (
                CommandError::from(SetupError::SyncBehindStopBlock {
                    synced_safe_block: 99,
                    stop_block: 100,
                }),
                "a sync that fell behind the flush's stop block",
            ),
            (
                CommandError::from(SetupError::HistoryExecution {
                    l1_input_index: 7,
                    position: 2,
                    transaction: "direct input 4".to_owned(),
                    source: AppError::Io(std::io::Error::other("disk unavailable")),
                }),
                "an application failure on the rebuilt history",
            ),
            // Reader-level chain-id mismatch (warm-boot backstop) is terminal,
            // like the boot-time BootstrapError::ChainIdMismatch.
            (
                input_reader_worker(InputReaderError::ChainIdMismatch {
                    rpc: 1,
                    expected: 31337,
                }),
                "the reader's warm-boot chain-id backstop",
            ),
            // The same mismatch surfacing during a startup-recovery safe-head
            // sync (RecoveryError path) is terminal too — not the unclassified
            // Recovery catch-all (which would loop on the wrong chain).
            (
                recovery_refuse(RecoveryFailure::InputReader(InputReaderError::ChainIdMismatch {
                    rpc: 1,
                    expected: 31337,
                })),
                "the same mismatch during the startup sync",
            ),
            (
                recovery_refuse(RecoveryFailure::InputReader(
                    InputReaderError::StorageTaskPanicked {
                        operation: "startup sync",
                    },
                )),
                "a storage-task panic during the startup sync",
            ),
            (
                recovery_refuse(RecoveryFailure::OpenStorage(StorageOpenError::Sqlite(sqlite(
                    rusqlite::ffi::ErrorCode::NotADatabase,
                    26,
                )))),
                "a persistent open failure during the startup sync",
            ),
            (
                CommandError::from(FeeOracleError::Join("storage task panicked".into())),
                "a fee-oracle storage-task panic at bootstrap",
            ),
            (
                CommandError::from(FeeOracleError::Misconfig("wrong pair".into())),
                "a fee-oracle misconfig at bootstrap",
            ),
            (
                fee_oracle_worker(FeeOracleError::Misconfig("wrong pair".into())),
                "a fee-oracle misconfig in the worker",
            ),
            (
                CommandError::AppBootstrap(AppError::Internal {
                    reason: "application invariant failed".into(),
                }),
                "an application invariant failure at bootstrap",
            ),
            // Lifecycle classifies by variant, not wholesale: fact refusals
            // page; a busy black-box write (class 1 below) must not.
            (
                CommandError::Lifecycle(LifecycleError::Storage(
                    rusqlite::Error::QueryReturnedNoRows,
                )),
                "a persistent lifecycle storage failure pages",
            ),
            (
                CommandError::Lifecycle(LifecycleError::NotAdmissible {
                    requested: crate::storage::LifecycleCommand::Run,
                    reason: "setup has not completed for this data directory",
                }),
                "an admission-fact refusal pages",
            ),
            (
                CommandError::Lifecycle(LifecycleError::CanonicalDivergence { nonce: 7 }),
                "divergence pages",
            ),
        ]);
        rows
    }

    /// Class 40: the escape check. The old instance must be stopped before
    /// a setup in a fresh directory can succeed, so it gets a dedicated code,
    /// distinct from the auto-recovery class (10).
    fn setup_escape() -> Vec<Row> {
        vec![
            (
                CommandError::from(SetupRefuse::WalletUnsettledAfterFlush {
                    pending: 14,
                    safe: 13,
                }),
                "a wallet still unsettled after the flush",
            ),
            (
                CommandError::from(SetupRefuse::SubmitterInputPastStop {
                    stop_block: 100,
                    found_block: 250,
                    safe_input_index: 7,
                }),
                "a submitter input past the stop block",
            ),
        ]
    }

    /// Class 1: unclassified operational failure; restart with backoff.
    fn unclassified() -> Vec<Row> {
        vec![
            (
                CommandError::Io(std::io::Error::other("boom")),
                "an operational I/O failure",
            ),
            (
                CommandError::from(crate::runtime::process_lock::ProcessLockError::Io {
                    path: "/data/sequencer.lock".to_string(),
                    source: std::io::Error::from_raw_os_error(13),
                }),
                "an I/O failure on the data-directory lock file",
            ),
            // An environmental read failure on the key file (a device or
            // memory error, not a wrong path) may clear on restart, so it must
            // not consume the do-not-restart code. EIO is the kind the OS
            // actually returns (decoded as `Uncategorized`); `other` is the
            // synthetic one.
            (
                key_file(std::io::Error::from_raw_os_error(5)),
                "an EIO device error on the key file may clear on restart",
            ),
            (
                key_file(std::io::Error::other("input/output error")),
                "a synthetic I/O error on the key file may clear on restart",
            ),
            (
                CommandError::ReferencedSnapshotArtifact {
                    path: "/durable/snapshot".into(),
                    source: std::io::Error::other("filesystem unavailable"),
                },
                "operational filesystem failures remain restartable",
            ),
            (
                CommandError::Storage(busy()),
                "transient storage contention remains restartable",
            ),
            (
                CommandError::Lifecycle(LifecycleError::Storage(busy())),
                "a transient lifecycle storage failure remains restartable",
            ),
            (
                CommandError::Worker(WorkerExit::Server(WorkerStop::StoppedUnexpectedly)),
                "a server that stopped unexpectedly",
            ),
            (
                fee_oracle_worker(FeeOracleError::Transient("RPC unavailable".into())),
                "a transient fee-oracle failure in the worker",
            ),
            (
                fee_oracle_worker(FeeOracleError::Storage(busy())),
                "fee-oracle storage contention remains restartable",
            ),
            (
                CommandError::AppBootstrap(AppError::Io(std::io::Error::other("disk unavailable"))),
                "an application I/O failure at bootstrap",
            ),
            (
                CommandError::AppBootstrap(AppError::Io(std::io::ErrorKind::Interrupted.into())),
                "an interrupted genesis read remains restartable",
            ),
            (
                CommandError::AppBootstrap(AppError::Io(std::io::ErrorKind::TimedOut.into())),
                "a timed out genesis read remains restartable",
            ),
            (
                CommandError::AppBootstrap(AppError::Io(
                    std::io::ErrorKind::PermissionDenied.into(),
                )),
                "a genesis dump the process may not read or write: may clear, and the C seam \
                 cannot carry the kind",
            ),
        ]
    }

    /// Every failure shape the suite pins, projected once onto the exit-code
    /// contract. Each row asserts its class and that the black-box verdict
    /// agrees with it (`is_terminal` exactly for class 30). The five classes
    /// carry distinct codes (pinned below), so a code pin is a verdict pin.
    #[test]
    fn every_pinned_failure_projects_to_its_class() {
        let table = [
            (EXIT_RESTART_EXPECT_RECOVERY, expect_recovery()),
            (EXIT_RESTART_TRANSIENT, transient()),
            (EXIT_TERMINAL, terminal()),
            (EXIT_SETUP_ESCAPE, setup_escape()),
            (EXIT_UNCLASSIFIED, unclassified()),
        ];
        for (code, rows) in table {
            assert!(!rows.is_empty(), "class {code} has no rows");
            for (error, why) in rows {
                assert_eq!(error.exit_code(), code, "{why}: {error:?}");
                assert_eq!(
                    error.failure_verdict().is_terminal(),
                    code == EXIT_TERMINAL,
                    "{why}: {error:?}"
                );
            }
        }
    }

    /// A detector exit aborts the run exactly when its exit code is terminal:
    /// only canonical divergence.
    #[test]
    fn danger_exit_aborts_only_on_canonical_divergence() {
        for status in [
            DangerStatus::ClosedBatchInDanger(crate::storage::BatchId::from_raw(1)),
            DangerStatus::TipInDanger(crate::storage::BatchId::from_raw(1)),
            DangerStatus::L1ViewStale,
            DangerStatus::EstimatedBatchInDanger(crate::storage::BatchId::from_raw(1)),
            DangerStatus::CanonicalDivergence(1),
        ] {
            let terminal = matches!(status, DangerStatus::CanonicalDivergence(_));
            assert_eq!(
                WorkerExit::DangerDetected { status }.is_terminal(),
                terminal,
                "{status:?}"
            );
        }
    }

    /// The exhaustive match makes a sixth verdict a compile error until its
    /// exit-line level is chosen here.
    #[test]
    fn exit_line_logs_at_error_only_for_terminal_and_unclassified() {
        use CommandFailureVerdict::*;
        let at_error = |verdict: CommandFailureVerdict| match verdict {
            Terminal | Unclassified => true,
            ExpectedRecovery | Retryable | SetupEscape => false,
        };
        let all = [
            ExpectedRecovery,
            Retryable,
            Terminal,
            SetupEscape,
            Unclassified,
        ];
        for verdict in all {
            assert_eq!(verdict.logs_at_error(), at_error(verdict), "{verdict:?}");
        }
        let actions: std::collections::BTreeSet<_> = all
            .iter()
            .map(|verdict| verdict.supervisor_action())
            .collect();
        assert_eq!(actions.len(), all.len());
    }

    /// The wire values a supervisor matches on, as integers: a renumbered
    /// constant must fail here, not in a runbook. They are pairwise distinct,
    /// which is what lets the table above pin verdicts through codes; the
    /// exhaustive match makes a sixth verdict a compile error until it is
    /// given its integer here.
    #[test]
    fn exit_codes_are_the_documented_integers() {
        use CommandFailureVerdict::*;
        let documented = |verdict: CommandFailureVerdict| -> u8 {
            match verdict {
                ExpectedRecovery => 10,
                Retryable => 20,
                Terminal => 30,
                SetupEscape => 40,
                Unclassified => 1,
            }
        };
        let all = [
            ExpectedRecovery,
            Retryable,
            Terminal,
            SetupEscape,
            Unclassified,
        ];
        for verdict in all {
            assert_eq!(verdict.exit_code(), documented(verdict), "{verdict:?}");
        }
        let distinct: std::collections::BTreeSet<u8> =
            all.iter().map(|verdict| verdict.exit_code()).collect();
        assert_eq!(
            distinct.len(),
            all.len(),
            "a code pin is a verdict pin only while the codes are distinct"
        );
    }

    #[test]
    fn fee_oracle_bootstrap_preserves_operational_storage_classification() {
        for code in [
            rusqlite::ffi::ErrorCode::DatabaseBusy,
            rusqlite::ffi::ErrorCode::DatabaseLocked,
        ] {
            let mapped = CommandError::from(FeeOracleError::Storage(sqlite(code, 5)));
            assert!(matches!(&mapped, CommandError::Storage(_)));
            assert_eq!(
                mapped.exit_code(),
                EXIT_UNCLASSIFIED,
                "SQLite contention remains restartable during setup's first quote"
            );
        }
    }

    #[test]
    fn fee_oracle_shutdown_ok_is_graceful() {
        let result: Result<Result<(), FeeOracleError>, tokio::task::JoinError> = Ok(Ok(()));
        assert!(WorkerStop::from_shutdown(result).is_ok());
    }

    #[tokio::test]
    async fn panicking_outer_worker_join_is_terminal() {
        let source = tokio::spawn(async {
            panic!("worker invariant failure");
        })
        .await
        .expect_err("task must panic");
        assert_eq!(
            CommandError::Worker(WorkerExit::Lane(WorkerStop::Join(source))).exit_code(),
            EXIT_TERMINAL
        );
    }
}
