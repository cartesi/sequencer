// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Egress HTTP API routes. Today: just `/ws/subscribe`. Health checks (`/livez`,
//! `/readyz`, `/healthz`) and additional read endpoints will land here.

mod state;
mod subscribe;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

pub(crate) use state::SubscribeState;
pub(crate) use subscribe::subscribe_l2_txs;

/// Build the egress router. Caller wires it into an `axum::serve` listener.
pub(crate) fn router(state: Arc<SubscribeState>) -> Router {
    Router::new()
        .route("/ws/subscribe", get(subscribe_l2_txs))
        .with_state(state)
}
