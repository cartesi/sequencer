// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Command error taxonomy. Three groupings:
//!
//! - [`BootstrapError`] / [`IdentityError`]: everything that can go wrong
//!   before runtime workers come up — config validation, deployment-identity
//!   guards, startup recovery, initial DB open.
//! - [`WorkerExit`] + per-worker `*Exit`: how each runtime worker exited.
//! - [`CommandError`]: the top-level error every command returns (run,
//!   setup, flush), with generic [`std::io::Error`] /
//!   [`rusqlite::Error`] catch-alls that are used widely enough not to nest.

use thiserror::Error;

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
    #[error("persistent storage invariant violation: {cause}")]
    StorageInvariantViolation { cause: String },
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
// One exhaustive semantic verdict owns failure classification. The
// terminal-fault black box and the orchestrator-facing exit code both derive
// from it, so an operational projection can never become admission policy.
// The exit code remains an ops hint, never protocol authority over the next
// boot. Reserved: 1 (unclassified), 2 (clap usage), and 101 for a panic
// before the command harness can project trusted-code failures to 30. A
// terminal containment that cannot drain within the two-second watchdog
// bound exits via `abort()` (SIGABRT/134), bypassing this projection
// deliberately — supervisors must treat 134 from the sequencer as
// terminal-class (the cause is in the logs and, best-effort, the black box;
// a persistent fault re-detects fail-loud on the next boot that reads it).

/// Restart with backoff; a recovery boot is expected next (it may take 15+
/// min: flush + safe-finality wait). Startup probes must accommodate it.
pub const EXIT_RESTART_EXPECT_RECOVERY: u8 = 10;
/// Restart with backoff; a transient refusal that self-heals when the L1 view
/// freshens. Alert only if it persists.
pub const EXIT_RESTART_TRANSIENT: u8 = 20;
/// Terminal — do not restart; page an operator. The state cannot self-heal.
pub const EXIT_TERMINAL: u8 = 30;
/// Sticky — do **not** auto-restart-loop; an operator must wipe the
/// uncompleted data dir and run `setup --recovery`. The recovery sibling of
/// [`EXIT_RESTART_EXPECT_RECOVERY`] (10), but *operator-initiated*: 10 means
/// "restart and `run()` auto-recovers"; 40 means "a previous instance left work
/// past the checkpoint, and only an explicit fresh `setup --recovery`
/// can resolve it" — a plain restart of `setup` would re-detect and re-refuse
/// forever. Distinct from terminal (30) in that a known recovery procedure
/// *does* fix it.
pub const EXIT_SETUP_NEEDS_RECOVERY: u8 = 40;
/// Unclassified operational failure (for example, a provider error). Restart
/// with backoff.
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
    SetupRecoveryRequired,
    Unclassified,
}

impl CommandFailureVerdict {
    /// Whether the black box records a terminal cause (and the process
    /// exits 30: do not restart, page an operator).
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }

    const fn exit_code(self) -> u8 {
        match self {
            Self::ExpectedRecovery => EXIT_RESTART_EXPECT_RECOVERY,
            Self::Retryable => EXIT_RESTART_TRANSIENT,
            Self::Terminal => EXIT_TERMINAL,
            Self::SetupRecoveryRequired => EXIT_SETUP_NEEDS_RECOVERY,
            Self::Unclassified => EXIT_UNCLASSIFIED,
        }
    }
}

