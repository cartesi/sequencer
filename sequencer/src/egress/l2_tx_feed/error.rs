// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use thiserror::Error;

use crate::storage::StorageOpenError;

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
    #[error("cannot resolve executed input count subscription cursor")]
    LoadExecutedInputCountCursor {
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "requested executed input count {requested_executed_input_count} predates feed anchor {minimum_executed_input_count}"
    )]
    ExecutedInputCountBeforeAnchor {
        requested_executed_input_count: u64,
        minimum_executed_input_count: u64,
    },
    #[error(
        "catch-up window exceeded: requested offset {requested_offset}, live start {live_start_offset}, max {max_catchup_events}"
    )]
    CatchUpWindowExceeded {
        requested_offset: u64,
        live_start_offset: u64,
        max_catchup_events: u64,
    },
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
    #[error("subscription task join error: {source}")]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },
}
