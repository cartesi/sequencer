// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

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
    #[error("cannot load next undrained safe-input index")]
    LoadNextUndrainedDirectInputIndex {
        #[source]
        source: rusqlite::Error,
    },
    #[error("cannot load safe inputs")]
    LoadSafeInputs {
        #[source]
        source: rusqlite::Error,
    },
    #[error("cannot load/create open batch/frame")]
    LoadOpenState {
        #[source]
        source: rusqlite::Error,
    },
    #[error("append user ops failed")]
    AppendUserOps {
        #[source]
        source: rusqlite::Error,
    },
    #[error("direct input execution failed")]
    ExecuteDirectInput {
        #[source]
        source: AppError,
    },
    #[error("failed to close/rotate frame")]
    CloseFrameRotate {
        #[source]
        source: rusqlite::Error,
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