impl CommandError {
    /// Classify this failure once for both the black-box recorder and the
    /// supervisor-facing projection.
    pub(crate) fn failure_verdict(&self) -> CommandFailureVerdict {
        match self {
            CommandError::Worker(WorkerExit::DangerDetected { status }) => {
                danger_failure_verdict(status)
            }
            // A storage decoder panic means the durable state violated an
            // internal contract. Restarting cannot repair that row. Egress
            // reports the same condition through the supervisor's terminal
            // fault signal; background tasks retain it in their exit shape.
            CommandError::StorageInvariantViolation { .. } => CommandFailureVerdict::Terminal,
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
// own error type, beside its enum; `WorkerExit::is_terminal` above
// composes them.

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
        // Transient: self-heal when the L1 view / provider recovers — or,
        // for the data-dir lock, when the previous owner finishes dying.
        // `FeeOracleTransient` is documented "may self-heal", which is this
        // class's definition.
        BootstrapError::ChainIdRpc { .. }
        | BootstrapError::Identity(IdentityError::FirstBootRequiresL1)
        | BootstrapError::DetectionNonceRead { .. }
        | BootstrapError::DataDirLocked { .. }
        | BootstrapError::FeeOracleTransient { .. }
        | BootstrapError::Flush(_) => CommandFailureVerdict::Retryable,

        // Terminal: needs an operator (wrong config, divergence, or a DB that
        // was never set up).
        BootstrapError::ChainIdMismatch { .. }
        | BootstrapError::InvalidProtocolTiming(_)
        | BootstrapError::FeeOracleMisconfig { .. }
        | BootstrapError::FeeOracleFatal { .. }
        | BootstrapError::SignerMisconfig { .. }
        | BootstrapError::SetupNotComplete
        | BootstrapError::CheckpointBeforeAppDeployment { .. }
        | BootstrapError::SetupRecovery(_)
        | BootstrapError::Identity(IdentityError::Mismatch { .. } | IdentityError::OrphanedState) => {
            CommandFailureVerdict::Terminal
        }

        // Sticky setup refusal: a previous instance left work past the
        // checkpoint. Distinct from the auto-recovery class (10) — a plain
        // restart re-refuses; only wipe + `setup --recovery` resolves it.
        BootstrapError::SetupRefuse(_) => CommandFailureVerdict::SetupRecoveryRequired,
        BootstrapError::OpenStorage(_) => CommandFailureVerdict::Unclassified,
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
    /// `eth_chainId` failed on a reachable RPC. We treat this as fatal
    /// rather than warn-and-continue: proceeding with an unverified chain id
    /// would pin a possibly-wrong deployment identity and poison subsequent
    /// L1-unreachable boots, in addition to issuing soft confirmations
    /// against the wrong chain's state. Operator should retry.
    #[error("could not query chain ID from RPC: {message}")]
    ChainIdRpc { message: String },
    /// Protocol-level config (`preemptive_margin_blocks` vs `max_wait_blocks`,
    /// `l1_read_stale_after_blocks` vs `danger_threshold`) failed validation.
    /// See [`ProtocolTimingError`].
    #[error(transparent)]
    InvalidProtocolTiming(#[from] ProtocolTimingError),
    /// Setup-pinned fee oracle configuration or validation is invalid.
    #[error("fee oracle misconfiguration: {message}")]
    FeeOracleMisconfig { message: String },
    /// A live quote/transport failed while bootstrapping. It may self-heal.
    #[error("fee oracle bootstrap transient failure: {message}")]
    FeeOracleTransient { message: String },
    /// Trusted-code join/arithmetic failures while setup persists the first
    /// quote. These need operator attention but are not source misconfiguration.
    #[error("fatal fee oracle bootstrap failure: {message}")]
    FeeOracleFatal { message: String },
    /// The keyed signer provider could not be constructed (bad RPC URL or
    /// private key). Deterministic operator misconfiguration: re-running the
    /// same configuration re-fails identically, so it classifies terminal
    /// like [`Self::ChainIdMismatch`] — the same semantic every command must
    /// share (recovery, setup, and flush previously disagreed).
    #[error("signer provider misconfiguration: {message}")]
    SignerMisconfig { message: String },
    /// Startup recovery (or refusal) failed before runtime workers started.
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
    /// Deployment-identity guards — see [`IdentityError`].
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// `run` (or `flush-mempool`) was invoked against a DB where `setup`
    /// has not completed — its completion fact is absent (setup never ran, or
    /// crashed midway), or its outputs are incomplete. The
    /// operator must run `setup` first; restarting `run` cannot self-heal.
    #[error("setup has not completed for this data dir — run `setup` first")]
    SetupNotComplete,
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
    Flush(#[from] crate::recovery::FlushError),
    /// `setup`'s detection gate could not read the batch-submitter wallet
    /// nonce from L1 (the one live RPC the gate makes). Transient — the
    /// operator retries once the provider recovers; the prior sync already
    /// proved L1 reachable, so this is a hiccup, not a misconfig.
    #[error("setup detection: could not read submitter nonce from RPC: {message}")]
    DetectionNonceRead { message: String },
    /// `setup`'s read-only detection gate found a previous instance left work
    /// this checkpoint cannot account for. Sticky: only wiping the uncompleted
    /// data dir and running `setup --recovery` resolves it — a plain
    /// `setup` restart re-detects and re-refuses.
    #[error(transparent)]
    SetupRefuse(#[from] SetupRefuse),
    /// `setup --checkpoint-block` predates the application's deployment block:
    /// a promotion cannot have landed before the application contract existed.
    /// Operator misconfig; restarting cannot self-heal.
    #[error(
        "checkpoint block {checkpoint_block} predates the application \
         deployment block {app_deployment_block}"
    )]
    CheckpointBeforeAppDeployment {
        checkpoint_block: u64,
        app_deployment_block: u64,
    },
    /// `setup --recovery` failed in a way only the operator can fix (bad config,
    /// a checkpoint that can't be loaded or doesn't fit the chain, or a DB that
    /// is already set up). Terminal — see [`SetupRecoveryError`].
    #[error(transparent)]
    SetupRecovery(#[from] SetupRecoveryError),
}

/// Terminal failures of the `setup --recovery` procedure — the ones
/// an operator must resolve (the flush and the post-flush re-sync reuse the
/// transient [`RecoveryError`] paths instead). All map to [`EXIT_TERMINAL`]:
/// a plain restart re-runs the same bad inputs and re-fails identically.
#[derive(Debug, Error)]
pub enum SetupRecoveryError {
    /// Cross-field config validation failed (recovery missing its dump dir /
    /// checkpoint block / key, or a plain `setup` carrying recovery-only args).
    /// See [`crate::commands::config::SetupConfig::validate`].
    #[error("invalid recovery configuration: {message}")]
    InvalidConfig { message: String },
    /// `setup --recovery` was invoked against a DB that is already set up.
    /// Recovery is a strict one-shot on a freshly-wiped DB (wipe and re-run with
    /// `--recovery`); re-pointing a live deployment at a different checkpoint
    /// would strand its existing state.
    #[error(
        "`setup --recovery` requires a freshly-wiped data dir, but this one is \
         already set up — wipe it and re-run"
    )]
    AlreadySetUp,
    /// The checkpoint dump could not be loaded (missing/corrupt `info.toml`, or
    /// the app's `from_dump` failed). Operator must supply a valid **sequencer**
    /// dump dir (`info.toml` + `state/`), not a watchdog CM checkpoint.
    #[error("failed to load checkpoint dump at {path}: {message}")]
    CheckpointLoad { path: String, message: String },
    /// The checkpoint's last-executed safe block `A` is not strictly before the
    /// checkpoint block `B`. The fold reconstructs the `(A, B]` fridge, so
    /// `A < B` must hold — otherwise the checkpoint dump and
    /// `--checkpoint-block` describe inconsistent points.
    #[error(
        "checkpoint last-executed safe block {executed_safe_block} (A) is not \
         before checkpoint block {checkpoint_block} (B)"
    )]
    CheckpointNotBeforeBlock {
        executed_safe_block: u64,
        checkpoint_block: u64,
    },
    /// The input reader reported a successful post-flush sync without
    /// persisting the safe-head observation that the recovery fold requires.
    #[error(
        "post-flush L1 resync completed without a persisted safe-head observation: \
         internal storage invariant violation"
    )]
    MissingResyncedSafeHead,
    /// A re-run of `setup --recovery` found a root tip from a *prior* (crashed
    /// before setup completion) attempt whose nonce differs from this
    /// attempt's resume nonce — a different checkpoint, or the same one after the
    /// post-flush head `C` advanced. The half-recovered DB cannot be resumed
    /// onto a tree rooted at the old nonce (the anchor would move but the
    /// existing root tip would not, silently breaking I16). Wipe the data dir
    /// and re-run.
    #[error(
        "partial recovery: existing root tip carries nonce {existing_root_nonce}, \
         but this attempt resumes at {requested_nonce} — wipe the data dir and re-run"
    )]
    PartialRecoveryMismatch {
        existing_root_nonce: u64,
        requested_nonce: u64,
    },
    /// A re-run of `setup --recovery` found a root tip carrying *this* attempt's
    /// resume nonce but **no finalized snapshot** — a prior attempt that crashed
    /// between opening the root tip and writing the snapshot. It cannot be
    /// resumed safely: a re-sync may have advanced `C` with new direct inputs
    /// (which leave `N'` unchanged) that resuming would leave unsequenced, so the
    /// snapshot cursor would lag the folded `S'` and `run` would drain+execute
    /// them a second time (divergence). Wipe the data dir and re-run (the
    /// one-shot recovery model).
    #[error(
        "partial recovery: root tip at nonce {root_nonce} exists with no finalized \
         snapshot (crashed mid-fill) — wipe the data dir and re-run"
    )]
    PartialRecoveryIncomplete { root_nonce: u64 },
    /// `setup --recovery` found a finalized snapshot but **no root tip**. A
    /// completed cockroach fill always has both (the tip is opened in step 2,
    /// before the snapshot in step 4), so this is residue from a *different*
    /// deployment mode left in the data dir — a plain `setup` that registered the
    /// genesis finalized snapshot and crashed before setup completion.
    /// Folding `(S', N')` and then silently keeping the old snapshot would mark
    /// setup complete over the genesis state instead of the recovered state. Wipe
    /// the data dir and re-run `setup --recovery`.
    #[error(
        "setup --recovery found a finalized snapshot (block {existing_finalized_block}) \
         with no root tip — residue from an incomplete plain `setup`; wipe the data \
         dir and re-run"
    )]
    RecoveryOverResidualSnapshot { existing_finalized_block: u64 },
    /// A plain (non-recovery) `setup` found a non-zero batch-tree anchor —
    /// residue from a `setup --recovery` that crashed before completion. Booting
    /// a genesis deployment over it would root the tree at the recovery nonce
    /// instead of 0. Wipe the data dir, then run plain `setup` or re-run
    /// `setup --recovery`.
    #[error(
        "plain setup found batch-tree anchor {anchor} (≠ 0) — leftover from an \
         incomplete `setup --recovery`; wipe the data dir and re-run"
    )]
    GenesisOverRecoveryResidue { anchor: u64 },
}

