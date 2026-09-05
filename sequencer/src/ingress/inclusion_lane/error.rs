// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Lane-level error types. Returned from the lane's join handle; the runtime
//! logs them and may shut down depending on severity.

use sequencer_core::application::AppError;
use thiserror::Error;

use super::dump_info::CreateDumpDirError;
use super::snapshot::{GcError, StampError, TakeDumpError};

#[derive(Debug, Error)]
pub enum InclusionLaneError {
    #[error("inclusion lane input channel closed")]
    ChannelClosed,
    #[error("application catchup failed")]
    CatchUp {
        #[source]
        source: CatchUpError,
    },
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("terminal storage invariant failure requested runtime shutdown")]
    TerminalStorageInvariant,
    #[error(
        "canonical divergence at batch nonce {nonce}, safe-input index {safe_input_index}; \
         cockroach recovery required"
    )]
    CanonicalDivergence { nonce: u64, safe_input_index: u64 },
    #[error("user op execution failed")]
    ExecuteUserOp {
        #[source]
        source: AppError,
    },
    #[error("direct input execution failed")]
    ExecuteDirectInput {
        #[source]
        source: AppError,
    },
    #[error("snapshot at batch close failed")]
    Snapshot(#[from] TakeDumpError),
    #[error("loading Application from finalized snapshot failed: {0}")]
    LoadFromDump(AppError),
    #[error("snapshot garbage collection failed")]
    Gc(#[from] GcError),
    #[error("stamping promotion metadata into the finalized dump failed")]
    PromotionStamp(#[from] StampError),
    #[error(
        "no open Tip at lane startup; the runtime must establish it via \
         the recovery reducer's EnsureOpenTip phase before starting the lane"
    )]
    NoOpenTip,
}

impl InclusionLaneError {
    pub(crate) fn is_terminal_invariant(&self) -> bool {
        match self {
            Self::Storage(source) => crate::storage::is_persistent_storage_error(source),
            Self::CatchUp { source } => source.is_terminal_invariant(),
            Self::ExecuteUserOp { source } | Self::ExecuteDirectInput { source } => {
                app_error_is_terminal(source)
            }
            Self::LoadFromDump(source) => referenced_snapshot_app_error_is_terminal(source),
            Self::Snapshot(source) => take_dump_error_is_terminal(source),
            Self::Gc(GcError::Storage(source))
            | Self::PromotionStamp(StampError::Storage(source)) => {
                crate::storage::is_persistent_storage_error(source)
            }
            Self::TerminalStorageInvariant | Self::CanonicalDivergence { .. } | Self::NoOpenTip => {
                true
            }
            Self::ChannelClosed | Self::PromotionStamp(StampError::Io(_)) => false,
        }
    }
}

fn app_error_is_terminal(source: &AppError) -> bool {
    matches!(source, AppError::Internal { .. })
}

fn referenced_snapshot_app_error_is_terminal(source: &AppError) -> bool {
    match source {
        AppError::Internal { .. } => true,
        AppError::Io(source) => super::dump_info::referenced_artifact_io_is_terminal(source),
    }
}

fn take_dump_error_is_terminal(source: &TakeDumpError) -> bool {
    match source {
        TakeDumpError::Storage(source) => crate::storage::is_persistent_storage_error(source),
        TakeDumpError::CreateDump(CreateDumpDirError::App(source)) => app_error_is_terminal(source),
        TakeDumpError::CreateDump(CreateDumpDirError::Io(_)) => false,
    }
}

#[derive(Debug, Error)]
pub enum CatchUpError {
    #[error("cannot load resume snapshot")]
    LoadSnapshot {
        #[source]
        source: rusqlite::Error,
    },
    #[error("cannot load replay entries from offset {offset}")]
    LoadReplay {
        offset: u64,
        #[source]
        source: rusqlite::Error,
    },
    #[error("replay user op failed: {source}")]
    ReplayUserOp {
        #[source]
        source: AppError,
    },
    #[error("replay direct input failed: {source}")]
    ReplayDirectInput {
        #[source]
        source: AppError,
    },
    #[error("snapshot executed-input count mismatch: application={application}, storage={storage}")]
    SnapshotExecutionCountMismatch { application: u64, storage: u64 },
    #[error(
        "physical replay row {db_offset} ({kind}) has execution offset {stored:?}, expected {expected:?}"
    )]
    ExecutionOffsetMismatch {
        db_offset: u64,
        kind: &'static str,
        expected: Option<u64>,
        stored: Option<u64>,
    },
    #[error(
        "no snapshot registered before lane catch-up; \
         runtime must ensure a genesis dump exists at first startup"
    )]
    NoSnapshot,
}

