// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Health probes `/livez`, `/readyz` and `/healthz` (contract: README "Health
//! probes"). They live on the internal listener because operators and kubelet
//! probe from inside the cluster. `/healthz` also carries the landing watch's
//! latest reading for monitoring; it never changes the status code.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::ingress::inclusion_lane::PendingUserOp;
use crate::landing::LandingFeed;
use crate::runtime::shutdown::RuntimeScope;

/// Narrow health-check state. Holds only the signals the probes inspect; the
/// `tx_sender` is a clone of the inclusion-lane channel and is closed iff the
/// lane has dropped its receiver.
#[derive(Clone)]
pub(crate) struct HealthState {
    pub tx_sender: mpsc::Sender<PendingUserOp>,
    pub shutdown: RuntimeScope,
    /// The landing watch's latest reading.
    pub landing: LandingFeed,
}

#[derive(Serialize)]
struct HealthStatus {
    status: &'static str,
    inclusion_lane: &'static str,
    /// The oldest unaccepted batch's age, in blocks, at the earliest block it
    /// can still land in (`H + 1 - s`); the history is void once it reaches
    /// `max_wait_blocks`. `null` when every batch is accepted or before the
    /// first reading.
    landing_lag_blocks: Option<u64>,
    /// The application's staleness window, pinned at setup.
    max_wait_blocks: u64,
    /// Wall clock minus the safe block's L1 timestamp, saturating at 0;
    /// `null` before the first reading.
    safe_head_age_seconds: Option<u64>,
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

    let reading = *state.landing.borrow();
    let body = HealthStatus {
        status: if all_ok { "ok" } else { "degraded" },
        inclusion_lane: if lane_ok { "ok" } else { "stopped" },
        landing_lag_blocks: reading.landing_lag_blocks(),
        max_wait_blocks: reading.max_wait_blocks,
        safe_head_age_seconds: reading.safe_head_age_seconds(crate::clock::unix_now_ms()),
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
    use crate::landing::LandingReading;
    use crate::storage::{BatchId, LandingLag};

    fn fresh_state() -> (Arc<HealthState>, mpsc::Receiver<PendingUserOp>) {
        let (tx_sender, rx) = mpsc::channel::<PendingUserOp>(1);
        let state = Arc::new(HealthState {
            tx_sender,
            shutdown: RuntimeScope::default(),
            landing: crate::landing::test_feed(1200),
        });
        (state, rx)
    }

    async fn body_of(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read health body");
        serde_json::from_slice(&bytes).expect("decode health body")
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

    /// Before the first reading only the window is known.
    #[tokio::test]
    async fn healthz_reports_the_window_and_no_lag_before_the_first_reading() {
        let (state, _rx) = fresh_state();
        let response = healthz(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_of(response).await,
            serde_json::json!({
                "status": "ok",
                "inclusion_lane": "ok",
                "landing_lag_blocks": null,
                "max_wait_blocks": 1200,
                "safe_head_age_seconds": null,
            })
        );
    }

    /// The published reading reaches the body, and a lag past the window does
    /// not change the status code: the landing watch, not the probe, stops
    /// the process.
    #[tokio::test]
    async fn healthz_reports_the_published_landing_reading() {
        let (tx_sender, _rx) = mpsc::channel::<PendingUserOp>(1);
        let (publish, landing) = tokio::sync::watch::channel(LandingReading {
            lag: None,
            max_wait_blocks: 1200,
            safe_block_timestamp: None,
        });
        let state = Arc::new(HealthState {
            tx_sender,
            shutdown: RuntimeScope::default(),
            landing,
        });
        let now_secs = crate::clock::unix_now_ms() / 1000;
        publish.send_replace(LandingReading {
            lag: Some(LandingLag {
                batch: BatchId::from_raw(4),
                first_frame_safe_block: 100,
                safe_block: 1400,
            }),
            max_wait_blocks: 1200,
            // An hour old, so the age is not at the 0 floor.
            safe_block_timestamp: Some(now_secs - 3600),
        });
        let response = healthz(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert_eq!(body["landing_lag_blocks"], 1301);
        assert_eq!(body["max_wait_blocks"], 1200);
        let age = body["safe_head_age_seconds"].as_u64().expect("an age");
        assert!((3600..3700).contains(&age), "age {age}");

        // A safe block stamped ahead of the local clock reads as age 0.
        publish.send_replace(LandingReading {
            lag: None,
            max_wait_blocks: 1200,
            safe_block_timestamp: Some(now_secs + 600),
        });
        let body = body_of(healthz(State(state)).await.into_response()).await;
        assert_eq!(body["landing_lag_blocks"], serde_json::Value::Null);
        assert_eq!(body["safe_head_age_seconds"], 0);
    }
}