/// `setup`'s read-only detection gate: the reasons a
/// fresh `setup` refuses because a *previous* instance left work past the
/// checkpoint. Because plain setup has already initialized a genesis baseline,
/// the remedy is to wipe that uncompleted data dir and run `setup --recovery`
/// which flushes/folds the outstanding batches; a plain `setup` restart
/// re-detects and re-refuses (hence [`EXIT_SETUP_NEEDS_RECOVERY`], not the
/// auto-recovery class 10).
///
/// Both variants carry diagnostic fields for the refusal log line.
#[derive(Debug, Error)]
pub enum SetupRefuse {
    /// Step 1: the batch-submitter wallet nonce is not settled
    /// (`pending > safe`) on the local provider — a previous instance left
    /// pending or mined-but-unsafe batch txs. Local-view only: a
    /// zombie tx dropped from this provider's pool but alive elsewhere evades
    /// this check; bounded at runtime by the content-identity check.
    #[error(
        "batch-submitter wallet nonce not settled (pending {pending} > safe \
         {safe}) — a previous instance left in-flight batch txs; wipe this \
         uncompleted data dir, then run `setup --recovery`"
    )]
    WalletNonceUnsettled { pending: u64, safe: u64 },
    /// Step 2: a batch-submitter tx exists in `(checkpoint_block, safe]` — a
    /// previous instance already wrote batches past this checkpoint, so a
    /// genesis-style bootstrap would silently diverge from canonical state.
    #[error(
        "batch-submitter input found at block {found_block} past checkpoint \
         block {checkpoint_block} (safe_input_index {safe_input_index}) — wipe \
         this uncompleted data dir, then run `setup --recovery`"
    )]
    BatchPastCheckpoint {
        checkpoint_block: u64,
        found_block: u64,
        safe_input_index: u64,
    },
}

