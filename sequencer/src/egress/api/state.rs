// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Egress-side axum state for WS subscribe and health probes.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::egress::l2_tx_feed::L2TxFeed;
use crate::http::ApiError;
use crate::runtime::shutdown::RuntimeScope;

#[derive(Clone)]
pub(crate) struct SubscribeState {
    pub shutdown: RuntimeScope,
    pub ws_subscriber_limit: Arc<Semaphore>,
    pub ws_max_catchup_events: u64,
    pub tx_feed: L2TxFeed,
}

impl SubscribeState {
    pub(crate) fn new(
        shutdown: RuntimeScope,
        tx_feed: L2TxFeed,
        ws_max_subscribers: usize,
        ws_max_catchup_events: u64,
    ) -> Self {
        Self {
            shutdown,
            ws_subscriber_limit: Arc::new(Semaphore::new(ws_max_subscribers)),
            ws_max_catchup_events,
            tx_feed,
        }
    }

    pub(crate) fn reject_if_shutting_down(&self) -> Result<(), ApiError> {
        if self.shutdown.is_shutdown_requested() {
            Err(ApiError::unavailable("sequencer shutting down"))
        } else {
            Ok(())
        }
    }

    pub(crate) fn try_acquire_ws_subscriber_permit(
        &self,
    ) -> Result<OwnedSemaphorePermit, ApiError> {
        self.ws_subscriber_limit
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::overloaded("ws subscriber limit reached"))
    }
}
