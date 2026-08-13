// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Runtime error taxonomy. Three groupings:
//!
//! - [`BootstrapError`] / [`IdentityError`]: everything that can go wrong
//!   before runtime workers come up — config validation, deployment-identity
//!   guards, startup recovery, initial DB open.
//! - [`WorkerExit`] + per-worker `*Exit`: how each runtime worker exited.
//! - [`RunError`]: the top-level error returned by `run()`, with generic
//!   [`std::io::Error`] / [`rusqlite::Error`] catch-alls that are used widely
//!   enough not to nest.

use thiserror::Error;

use crate::ingress::inclusion_lane::InclusionLaneError;
use crate::l1::fee_oracle::worker::FeeOracleError;
use crate::l1::reader::InputReaderError;
use crate::l1::submitter::{BatchPosterError, BatchSubmitterError};
use crate::recovery::{DangerDetectorError, RecoveryError, RefuseReason};
use crate::storage::{DangerStatus, DeploymentIdentity, StorageOpenError};
use sequencer_core::protocol::ProtocolTimingError;

// ── Top-level RunError ────────────────────────────────────────────────

/// Top-level runtime error. Grouped by phase:
///
/// - `Bootstrap`: startup failures before runtime workers come up.
/// - `Worker`: one of the runtime workers exited (server, inclusion lane,
///   input reader, batch submitter, danger detector, fee oracle).
/// - `Io` / `Storage`: generic catch-alls used widely; not worth nesting.
#[derive(Debug, Error)]
pub enum RunError {
    #[error("bootstrap failed: {0}")]
    Bootstrap(#[from] BootstrapError),
    #[error("worker exited: {0}")]
    Worker(#[from] WorkerExit),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("storage operation failed: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("application bootstrap failed: {0}")]
    AppBootstrap(#[from] sequencer_core::application::AppError),
}

// ── R4 exit-code projection (WP10) ─────────────────────────────────────
//
// The orchestrator contract: a pure projection of `RunError` into a process
// exit code so the supervisor can tell "restart me" from "restarting is
// futile" without parsing logs. Lives here (one match), called by the harness
// so every app binary inherits it. The exit code is an ops hint, never
// protocol — authority over what the next boot does stays with startup's own
// `check_danger`. Reserved: 1 (unclassified), 2 (clap usage), 101 (panic).

/// Restart with backoff; a recovery boot is expected next (it may take 15+
/// min: flush + safe-finality wait). Startup probes must accommodate it.
pub const EXIT_RESTART_EXPECT_RECOVERY: u8 = 10;
/// Restart with backoff; a transient refusal that self-heals when the L1 view
/// freshens. Alert only if it persists.
pub const EXIT_RESTART_TRANSIENT: u8 = 20;
/// Terminal — do not restart; page an operator. The state cannot self-heal.
pub const EXIT_TERMINAL: u8 = 30;
/// Sticky — do **not** auto-restart-loop; an operator must run
/// `setup --recovery`. The recovery sibling of [`EXIT_RESTART_EXPECT_RECOVERY`]
/// (10), but *operator-initiated*: 10 means "restart and `run()` auto-recovers";
/// 40 means "a previous instance left work past the checkpoint, and only an
/// explicit `setup --recovery` (PR5) can resolve it" — a plain restart of
/// `setup` would re-detect and re-refuse forever. Distinct from terminal (30)
/// in that a known recovery command *does* fix it.
pub const EXIT_SETUP_NEEDS_RECOVERY: u8 = 40;
/// Unclassified failure (worker crash, provider error). Restart with backoff.
pub const EXIT_UNCLASSIFIED: u8 = 1;

impl RunError {
    /// Project this error onto the R4 exit-code contract. Clean shutdown
    /// (exit 0) is handled by the caller — a `RunError` is always a failure.
    pub fn exit_code(&self) -> u8 {
        match self {
            RunError::Worker(WorkerExit::DangerDetector(DangerDetectorExit::DangerDetected {
                status,
            })) => danger_exit_code(status),
            // A wrong-chain RPC caught by the reader after boot (the warm-boot
            // deferral's backstop) is terminal, like the boot-time
            // `BootstrapError::ChainIdMismatch` — a misconfig the operator must
            // fix; a blind restart re-hits the same wrong chain.
            RunError::Worker(WorkerExit::InputReader(InputReaderExit::Source(
                InputReaderError::ChainIdMismatch { .. },
            ))) => EXIT_TERMINAL,
            // Same for the submitter's pre-send chain-id gate: a wrong-chain RPC
            // is an operator misconfig, terminal rather than a restart loop that
            // keeps refusing to sign onto it.
            RunError::Worker(WorkerExit::BatchSubmitter(BatchSubmitterExit::Source(
                BatchSubmitterError::Poster(BatchPosterError::ChainIdMismatch { .. }),
            ))) => EXIT_TERMINAL,
            // Fee-oracle arithmetic that cannot encode without undercharging, or
            // a pool/config misconfig discovered at runtime, is terminal — page
            // an operator rather than restart-loop.
            RunError::Worker(WorkerExit::FeeOracle(FeeOracleExit::Source(
                FeeOracleError::FatalMath(_) | FeeOracleError::Misconfig(_),
            ))) => EXIT_TERMINAL,
            RunError::Bootstrap(b) => bootstrap_exit_code(b),
            // Worker crashes, provider errors, IO/storage/app catch-alls.
            RunError::Worker(_)
            | RunError::Io(_)
            | RunError::Storage(_)
            | RunError::AppBootstrap(_) => EXIT_UNCLASSIFIED,
        }
    }
}

fn danger_exit_code(status: &DangerStatus) -> u8 {
    match status {
        // A doomed closed batch / aging Tip: the next boot legitimately runs
        // a slow recovery (flush + cascade).
        DangerStatus::ClosedBatchInDanger(_) | DangerStatus::TipInDanger(_) => {
            EXIT_RESTART_EXPECT_RECOVERY
        }
        // View-dependent refusals that self-heal once the provider recovers.
        DangerStatus::L1ViewStale | DangerStatus::EstimatedBatchInDanger(_) => {
            EXIT_RESTART_TRANSIENT
        }
        // The only genuinely terminal danger: canonical divergence (R2).
        DangerStatus::CanonicalDivergence(_) => EXIT_TERMINAL,
        // `Safe` is never a detector exit. If it ever reaches here the danger
        // classification is self-contradicting — page an operator (EXIT_TERMINAL)
        // rather than silently restart-loop with backoff (EXIT_UNCLASSIFIED).
        DangerStatus::Safe => {
            debug_assert!(false, "danger_exit_code called with DangerStatus::Safe");
            EXIT_TERMINAL
        }
    }
}

fn bootstrap_exit_code(err: &BootstrapError) -> u8 {
    match err {
        // Transient: self-heal when the L1 view / provider recovers.
        BootstrapError::ChainIdRpc { .. }
        | BootstrapError::Identity(IdentityError::FirstBootRequiresL1)
        | BootstrapError::DetectionNonceRead { .. }
        | BootstrapError::Flush(_) => EXIT_RESTART_TRANSIENT,
        BootstrapError::Recovery(RecoveryError::Refuse(
            RefuseReason::L1ViewStale | RefuseReason::EstimatedBatchInDanger { .. },
        )) => EXIT_RESTART_TRANSIENT,
        // A flush failure or a re-sync lagging the flush view during startup
        // recovery is transient — the respawn retries with a fresher view.
        // (Same class the `flush-mempool` subcommand gives a `FlushError`.)
        BootstrapError::Recovery(
            RecoveryError::Flush(_) | RecoveryError::ResyncBehindFlushView { .. },
        ) => EXIT_RESTART_TRANSIENT,

        // Terminal: needs an operator (wrong config, divergence, or a DB that
        // was never set up).
        BootstrapError::ChainIdMismatch { .. }
        | BootstrapError::InvalidProtocolTiming(_)
        | BootstrapError::FeeOracleMisconfig { .. }
        | BootstrapError::SetupNotComplete
        | BootstrapError::CheckpointBeforeAppDeployment { .. }
        | BootstrapError::SetupRecovery(_)
        | BootstrapError::Identity(IdentityError::Mismatch { .. } | IdentityError::OrphanedState) => {
            EXIT_TERMINAL
        }

        // Sticky setup refusal: a previous instance left work past the
        // checkpoint. Distinct from the auto-recovery class (10) — a plain
        // restart re-refuses; only `setup --recovery` (PR5) resolves it.
        BootstrapError::SetupRefuse(_) => EXIT_SETUP_NEEDS_RECOVERY,
        BootstrapError::Recovery(RecoveryError::Refuse(RefuseReason::CanonicalDivergence {
            ..
        })) => EXIT_TERMINAL,
        // A wrong-chain RPC caught by the reader *during* a startup-recovery sync
        // is terminal, like the boot-time and worker-path ChainIdMismatch arms —
        // an operator misconfig a blind restart re-hits (without this it would
        // fall to the catch-all below and loop on the wrong chain).
        BootstrapError::Recovery(RecoveryError::InputReader(
            InputReaderError::ChainIdMismatch { .. },
        )) => EXIT_TERMINAL,
        // Same class for the preemptive-recovery flush's pre-signing chain-id
        // gate: a mismatch is an operator misconfig, terminal rather than a
        // restart loop on the wrong chain. (An RPC *error* during the check
        // surfaces as `RecoveryError::Provider` and falls to the retry arm.)
        BootstrapError::Recovery(RecoveryError::ChainIdMismatch { .. }) => EXIT_TERMINAL,

        // Other recovery / storage-open failures: unclassified, retry.
        BootstrapError::Recovery(_)
        | BootstrapError::OpenStorage(_)
        | BootstrapError::FeeOracleTransient { .. } => EXIT_UNCLASSIFIED,
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
    /// Startup recovery (or refusal) failed before runtime workers started.
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
    /// Deployment-identity guards — see [`IdentityError`].
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// `run` (or `flush-mempool`) was invoked against a DB where `setup`
    /// has not completed — the `setup_complete` marker is absent (setup
    /// never ran, or crashed midway), or its outputs are incomplete. The
    /// operator must run `setup` first; restarting `run` cannot self-heal.
    #[error("setup has not completed for this data dir — run `setup` first")]
    SetupNotComplete,
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
    /// this checkpoint cannot account for. Sticky: only `setup --recovery`
    /// (PR5) resolves it — a plain `setup` restart re-detects and re-refuses.
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
    /// See [`crate::runtime::config::SetupConfig::validate`].
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
    /// A re-run of `setup --recovery` found a root tip from a *prior* (crashed
    /// before the `setup_complete` marker) attempt whose nonce differs from this
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
    /// genesis finalized snapshot and crashed before its `setup_complete` marker.
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
    /// residue from a `setup --recovery` that crashed before its marker. Booting
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
/// checkpoint. The remedy is `setup --recovery` (PR5), which flushes/folds the
/// outstanding batches; a plain `setup` restart re-detects and re-refuses
/// (hence [`EXIT_SETUP_NEEDS_RECOVERY`], not the auto-recovery class 10).
///
/// Both variants carry diagnostic fields for the refusal log line, mirroring
/// the danger-detector [`RefuseReason`] precedent.
#[derive(Debug, Error)]
pub enum SetupRefuse {
    /// Step 1: the batch-submitter wallet nonce is not settled
    /// (`pending > safe`) on the local provider — a previous instance left
    /// pending or mined-but-unsafe batch txs. Local-view only (review F1): a
    /// zombie tx dropped from this provider's pool but alive elsewhere evades
    /// this check; bounded at runtime by the content-identity check.
    #[error(
        "batch-submitter wallet nonce not settled (pending {pending} > safe \
         {safe}) — a previous instance left in-flight batch txs; run \
         `setup --recovery`"
    )]
    WalletNonceUnsettled { pending: u64, safe: u64 },
    /// Step 2: a batch-submitter tx exists in `(checkpoint_block, safe]` — a
    /// previous instance already wrote batches past this checkpoint, so a
    /// genesis-style bootstrap would silently diverge from canonical state.
    #[error(
        "batch-submitter input found at block {found_block} past checkpoint \
         block {checkpoint_block} (safe_input_index {safe_input_index}) — run \
         `setup --recovery`"
    )]
    BatchPastCheckpoint {
        checkpoint_block: u64,
        found_block: u64,
        safe_input_index: u64,
    },
}

/// Deployment-identity failure modes. The sequencer pins itself to a specific
/// (chain_id, app_address, input_box_address, app_deployment_block,
/// batch_submitter_address) tuple on first successful boot, then refuses to
/// run under a different identity to prevent silently associating state from
/// one deployment with another.
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
    /// boxing this variant alone would push `RunError`'s stack footprint past
    /// 184 bytes, which clippy's `result_large_err` flags (and which inflates
    /// every `Result<_, RunError>` in the codebase, even successful returns).
    /// The heap allocation is paid only on the error path, which is cold.
    #[error("deployment identity mismatch ({fields}); stored={stored:?}; expected={expected:?}")]
    Mismatch {
        fields: String,
        stored: Box<DeploymentIdentity>,
        expected: Box<DeploymentIdentity>,
    },
}