/// Deployment-identity failure modes. The sequencer pins itself to a specific
/// (chain_id, app_address, input_box_address, app_deployment_block,
/// batch_submitter_address, fee_oracle) tuple on first successful boot, then
/// refuses to run under a different identity to prevent silently associating
/// state from one deployment with another.
#[derive(Debug, Error)]
pub enum IdentityError {
    /// L1 unreachable AND no cached identity in the DB. We need at least one
    /// (live L1 query OR a prior boot's pinned identity) to safely bind this
    /// sequencer to a deployment. Operator: bring up L1 and retry.
    #[error("first boot requires L1: no cached deployment identity and L1 is unreachable")]
    FirstBootRequiresL1,
    /// The DB has persisted state but no pinned identity. Binding the current
    /// config now would silently inherit an unknown deployment's data.
    /// Operator: confirm provenance or wipe the DB.
    #[error("orphaned state: DB has persisted state but no deployment identity to claim it")]
    OrphanedState,
    /// The pinned identity doesn't match the current config.
    ///
    /// `stored` and `expected` are boxed so the enum stays small — without
    /// boxing this variant alone would push `CommandError`'s stack footprint past
    /// 184 bytes, which clippy's `result_large_err` flags (and which inflates
    /// every `Result<_, CommandError>` in the codebase, even successful returns).
    /// The heap allocation is paid only on the error path, which is cold.
    #[error("deployment identity mismatch ({fields}); stored={stored:?}; expected={expected:?}")]
    Mismatch {
        fields: String,
        stored: Box<DeploymentIdentity>,
        expected: Box<DeploymentIdentity>,
    },
}

