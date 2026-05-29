// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Lane-level error types. Returned from the lane's join handle; the runtime
//! logs them and may shut down depending on severity.

use sequencer_core::application::AppError;
use thiserror::Error;

use super::snapshot::{GcError, PromoteError, TakeDumpError};

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
    #[error("snapshot promotion failed")]
    Promote(#[from] PromoteError),
    #[error("loading Application from finalized snapshot failed: {0}")]
    LoadFromDump(AppError),
    #[error("snapshot garbage collection failed")]
    Gc(#[from] GcError),
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
    #[error("replay user op failed: {reason}")]
    ReplayUserOpInternal { reason: String },
    #[error("replay direct input failed: {reason}")]
    ReplayDirectInputInternal { reason: String },
    #[error(
        "no snapshot registered before lane catch-up; \
         runtime must ensure a genesis dump exists at first startup"
    )]
    NoSnapshot,
}
