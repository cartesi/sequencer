// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Coherent history metadata.

use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sequencer_core::history::{EraId, RecoveryGeneration};
use serde::Deserialize;

use crate::http::{ApiError, StorageTaskError, storage_task};
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::{HistoricalReadError, Storage};

struct HistoryState {
    db_path: String,
    shutdown: RuntimeScope,
}

pub(super) fn router(db_path: String, shutdown: RuntimeScope) -> Router {
    Router::new()
        .route("/history", get(history))
        .with_state(Arc::new(HistoryState { db_path, shutdown }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    era_id: Option<EraId>,
    from_generation: Option<RecoveryGeneration>,
}

async fn history(
    State(state): State<Arc<HistoryState>>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(error) => return ApiError::bad_request(error.body_text()).into_response(),
    };
    if query.from_generation.is_some() && query.era_id.is_none() {
        return ApiError::bad_request("from_generation requires era_id").into_response();
    }
    if let Err(error) = crate::http::reject_if_shutting_down(&state.shutdown) {
        return error.into_response();
    }
    let db_path = state.db_path.clone();
    match storage_task(state.shutdown.clone(), "read history metadata", move |_| {
        Ok(Storage::open_read_only(&db_path)?.history_info(query.era_id, query.from_generation)?)
    })
    .await
    {
        Ok(info) => Json(info).into_response(),
        Err(error) => read_error(error),
    }
}

fn read_error(error: StorageTaskError) -> Response {
    match error.downcast_ref::<HistoricalReadError>() {
        Some(HistoricalReadError::Policy(policy)) => {
            return (StatusCode::CONFLICT, Json(*policy)).into_response();
        }
        Some(HistoricalReadError::BadRequest(message)) => {
            return ApiError::bad_request(message.clone()).into_response();
        }
        _ => {}
    }
    tracing::warn!(%error, "history read unavailable");
    ApiError::unavailable("history read unavailable").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERA: &str = "11111111-1111-4111-8111-111111111111";

    fn state() -> Arc<HistoryState> {
        Arc::new(HistoryState {
            db_path: "unused: invalid queries do not access storage".to_owned(),
            shutdown: RuntimeScope::default(),
        })
    }

    #[tokio::test]
    async fn malformed_queries_return_bad_request_json_before_storage() {
        for query in [
            "?from_generation=0",
            &format!("?era_id={ERA}&from_generation=-1"),
            &format!("?era_id={ERA}&from_generation=invalid"),
            &format!("?era_id={ERA}&unrecognized=1"),
        ] {
            let uri = format!("/history{query}").parse().unwrap();
            let response = history(State(state()), Query::try_from_uri(&uri)).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(error["code"], "BAD_REQUEST");
        }
    }

    #[tokio::test]
    async fn shutdown_refuses_before_opening_storage() {
        let state = state();
        state.shutdown.request_shutdown();
        for query in [
            "".to_owned(),
            format!("?era_id={ERA}"),
            format!("?era_id={ERA}&from_generation=0"),
        ] {
            let uri = format!("/history{query}").parse().unwrap();
            let response = history(State(state.clone()), Query::try_from_uri(&uri)).await;
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{query}"
            );
        }
    }

    #[tokio::test]
    async fn history_reports_a_rebuilt_eras_base_and_rebuilt_end() {
        use crate::egress::test_era;

        let db = crate::test_support::temp_db("history-rebuilt-era");
        let mut storage =
            test_era::seed_rebuilt_era(&db.path, std::path::Path::new("/unused/baseline"));
        test_era::append_live_user_op(&mut storage, 2);
        drop(storage);
        let state = Arc::new(HistoryState {
            db_path: db.path.clone(),
            shutdown: RuntimeScope::default(),
        });
        let response = history(
            State(state),
            Query::try_from_uri(&"/history".parse().unwrap()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // The era starts at the base count; the head is past the queued
        // direct and the live op.
        assert_eq!(info["history"]["available_from"], test_era::BASE);
        assert_eq!(info["history"]["head"], test_era::REBUILT_END + 2);
        assert_eq!(
            info["baseline"],
            serde_json::json!({
                "l1_stop_block": test_era::STOP_BLOCK,
                "l1_end_input_index": test_era::FIRST_QUEUED_INPUT + 1,
                "next_batch_nonce": test_era::NEXT_BATCH_NONCE,
                "rebuilt_history_end": test_era::REBUILT_END,
            })
        );
        // With no accepted batch yet, the era's baseline is the checkpoint.
        assert_eq!(
            info["accepted_checkpoint"],
            serde_json::json!({
                "inclusion_block": test_era::STOP_BLOCK,
                "executed_input_count": test_era::REBUILT_END,
                "next_batch_nonce": test_era::NEXT_BATCH_NONCE,
            })
        );
    }
}