// ── Worker exits ───────────────────────────────────────────────────────

/// Which runtime worker exited, and why. One generic stop shape per worker
/// (this replaced six hand-copied per-worker enums), plus the danger
/// detector's deliberate `RecoveryRequired` trip as its own first-class arm:
/// not an error, but causes the runtime to exit so the orchestrator can
/// respawn into startup recovery.
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
            WorkerExit::DangerDetected { status } => {
                matches!(
                    status,
                    DangerStatus::CanonicalDivergence(_) | DangerStatus::Safe
                )
            }
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
            ProcessLockError::Io(source) => CommandError::Io(source),
        }
    }
}

/// One shared classification for the keyed signer-provider constructor, so
/// `setup`, `flush-mempool`, and any future keyed command cannot drift (the
/// three sites previously classified `Create` three different ways). The
/// run-recovery reducer keeps its own explicit Retry/Refuse polarity map —
/// that polarity is the reducer's to own — but its terminal/transient split
/// must agree with this one.
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

impl From<crate::recovery::FlushError> for CommandError {
    fn from(e: crate::recovery::FlushError) -> Self {
        CommandError::Bootstrap(BootstrapError::Flush(e))
    }
}

impl From<SetupRefuse> for CommandError {
    fn from(e: SetupRefuse) -> Self {
        CommandError::Bootstrap(BootstrapError::SetupRefuse(e))
    }
}

