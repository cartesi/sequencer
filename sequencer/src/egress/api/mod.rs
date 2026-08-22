// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Egress HTTP API routes: WebSocket subscribe + k8s-style health probes.
//! Additional read endpoints will land here.

mod health;
mod snapshot;
mod state;
mod subscribe;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

use crate::runtime::shutdown::RuntimeScope;
use crate::storage::ReleaseScheduler;

pub(crate) use health::HealthState;
pub use snapshot::SnapshotState;
pub(crate) use state::SubscribeState;

/// Build the egress router. Each subrouter has its own state; the merge is
/// transparent to axum's routing. Snapshot routes are always part of the
/// egress (internal) side — the public/ingress side is a separate router.
pub(crate) fn router(
    subscribe_state: Arc<SubscribeState>,
    health_state: Arc<HealthState>,
    snapshot_state: SnapshotState,
    shutdown: RuntimeScope,
    snapshot_release_scheduler: ReleaseScheduler,
) -> Router {
    let subscribe_router = Router::new()
        .route("/ws/subscribe", get(subscribe::subscribe_l2_txs))
        .with_state(subscribe_state);

    let health_router = Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .route("/healthz", get(health::healthz))
        .with_state(health_state);

    subscribe_router
        .merge(health_router)
        .merge(snapshot::router(
            snapshot_state,
            shutdown,
            snapshot_release_scheduler,
        ))
}
