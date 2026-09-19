// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use sequencer_core::history::HistoryPolicyError;
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
    #[error("subscription preparation task join error: {source}")]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error(transparent)]
    History(#[from] HistoryPolicyError),
}

impl From<crate::storage::HistoryReadError> for SubscribeError {
    fn from(error: crate::storage::HistoryReadError) -> Self {
        match error {
            crate::storage::HistoryReadError::Policy(error) => Self::History(error),
            crate::storage::HistoryReadError::Storage(source) => Self::LoadHeadOffset { source },
        }
    }
}

impl SubscribeError {
    pub(super) fn is_persistent_storage_invariant(&self) -> bool {
        match self {
            Self::OpenStorage { source } => open_error_is_persistent(source),
            Self::LoadHeadOffset { source } => is_persistent_storage_error(source),
            Self::Join { source } => source.is_panic(),
            Self::History(_) => false,
        }
    }
}

#[derive(Debug, Error)]
pub enum SubscriptionError {
    #[error(transparent)]
    History(HistoryPolicyError),
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
    #[error("subscription task join error: {source}")]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },
}

impl SubscriptionError {
    pub(super) fn is_persistent_storage_invariant(&self) -> bool {
        match self {
            Self::History(_) => false,
            Self::OpenStorage { source } => open_error_is_persistent(source),
            Self::LoadReplay { source, .. } => is_persistent_storage_error(source),
            Self::Join { source } => source.is_panic(),
        }
    }
}

fn open_error_is_persistent(error: &StorageOpenError) -> bool {
    is_persistent_storage_open_error(error)
}
