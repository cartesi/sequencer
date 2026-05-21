// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Egress HTTP API routes: WebSocket subscribe + k8s-style health probes.
//! Additional read endpoints will land here.

mod get_state;
mod health;
mod snapshot;
mod state;
mod subscribe;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

pub(crate) use health::HealthState;
pub use snapshot::SnapshotState;
pub(crate) use state::{GetStateState, SubscribeState};

/// Build the egress router. Each subrouter has its own state; the merge is
/// transparent to axum's routing. Snapshot routes are always part of the
/// egress (internal) side — the public/ingress side is a separate router.
pub(crate) fn router(
    subscribe_state: Arc<SubscribeState>,
    get_state_state: GetStateState,
    health_state: Arc<HealthState>,
    snapshot_state: Arc<SnapshotState>,
) -> Router {
    let subscribe_router = Router::new()
        .route("/ws/subscribe", get(subscribe::subscribe_l2_txs))
        .with_state(subscribe_state);

    let state_router = Router::new()
        .route("/get_state", get(get_state::get_state))
        .with_state(get_state_state);

    let health_router = Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .route("/healthz", get(health::healthz))
        .with_state(health_state);

    subscribe_router
        .merge(state_router)
        .merge(health_router)
        .merge(snapshot::router(snapshot_state))
}
