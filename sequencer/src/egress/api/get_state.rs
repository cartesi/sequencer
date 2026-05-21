// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::egress::api::GetStateState;
use crate::egress::app_state::StateSnapshotError;
use crate::http::ApiError;

#[derive(Debug, Serialize)]
pub(crate) struct GetStateResponse {
    safe_block: u64,
    state: String,
}

pub(crate) async fn get_state(
    State(state): State<GetStateState>,
) -> Result<Json<GetStateResponse>, ApiError> {
    state.reject_if_shutting_down()?;
    let snapshotter = state.snapshotter.clone();
    let snapshot = tokio::task::spawn_blocking(move || snapshotter.snapshot())
        .await
        .map_err(|err| ApiError::internal_error(format!("state snapshot task failed: {err}")))?
        .map_err(map_snapshot_error)?;

    Ok(Json(GetStateResponse {
        safe_block: snapshot.safe_block,
        state: snapshot.state,
    }))
}

fn map_snapshot_error(err: StateSnapshotError) -> ApiError {
    match err {
        StateSnapshotError::SafeHeadUnavailable => ApiError::unavailable(err.to_string()),
        StateSnapshotError::Storage { .. }
        | StateSnapshotError::OpenStorage { .. }
        | StateSnapshotError::Application { .. } => ApiError::internal_error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::egress::app_state::{StateSnapshot, StateSnapshotProvider};
    use crate::runtime::shutdown::ShutdownSignal;

    struct FixedSnapshotter;

    impl StateSnapshotProvider for FixedSnapshotter {
        fn snapshot(&self) -> Result<StateSnapshot, StateSnapshotError> {
            Ok(StateSnapshot {
                safe_block: 42,
                state: "{\"ok\":true}".to_string(),
            })
        }
    }

    struct UnavailableSnapshotter;

    impl StateSnapshotProvider for UnavailableSnapshotter {
        fn snapshot(&self) -> Result<StateSnapshot, StateSnapshotError> {
            Err(StateSnapshotError::SafeHeadUnavailable)
        }
    }

    #[tokio::test]
    async fn get_state_returns_safe_block_and_raw_state() {
        let state = GetStateState {
            shutdown: ShutdownSignal::default(),
            snapshotter: Arc::new(FixedSnapshotter),
        };

        let response = get_state(State(state)).await.expect("get state");

        assert_eq!(response.safe_block, 42);
        assert_eq!(response.state, "{\"ok\":true}");
    }

    #[tokio::test]
    async fn get_state_rejects_shutdown() {
        let shutdown = ShutdownSignal::default();
        shutdown.request_shutdown();
        let state = GetStateState {
            shutdown,
            snapshotter: Arc::new(FixedSnapshotter),
        };

        let err = get_state(State(state)).await.expect_err("shutdown");

        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn get_state_reports_unavailable_when_safe_head_missing() {
        let state = GetStateState {
            shutdown: ShutdownSignal::default(),
            snapshotter: Arc::new(UnavailableSnapshotter),
        };

        let err = get_state(State(state))
            .await
            .expect_err("missing safe head");

        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
}
