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
use crate::l1::reader::InputReaderError;
use crate::l1::submitter::BatchSubmitterError;
use crate::recovery::{DangerDetectorError, RecoveryError};
use crate::storage::{DangerStatus, DeploymentIdentity, StorageOpenError};
use sequencer_core::protocol::ProtocolTimingError;

// ── Top-level RunError ────────────────────────────────────────────────

/// Top-level runtime error. Grouped by phase:
///
/// - `Bootstrap`: startup failures before runtime workers come up.
/// - `Worker`: one of the runtime workers exited (server, inclusion lane,
///   input reader, batch submitter, danger detector).
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

// ── Bootstrap-phase errors ─────────────────────────────────────────────

/// Anything that can go wrong before runtime workers start: config validation,
/// deployment-identity guards, startup recovery, initial DB open.
#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error("RPC chain ID {rpc} does not match --chain-id {config}")]
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
    /// Startup recovery (or refusal) failed before runtime workers started.
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
    /// Deployment-identity guards — see [`IdentityError`].
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

/// Deployment-identity failure modes. The sequencer pins itself to a specific
/// (chain_id, app_address, input_box_address, input_box_genesis_block,
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