impl From<SetupRecoveryError> for CommandError {
    fn from(e: SetupRecoveryError) -> Self {
        CommandError::Bootstrap(BootstrapError::SetupRecovery(e))
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
            FeeOracleError::FatalMath(error) => {
                CommandError::Bootstrap(BootstrapError::FeeOracleFatal {
                    message: FeeOracleError::FatalMath(error).to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l1::fee_oracle::worker::FeeOracleError;
    use crate::l1::submitter::BatchPosterError;
    use crate::l1::watermark::WalletNonceWatermarkError;
    use crate::recovery::{
        FlushError, RecoveryError, RecoveryFailure, RecoveryRefusalReason, RecoveryRetryReason,
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
            fee_oracle: crate::storage::FeeOracleIdentity::Fixed { log_gas_price: 0 },
        }
    }

    #[test]
    fn semantic_verdict_drives_lifecycle_and_exit_projection() {
        let cases = [
            (
                danger(DangerStatus::TipInDanger(3)),
                CommandFailureVerdict::ExpectedRecovery,
                EXIT_RESTART_EXPECT_RECOVERY,
            ),
            (
                CommandError::Bootstrap(BootstrapError::ChainIdRpc {
                    message: "provider unavailable".into(),
                }),
                CommandFailureVerdict::Retryable,
                EXIT_RESTART_TRANSIENT,
            ),
            (
                CommandError::StorageInvariantViolation {
                    cause: "broken durable invariant".into(),
                },
                CommandFailureVerdict::Terminal,
                EXIT_TERMINAL,
            ),
            (
                CommandError::from(SetupRefuse::WalletNonceUnsettled {
                    pending: 14,
                    safe: 13,
                }),
                CommandFailureVerdict::SetupRecoveryRequired,
                EXIT_SETUP_NEEDS_RECOVERY,
            ),
            (
                CommandError::Io(std::io::Error::other("operational failure")),
                CommandFailureVerdict::Unclassified,
                EXIT_UNCLASSIFIED,
            ),
        ];

        for (error, expected_verdict, expected_exit_code) in cases {
            let verdict = error.failure_verdict();
            assert_eq!(verdict, expected_verdict);
            assert_eq!(error.exit_code(), expected_exit_code);
            assert_eq!(
                verdict.is_terminal(),
                matches!(verdict, CommandFailureVerdict::Terminal)
            );
        }
    }

    #[test]
    fn r4_class_10_expect_recovery_boot() {
        assert_eq!(
            danger(DangerStatus::ClosedBatchInDanger(0)).exit_code(),
            EXIT_RESTART_EXPECT_RECOVERY
        );
        assert_eq!(
            danger(DangerStatus::TipInDanger(3)).exit_code(),
            EXIT_RESTART_EXPECT_RECOVERY
        );
    }

    #[test]
    fn r4_class_20_transient_refusal() {
        assert_eq!(
            danger(DangerStatus::L1ViewStale).exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            danger(DangerStatus::EstimatedBatchInDanger(2)).exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::retry(
                RecoveryRetryReason::L1ViewStale,
            )))
            .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Identity(IdentityError::FirstBootRequiresL1))
                .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::ChainIdRpc {
                message: "x".into()
            })
            .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            CommandError::from(FlushError::Provider("x".into())).exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        // Documented "may self-heal" — the definition of this class (it
        // previously projected to unclassified/1, misleading restart policy).
        assert_eq!(
            CommandError::from(FeeOracleError::Transient("RPC unavailable".into())).exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            CommandError::from(FlushError::Watermark(WalletNonceWatermarkError::Storage(
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                        extended_code: 5,
                    },
                    None,
                )
            ),))
            .exit_code(),
            EXIT_RESTART_TRANSIENT,
            "operational watermark contention remains retryable"
        );
        // A flush failure surfaced via startup recovery must land in the same
        // class as the flush-mempool subcommand's FlushError (review M2).
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::retry(
                RecoveryFailure::Flush(FlushError::Provider("x".into())),
            )))
            .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::retry(
                RecoveryRetryReason::ResyncBehindFlushView {
                    resynced_safe_block: 1,
                    flush_observed_safe_block: 2,
                },
            )))
            .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
    }

    #[test]
    fn r4_class_30_terminal_operator_required() {
        assert_eq!(
            CommandError::StorageInvariantViolation {
                cause: "test cause".into()
            }
            .exit_code(),
            EXIT_TERMINAL
        );
        // A deterministic signer-construction misconfig (bad RPC URL or
        // private key) classifies terminal in every command, matching the
        // ChainIdMismatch precedent; setup/flush previously projected it
        // unclassified while recovery refused it.
        assert_eq!(
            CommandError::from(crate::l1::provider::VerifiedSignerProviderError::Create(
                "bad key".into()
            ))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::ReferencedSnapshotArtifact {
                path: "/durable/snapshot".into(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            }
            .exit_code(),
            EXIT_TERMINAL,
            "a missing DB-referenced snapshot cannot self-heal on restart"
        );
        assert_eq!(
            CommandError::Storage(rusqlite::Error::QueryReturnedNoRows).exit_code(),
            EXIT_TERMINAL,
            "a mandatory durable row disappearing cannot self-heal on restart"
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::DangerDetector(WorkerStop::Source(
                DangerDetectorError::Storage(rusqlite::Error::QueryReturnedNoRows,)
            ),))
            .exit_code(),
            EXIT_TERMINAL,
            "typed persistent storage errors retain terminal classification through workers"
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::Source(
                FeeOracleError::Storage(rusqlite::Error::QueryReturnedNoRows),
            )))
            .exit_code(),
            EXIT_TERMINAL,
            "the newer fee-oracle worker retains persistent storage classification"
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::Source(
                FeeOracleError::Join("blocking storage task panicked".into()),
            )))
            .exit_code(),
            EXIT_TERMINAL,
            "a fee-oracle blocking-task panic is a trusted-code failure"
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::InputReader(WorkerStop::Source(
                InputReaderError::StorageTaskPanicked {
                    operation: "reading corrupt state",
                },
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::BatchSubmitter(WorkerStop::Source(
                BatchSubmitterError::StorageTaskPanicked {
                    operation: "reading corrupt state",
                },
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::DangerDetector(WorkerStop::Source(
                DangerDetectorError::StorageTaskPanicked,
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::BatchSubmitter(WorkerStop::Source(
                BatchSubmitterError::Poster(BatchPosterError::StorageInvariantViolation),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        let persistent_watermark =
            || WalletNonceWatermarkError::Storage(rusqlite::Error::QueryReturnedNoRows);
        assert_eq!(
            CommandError::Worker(WorkerExit::BatchSubmitter(WorkerStop::Source(
                BatchSubmitterError::Poster(BatchPosterError::Watermark(persistent_watermark())),
            )))
            .exit_code(),
            EXIT_TERMINAL,
            "persistent write-before-broadcast storage failure must not retry as a provider error"
        );
        assert_eq!(
            CommandError::from(FlushError::Watermark(persistent_watermark())).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::refuse(
                RecoveryFailure::Flush(FlushError::Watermark(persistent_watermark())),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            danger(DangerStatus::CanonicalDivergence(0)).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::refuse(
                RecoveryRefusalReason::CanonicalDivergence { nonce: 0 },
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::SetupNotComplete).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::ChainIdMismatch { rpc: 1, config: 2 })
                .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::InvalidProtocolTiming(
                ProtocolTimingError::MarginNotLessThanMaxWait {
                    margin: 1200,
                    max_wait: 1200
                }
            ))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Identity(IdentityError::OrphanedState))
                .exit_code(),
            EXIT_TERMINAL
        );
        // `setup --recovery` operator-fixable failures are terminal (a restart
        // re-runs the same bad inputs).
        assert_eq!(
            CommandError::from(SetupRecoveryError::AlreadySetUp).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch {
                fields: "chain_id".into(),
                stored: Box::new(dummy_identity()),
                expected: Box::new(dummy_identity()),
            }))
            .exit_code(),
            EXIT_TERMINAL
        );
        // Partial-recovery residue: operator must wipe — terminal.
        assert_eq!(
            CommandError::from(SetupRecoveryError::PartialRecoveryMismatch {
                existing_root_nonce: 3,
                requested_nonce: 5,
            })
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::from(SetupRecoveryError::GenesisOverRecoveryResidue { anchor: 7 })
                .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::from(SetupRecoveryError::PartialRecoveryIncomplete { root_nonce: 3 })
                .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::from(SetupRecoveryError::RecoveryOverResidualSnapshot {
                existing_finalized_block: 0,
            })
            .exit_code(),
            EXIT_TERMINAL
        );
        // Reader-level chain-id mismatch (warm-boot backstop) is terminal, like
        // the boot-time BootstrapError::ChainIdMismatch.
        assert_eq!(
            CommandError::Worker(WorkerExit::InputReader(WorkerStop::Source(
                InputReaderError::ChainIdMismatch {
                    rpc: 1,
                    expected: 31337,
                }
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        // The same mismatch surfacing during a startup-recovery safe-head sync
        // (RecoveryError path) is terminal too — not the unclassified Recovery
        // catch-all (which would loop on the wrong chain).
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::refuse(
                RecoveryFailure::InputReader(InputReaderError::ChainIdMismatch {
                    rpc: 1,
                    expected: 31337,
                }),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::refuse(
                RecoveryFailure::InputReader(InputReaderError::StorageTaskPanicked {
                    operation: "startup sync",
                }),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
    }

    #[test]
    fn startup_recovery_reader_persistent_open_failure_is_terminal() {
        let source = StorageOpenError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::NotADatabase,
                extended_code: 26,
            },
            None,
        ));
        let error = CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::refuse(
            RecoveryFailure::OpenStorage(source),
        )));

        assert_eq!(error.exit_code(), EXIT_TERMINAL);
    }

    #[test]
    fn startup_recovery_reader_busy_open_failure_remains_restartable() {
        let source = StorageOpenError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            None,
        ));
        let error = CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::retry(
            RecoveryFailure::OpenStorage(source),
        )));

        assert_eq!(error.exit_code(), EXIT_RESTART_TRANSIENT);
    }

    #[test]
    fn r4_class_40_setup_needs_operator_recovery() {
        // Sticky setup refusals: the operator must wipe the uncompleted data
        // dir and run `setup --recovery`, not plain-restart (which would
        // re-detect and re-refuse) — so they get a dedicated code, distinct
        // from the auto-recovery class (10).
        assert_eq!(
            CommandError::from(SetupRefuse::WalletNonceUnsettled {
                pending: 14,
                safe: 13,
            })
            .exit_code(),
            EXIT_SETUP_NEEDS_RECOVERY
        );
        assert_eq!(
            CommandError::from(SetupRefuse::BatchPastCheckpoint {
                checkpoint_block: 100,
                found_block: 250,
                safe_input_index: 7,
            })
            .exit_code(),
            EXIT_SETUP_NEEDS_RECOVERY
        );
        // A checkpoint predating genesis is operator misconfig — terminal (30),
        // not a recovery trigger.
        assert_eq!(
            CommandError::Bootstrap(BootstrapError::CheckpointBeforeAppDeployment {
                checkpoint_block: 5,
                app_deployment_block: 10,
            })
            .exit_code(),
            EXIT_TERMINAL
        );
    }

    #[test]
    fn r4_class_1_unclassified() {
        assert_eq!(
            CommandError::Io(std::io::Error::other("boom")).exit_code(),
            EXIT_UNCLASSIFIED
        );
        assert_eq!(
            CommandError::ReferencedSnapshotArtifact {
                path: "/durable/snapshot".into(),
                source: std::io::Error::other("filesystem unavailable"),
            }
            .exit_code(),
            EXIT_UNCLASSIFIED,
            "operational filesystem failures remain restartable"
        );
        assert_eq!(
            CommandError::Storage(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                    extended_code: 5,
                },
                None,
            ))
            .exit_code(),
            EXIT_UNCLASSIFIED,
            "transient storage contention remains restartable"
        );
        // Lifecycle classifies by variant, not wholesale: fact refusals
        // page; a busy black-box write must not.
        assert_eq!(
            CommandError::Lifecycle(LifecycleError::Storage(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                    extended_code: 5,
                },
                None,
            )))
            .exit_code(),
            EXIT_UNCLASSIFIED,
            "a transient lifecycle storage failure remains restartable"
        );
        assert_eq!(
            CommandError::Lifecycle(LifecycleError::Storage(
                rusqlite::Error::QueryReturnedNoRows
            ))
            .exit_code(),
            EXIT_TERMINAL,
            "a persistent lifecycle storage failure pages"
        );
        assert_eq!(
            CommandError::Lifecycle(LifecycleError::NotAdmissible {
                requested: crate::storage::LifecycleCommand::Run,
                reason: "setup has not completed for this data directory",
            })
            .exit_code(),
            EXIT_TERMINAL,
            "an admission-fact refusal pages"
        );
        assert_eq!(
            CommandError::Lifecycle(LifecycleError::CanonicalDivergence { nonce: 7 }).exit_code(),
            EXIT_TERMINAL,
            "divergence pages"
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::Server(WorkerStop::StoppedUnexpectedly)).exit_code(),
            EXIT_UNCLASSIFIED
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::Source(
                FeeOracleError::Transient("RPC unavailable".into()),
            )))
            .exit_code(),
            EXIT_UNCLASSIFIED
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::Source(
                FeeOracleError::Storage(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                        extended_code: 5,
                    },
                    None,
                )),
            )))
            .exit_code(),
            EXIT_UNCLASSIFIED,
            "fee-oracle storage contention remains restartable"
        );
    }

    #[test]
    fn fee_oracle_bootstrap_preserves_operational_storage_classification() {
        for code in [
            rusqlite::ffi::ErrorCode::DatabaseBusy,
            rusqlite::ffi::ErrorCode::DatabaseLocked,
        ] {
            let error = FeeOracleError::Storage(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code,
                    extended_code: 5,
                },
                None,
            ));
            let mapped = CommandError::from(error);
            assert!(matches!(&mapped, CommandError::Storage(_)));
            assert_eq!(
                mapped.exit_code(),
                EXIT_UNCLASSIFIED,
                "SQLite contention remains restartable during setup's first quote"
            );
        }
    }

    #[test]
    fn fee_oracle_fatal_math_is_terminal_on_bootstrap_and_worker() {
        use crate::l1::fee_oracle::math::MathError;
        assert_eq!(
            CommandError::from(FeeOracleError::FatalMath(MathError::Overflow)).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::Source(
                FeeOracleError::FatalMath(MathError::Overflow),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::Source(
                FeeOracleError::FatalMath(MathError::ExceedsRepresentableRange),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::from(FeeOracleError::Misconfig("wrong pair".into())).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            CommandError::Worker(WorkerExit::FeeOracle(WorkerStop::Source(
                FeeOracleError::Misconfig("wrong pair".into()),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
    }

    #[test]
    fn fee_oracle_shutdown_ok_is_graceful() {
        let result: Result<Result<(), FeeOracleError>, tokio::task::JoinError> = Ok(Ok(()));
        assert!(WorkerStop::from_shutdown(result).is_ok());
    }

    #[test]
    fn r4_app_bootstrap_internal_error_is_terminal() {
        assert_eq!(
            CommandError::AppBootstrap(AppError::Internal {
                reason: "application invariant failed".into(),
            })
            .exit_code(),
            EXIT_TERMINAL
        );
    }

    #[test]
    fn r4_app_bootstrap_io_error_is_unclassified() {
        assert_eq!(
            CommandError::AppBootstrap(AppError::Io(std::io::Error::other("disk unavailable")))
                .exit_code(),
            EXIT_UNCLASSIFIED
        );
    }

    #[tokio::test]
    async fn r4_panicking_outer_worker_join_is_terminal() {
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