// ── Worker exits ───────────────────────────────────────────────────────

/// Which runtime worker exited, and why.
#[derive(Debug, Error)]
pub enum WorkerExit {
    #[error("server: {0}")]
    Server(#[from] ServerExit),
    #[error("inclusion lane: {0}")]
    Lane(#[from] LaneExit),
    #[error("input reader: {0}")]
    InputReader(#[from] InputReaderExit),
    #[error("batch submitter: {0}")]
    BatchSubmitter(#[from] BatchSubmitterExit),
    #[error("danger detector: {0}")]
    DangerDetector(#[from] DangerDetectorExit),
    #[error("fee oracle: {0}")]
    FeeOracle(#[from] FeeOracleExit),
}

/// Generic worker exit shape: stopped without signal / errored / failed to join.
#[derive(Debug, Error)]
pub enum ServerExit {
    #[error("stopped unexpectedly")]
    StoppedUnexpectedly,
    #[error("io error: {0}")]
    Source(std::io::Error),
    #[error("join error: {0}")]
    Join(tokio::task::JoinError),
}

#[derive(Debug, Error)]
pub enum LaneExit {
    #[error("stopped unexpectedly")]
    StoppedUnexpectedly,
    #[error("{0}")]
    Source(InclusionLaneError),
    #[error("join error: {0}")]
    Join(tokio::task::JoinError),
}

#[derive(Debug, Error)]
pub enum InputReaderExit {
    #[error("stopped unexpectedly")]
    StoppedUnexpectedly,
    #[error("{0}")]
    Source(InputReaderError),
    #[error("join error: {0}")]
    Join(tokio::task::JoinError),
}

#[derive(Debug, Error)]
pub enum BatchSubmitterExit {
    #[error("stopped unexpectedly")]
    StoppedUnexpectedly,
    #[error("{0}")]
    Source(BatchSubmitterError),
    #[error("join error: {0}")]
    Join(tokio::task::JoinError),
}

/// Detector has an extra variant for the deliberate `RecoveryRequired` trip:
/// not an error per se, but causes the runtime to exit so the orchestrator
/// can respawn into startup recovery.
#[derive(Debug, Error)]
pub enum DangerDetectorExit {
    #[error("stopped unexpectedly")]
    StoppedUnexpectedly,
    #[error("{0}")]
    Source(DangerDetectorError),
    #[error("join error: {0}")]
    Join(tokio::task::JoinError),
    #[error("danger detected ({status:?}) — stopping for startup recovery")]
    DangerDetected { status: DangerStatus },
}

#[derive(Debug, Error)]
pub enum FeeOracleExit {
    #[error("stopped unexpectedly")]
    StoppedUnexpectedly,
    #[error("{0}")]
    Source(FeeOracleError),
    #[error("join error: {0}")]
    Join(tokio::task::JoinError),
}

// ── Shutdown-time constructors ────────────────────────────────────────
//
// Used during orderly shutdown (runtime-wide shutdown was already
// requested). `Ok(())` is the expected "drained cleanly" outcome and
// returns `Ok(())`; everything else surfaces as the matching error variant.
// Distinct from the select-arm `From` impls, where `Ok(())` means the worker
// stopped *before* shutdown was triggered (`StoppedUnexpectedly`).

impl ServerExit {
    pub fn from_shutdown(
        result: Result<std::io::Result<()>, tokio::task::JoinError>,
    ) -> Result<(), Self> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(source)) => Err(Self::Source(source)),
            Err(source) => Err(Self::Join(source)),
        }
    }
}

impl LaneExit {
    pub fn from_shutdown(
        result: Result<Result<(), InclusionLaneError>, tokio::task::JoinError>,
    ) -> Result<(), Self> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(source)) => Err(Self::Source(source)),
            Err(source) => Err(Self::Join(source)),
        }
    }
}

