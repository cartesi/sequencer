// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use thiserror::Error;

use crate::storage::{
    StorageOpenError, is_persistent_storage_error, is_persistent_storage_open_error,
};

#[derive(Debug, Error)]
pub enum SubscribeError {
    #[error("cannot open subscription storage")]
    OpenStorage {
        #[source]
        source: StorageOpenError,
    },
    #[error("cannot load feed head offset")]
    LoadHeadOffset {
        #[source]
        source: rusqlite::Error,
    },
    #[error("persistent storage invariant violation while preparing subscription")]
    StorageInvariantViolation,
    #[error(
        "catch-up window exceeded: requested offset {requested_offset}, live start {live_start_offset}, max {max_catchup_events}"
    )]
    CatchUpWindowExceeded {
        requested_offset: u64,
        live_start_offset: u64,
        max_catchup_events: u64,
    },
}

impl SubscribeError {
    pub(super) fn is_persistent_storage_invariant(&self) -> bool {
        match self {
            Self::OpenStorage { source } => open_error_is_persistent(source),
            Self::LoadHeadOffset { source } => is_persistent_storage_error(source),
            Self::StorageInvariantViolation => true,
            Self::CatchUpWindowExceeded { .. } => false,
        }
    }
}

#[derive(Debug, Error)]
pub enum SubscriptionError {
    #[error("cannot open subscription storage")]
    OpenStorage {
        #[source]
        source: StorageOpenError,
    },
    #[error("cannot load ordered tx page from offset {offset}")]
    LoadReplay {
        offset: u64,
        #[source]
        source: rusqlite::Error,
    },
    #[error("persistent storage invariant violation while reading subscription")]
    StorageInvariantViolation,
    #[error("subscription task join error: {source}")]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },
}

impl SubscriptionError {
    pub(super) fn is_persistent_storage_invariant(&self) -> bool {
        match self {
            Self::OpenStorage { source } => open_error_is_persistent(source),
            Self::LoadReplay { source, .. } => is_persistent_storage_error(source),
            Self::StorageInvariantViolation => true,
            Self::Join { source } => source.is_panic(),
        }
    }
}

fn open_error_is_persistent(error: &StorageOpenError) -> bool {
    is_persistent_storage_open_error(error)
}
