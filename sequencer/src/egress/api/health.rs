// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Health probes (k8s-style):
//!
//! - `GET /livez`   — process is up. Always 200.
//! - `GET /readyz`  — ready to accept new transactions. 503 if shutdown is in
//!   progress or the inclusion lane has dropped its receiver.
//! - `GET /healthz` — JSON status report. 200 / 503 mirroring `/readyz`.
//!
//! Lives on egress because operators (and kubelet, in practice) probe from the
//! internal-cluster side.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::ingress::inclusion_lane::PendingUserOp;
use crate::runtime::shutdown::RuntimeScope;

/// Narrow health-check state. Holds only the signals the probes inspect; the
/// `tx_sender` is a clone of the inclusion-lane channel and is closed iff the
/// lane has dropped its receiver.
#[derive(Clone)]
pub(crate) struct HealthState {
    pub tx_sender: mpsc::Sender<PendingUserOp>,
    pub shutdown: RuntimeScope,
}

#[derive(Serialize)]
struct HealthStatus {
    status: &'static str,
    inclusion_lane: &'static str,
}

pub(crate) async fn livez() -> StatusCode {
    StatusCode::OK
}

pub(crate) async fn readyz(State(state): State<Arc<HealthState>>) -> StatusCode {
    if state.shutdown.is_shutdown_requested() || state.tx_sender.is_closed() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

pub(crate) async fn healthz(State(state): State<Arc<HealthState>>) -> impl IntoResponse {
    let lane_ok = !state.tx_sender.is_closed();
    let shutting_down = state.shutdown.is_shutdown_requested();
    let all_ok = lane_ok && !shutting_down;

    let body = HealthStatus {
        status: if all_ok { "ok" } else { "degraded" },
        inclusion_lane: if lane_ok { "ok" } else { "stopped" },
    };

    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_state() -> (Arc<HealthState>, mpsc::Receiver<PendingUserOp>) {
        let (tx_sender, rx) = mpsc::channel::<PendingUserOp>(1);
        let state = Arc::new(HealthState {
            tx_sender,
            shutdown: RuntimeScope::default(),
        });
        (state, rx)
    }

    #[tokio::test]
    async fn livez_is_always_ok() {
        assert_eq!(livez().await, StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_ok_when_lane_alive_and_not_shutting_down() {
        let (state, _rx) = fresh_state();
        assert_eq!(readyz(State(state)).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_unavailable_when_shutdown_requested() {
        let (state, _rx) = fresh_state();
        state.shutdown.request_shutdown();
        assert_eq!(readyz(State(state)).await, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readyz_is_unavailable_after_storage_invariant_failure() {
        let (state, _rx) = fresh_state();
        state
            .shutdown
            .contain_storage_invariant_failure("test fault");
        assert_eq!(readyz(State(state)).await, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readyz_is_unavailable_when_lane_dropped() {
        let (state, rx) = fresh_state();
        drop(rx);
        assert_eq!(readyz(State(state)).await, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_reports_lane_stopped_after_lane_drop() {
        let (state, rx) = fresh_state();
        drop(rx);
        let response = healthz(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