impl InputReaderExit {
    pub fn from_shutdown(
        result: Result<Result<(), InputReaderError>, tokio::task::JoinError>,
    ) -> Result<(), Self> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(source)) => Err(Self::Source(source)),
            Err(source) => Err(Self::Join(source)),
        }
    }
}

impl BatchSubmitterExit {
    pub fn from_shutdown(
        result: Result<
            Result<crate::l1::submitter::SubmitterExit, BatchSubmitterError>,
            tokio::task::JoinError,
        >,
    ) -> Result<(), Self> {
        match result {
            Ok(Ok(crate::l1::submitter::SubmitterExit::Shutdown)) => Ok(()),
            Ok(Err(source)) => Err(Self::Source(source)),
            Err(source) => Err(Self::Join(source)),
        }
    }
}

impl DangerDetectorExit {
    pub fn from_shutdown(
        result: Result<
            Result<crate::recovery::DetectorExit, DangerDetectorError>,
            tokio::task::JoinError,
        >,
    ) -> Result<(), Self> {
        match result {
            Ok(Ok(crate::recovery::DetectorExit::Shutdown)) => Ok(()),
            Ok(Ok(crate::recovery::DetectorExit::RecoveryRequired { status })) => {
                Err(Self::DangerDetected { status })
            }
            Ok(Err(source)) => Err(Self::Source(source)),
            Err(source) => Err(Self::Join(source)),
        }
    }
}

