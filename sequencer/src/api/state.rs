// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::sync::Arc;

use alloy_sol_types::Eip712Domain;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::{ApiConfig, ApiError};
use crate::inclusion_lane::PendingUserOp;
use crate::l2_tx_feed::L2TxFeed;
use crate::shutdown::ShutdownSignal;

#[derive(Clone)]
pub(super) struct ApiState {
    pub tx_sender: mpsc::Sender<PendingUserOp>,
    pub domain: Eip712Domain,
    pub max_user_op_data_bytes: usize,
    pub shutdown: ShutdownSignal,
    pub ws_subscriber_limit: Arc<Semaphore>,
    pub ws_max_catchup_events: u64,
    pub tx_feed: L2TxFeed,
}

impl ApiState {
    pub(super) fn new(
        tx_sender: mpsc::Sender<PendingUserOp>,
        domain: Eip712Domain,
        max_user_op_data_bytes: usize,
        shutdown: ShutdownSignal,
        tx_feed: L2TxFeed,
        config: ApiConfig,
    ) -> Self {
        Self {
            tx_sender,
            domain,
            max_user_op_data_bytes,
            shutdown,
            ws_subscriber_limit: Arc::new(Semaphore::new(config.ws_max_subscribers)),
            ws_max_catchup_events: config.ws_max_catchup_events,
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
