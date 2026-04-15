// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Lane-level error types. Returned from the lane's join handle; the runtime
//! logs them and may shut down depending on severity.

use sequencer_core::application::AppError;
use thiserror::Error;

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
    #[error("direct input execution failed")]
    ExecuteDirectInput {
        #[source]
        source: AppError,
    },
}

#[derive(Debug, Error)]
pub enum CatchUpError {
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
}