impl FeeOracleExit {
    pub fn from_shutdown(
        result: Result<Result<(), FeeOracleError>, tokio::task::JoinError>,
    ) -> Result<(), Self> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(source)) => Err(Self::Source(source)),
            Err(source) => Err(Self::Join(source)),
        }
    }
}

// ── Chained `From` impls so `?` works at the top-level RunError ────────
//
// thiserror's `#[from]` is one-level; nested propagation needs manual
// impls. Each leaf error type that can bubble up through `?` in `run()`
// gets a direct From<Leaf> for RunError.

impl From<StorageOpenError> for RunError {
    fn from(e: StorageOpenError) -> Self {
        RunError::Bootstrap(e.into())
    }
}

impl From<ProtocolTimingError> for RunError {
    fn from(e: ProtocolTimingError) -> Self {
        RunError::Bootstrap(e.into())
    }
}

impl From<RecoveryError> for RunError {
    fn from(e: RecoveryError) -> Self {
        RunError::Bootstrap(e.into())
    }
}

impl From<IdentityError> for RunError {
    fn from(e: IdentityError) -> Self {
        RunError::Bootstrap(e.into())
    }
}

impl From<crate::recovery::FlushError> for RunError {
    fn from(e: crate::recovery::FlushError) -> Self {
        RunError::Bootstrap(BootstrapError::Flush(e))
    }
}