impl CatchUpError {
    fn is_terminal_invariant(&self) -> bool {
        match self {
            Self::LoadSnapshot { source } | Self::LoadReplay { source, .. } => {
                crate::storage::is_persistent_storage_error(source)
            }
            Self::ReplayUserOp { source } | Self::ReplayDirectInput { source } => {
                app_error_is_terminal(source)
            }
            Self::NoSnapshot
            | Self::SnapshotExecutionCountMismatch { .. }
            | Self::ExecutionOffsetMismatch { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_internal() -> AppError {
        AppError::Internal {
            reason: "application invariant failed".into(),
        }
    }

    fn app_io() -> AppError {
        AppError::Io(std::io::Error::other("filesystem unavailable"))
    }

    fn app_io_kind(kind: std::io::ErrorKind) -> AppError {
        AppError::Io(std::io::Error::from(kind))
    }

    #[test]
    fn application_internal_errors_are_terminal_through_lane_wrappers() {
        let errors = [
            InclusionLaneError::ExecuteUserOp {
                source: app_internal(),
            },
            InclusionLaneError::ExecuteDirectInput {
                source: app_internal(),
            },
            InclusionLaneError::LoadFromDump(app_internal()),
            InclusionLaneError::CatchUp {
                source: CatchUpError::ReplayUserOp {
                    source: app_internal(),
                },
            },
            InclusionLaneError::CatchUp {
                source: CatchUpError::ReplayDirectInput {
                    source: app_internal(),
                },
            },
            InclusionLaneError::Snapshot(TakeDumpError::CreateDump(CreateDumpDirError::App(
                app_internal(),
            ))),
        ];

        for error in errors {
            assert!(error.is_terminal_invariant(), "{error}");
        }
    }

    #[test]
    fn application_and_filesystem_io_errors_remain_operational() {
        let errors = [
            InclusionLaneError::ExecuteUserOp { source: app_io() },
            InclusionLaneError::ExecuteDirectInput { source: app_io() },
            InclusionLaneError::LoadFromDump(app_io()),
            InclusionLaneError::CatchUp {
                source: CatchUpError::ReplayUserOp { source: app_io() },
            },
            InclusionLaneError::CatchUp {
                source: CatchUpError::ReplayDirectInput { source: app_io() },
            },
            InclusionLaneError::Snapshot(TakeDumpError::CreateDump(CreateDumpDirError::App(
                app_io(),
            ))),
            InclusionLaneError::Snapshot(TakeDumpError::CreateDump(CreateDumpDirError::Io(
                std::io::Error::other("dump directory unavailable"),
            ))),
            InclusionLaneError::PromotionStamp(StampError::Io(std::io::Error::other(
                "metadata unavailable",
            ))),
        ];

        for error in errors {
            assert!(!error.is_terminal_invariant(), "{error}");
        }
    }

    #[test]
    fn missing_or_corrupt_referenced_snapshot_is_terminal() {
        for source in [
            app_io_kind(std::io::ErrorKind::NotFound),
            app_io_kind(std::io::ErrorKind::InvalidData),
            app_io_kind(std::io::ErrorKind::UnexpectedEof),
        ] {
            let error = InclusionLaneError::LoadFromDump(source);
            assert!(error.is_terminal_invariant(), "{error}");
        }
    }

    #[test]
    fn persistent_snapshot_storage_errors_are_terminal() {
        let errors = [
            InclusionLaneError::Snapshot(TakeDumpError::Storage(
                rusqlite::Error::QueryReturnedNoRows,
            )),
            InclusionLaneError::Gc(GcError::Storage(rusqlite::Error::QueryReturnedNoRows)),
            InclusionLaneError::PromotionStamp(StampError::Storage(
                rusqlite::Error::QueryReturnedNoRows,
            )),
        ];

        for error in errors {
            assert!(error.is_terminal_invariant(), "{error}");
        }
    }
}
