// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared axum state. Fields are partitioned by endpoint (tx-only, ws-only,
//! shared) — this partition is what makes the future tx/ws split mechanical.
//! Adding a new field that's used by both endpoints is fine; adding one that
//! couples them is the bit to think twice about.

use std::sync::Arc;

use alloy_sol_types::Eip712Domain;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::{ApiConfig, ApiError};
use crate::inclusion_lane::PendingUserOp;
use crate::l2_tx_feed::L2TxFeed;
use crate::shutdown::ShutdownSignal;

#[derive(Clone)]
pub(super) struct ApiState {
    // ── tx-only ────────────────────────────────────────────────────────
    pub tx_sender: mpsc::Sender<PendingUserOp>,
    pub domain: Eip712Domain,
    pub max_user_op_data_bytes: usize,

    // ── shared ─────────────────────────────────────────────────────────
    pub shutdown: ShutdownSignal,

    // ── ws-only ────────────────────────────────────────────────────────
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