impl From<SetupRefuse> for RunError {
    fn from(e: SetupRefuse) -> Self {
        RunError::Bootstrap(BootstrapError::SetupRefuse(e))
    }
}

impl From<SetupRecoveryError> for RunError {
    fn from(e: SetupRecoveryError) -> Self {
        RunError::Bootstrap(BootstrapError::SetupRecovery(e))
    }
}

impl From<FeeOracleError> for RunError {
    fn from(error: FeeOracleError) -> Self {
        match error {
            FeeOracleError::OpenStorage(error) => RunError::from(error),
            FeeOracleError::Transient(message) => {
                RunError::Bootstrap(BootstrapError::FeeOracleTransient { message })
            }
            error => RunError::Bootstrap(BootstrapError::FeeOracleMisconfig {
                message: error.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l1::fee_oracle::worker::FeeOracleError;
    use crate::recovery::{FlushError, RecoveryError};
    use crate::storage::DeploymentIdentity;
    use sequencer_core::protocol::ProtocolTimingError;

    fn danger(status: DangerStatus) -> RunError {
        RunError::Worker(WorkerExit::DangerDetector(
            DangerDetectorExit::DangerDetected { status },
        ))
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
            RunError::Bootstrap(BootstrapError::Recovery(RecoveryError::Refuse(
                RefuseReason::L1ViewStale
            )))
            .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::Identity(IdentityError::FirstBootRequiresL1))
                .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::ChainIdRpc {
                message: "x".into()
            })
            .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            RunError::from(FlushError::Provider("x".into())).exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        // A flush failure surfaced via startup recovery must land in the same
        // class as the flush-mempool subcommand's FlushError (review M2).
        assert_eq!(
            RunError::Bootstrap(BootstrapError::Recovery(RecoveryError::Flush(
                FlushError::Provider("x".into())
            )))
            .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::Recovery(
                RecoveryError::ResyncBehindFlushView {
                    resynced_safe_block: 1,
                    flush_observed_safe_block: 2,
                }
            ))
            .exit_code(),
            EXIT_RESTART_TRANSIENT
        );
    }

    #[test]
    fn r4_class_30_terminal_operator_required() {
        assert_eq!(
            danger(DangerStatus::CanonicalDivergence(0)).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::Recovery(RecoveryError::Refuse(
                RefuseReason::CanonicalDivergence { nonce: 0 }
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::SetupNotComplete).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::ChainIdMismatch { rpc: 1, config: 2 }).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::InvalidProtocolTiming(
                ProtocolTimingError::MarginNotLessThanMaxWait {
                    margin: 1200,
                    max_wait: 1200
                }
            ))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::Identity(IdentityError::OrphanedState)).exit_code(),
            EXIT_TERMINAL
        );
        // `setup --recovery` operator-fixable failures are terminal (a restart
        // re-runs the same bad inputs).
        assert_eq!(
            RunError::from(SetupRecoveryError::AlreadySetUp).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch {
                fields: "chain_id".into(),
                stored: Box::new(dummy_identity()),
                expected: Box::new(dummy_identity()),
            }))
            .exit_code(),
            EXIT_TERMINAL
        );
        // Partial-recovery residue (B1/B2): operator must wipe — terminal.
        assert_eq!(
            RunError::from(SetupRecoveryError::PartialRecoveryMismatch {
                existing_root_nonce: 3,
                requested_nonce: 5,
            })
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::from(SetupRecoveryError::GenesisOverRecoveryResidue { anchor: 7 })
                .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::from(SetupRecoveryError::PartialRecoveryIncomplete { root_nonce: 3 })
                .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::from(SetupRecoveryError::RecoveryOverResidualSnapshot {
                existing_finalized_block: 0,
            })
            .exit_code(),
            EXIT_TERMINAL
        );
        // Reader-level chain-id mismatch (warm-boot backstop) is terminal, like
        // the boot-time BootstrapError::ChainIdMismatch.
        assert_eq!(
            RunError::Worker(WorkerExit::InputReader(InputReaderExit::Source(
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
            RunError::Bootstrap(BootstrapError::Recovery(RecoveryError::InputReader(
                InputReaderError::ChainIdMismatch {
                    rpc: 1,
                    expected: 31337,
                }
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
    }

    #[test]
    fn r4_class_40_setup_needs_operator_recovery() {
        // Sticky setup refusals: operator must run `setup --recovery`, not a
        // plain restart (which would re-detect and re-refuse) — so they get a
        // dedicated code, distinct from the auto-recovery class (10).
        assert_eq!(
            RunError::from(SetupRefuse::WalletNonceUnsettled {
                pending: 14,
                safe: 13,
            })
            .exit_code(),
            EXIT_SETUP_NEEDS_RECOVERY
        );
        assert_eq!(
            RunError::from(SetupRefuse::BatchPastCheckpoint {
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
            RunError::Bootstrap(BootstrapError::CheckpointBeforeAppDeployment {
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
            RunError::Io(std::io::Error::other("boom")).exit_code(),
            EXIT_UNCLASSIFIED
        );
        assert_eq!(
            RunError::Worker(WorkerExit::Server(ServerExit::StoppedUnexpectedly)).exit_code(),
            EXIT_UNCLASSIFIED
        );
        assert_eq!(
            RunError::from(FeeOracleError::Transient("RPC unavailable".into())).exit_code(),
            EXIT_UNCLASSIFIED
        );
        assert_eq!(
            RunError::Worker(WorkerExit::FeeOracle(FeeOracleExit::Source(
                FeeOracleError::Transient("RPC unavailable".into()),
            )))
            .exit_code(),
            EXIT_UNCLASSIFIED
        );
    }

    #[test]
    fn fee_oracle_fatal_math_is_terminal_on_bootstrap_and_worker() {
        use crate::l1::fee_oracle::math::MathError;
        assert_eq!(
            RunError::from(FeeOracleError::FatalMath(MathError::Overflow)).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Worker(WorkerExit::FeeOracle(FeeOracleExit::Source(
                FeeOracleError::FatalMath(MathError::Overflow),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Worker(WorkerExit::FeeOracle(FeeOracleExit::Source(
                FeeOracleError::FatalMath(MathError::ExceedsRepresentableRange),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::from(FeeOracleError::Misconfig("wrong pair".into())).exit_code(),
            EXIT_TERMINAL
        );
        assert_eq!(
            RunError::Worker(WorkerExit::FeeOracle(FeeOracleExit::Source(
                FeeOracleError::Misconfig("wrong pair".into()),
            )))
            .exit_code(),
            EXIT_TERMINAL
        );
    }

    #[test]
    fn fee_oracle_shutdown_ok_is_graceful() {
        let result: Result<Result<(), FeeOracleError>, tokio::task::JoinError> = Ok(Ok(()));
        assert!(FeeOracleExit::from_shutdown(result).is_ok());
    }
}
