// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Coherent history metadata and bounded pages of the immutable era L1 prefix.

use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sequencer_core::history::{EraId, RecoveryGeneration};
use sequencer_core::history_api::{HISTORICAL_INPUT_MAX_ITEMS, HistoricalL1InputStart};
use serde::Deserialize;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;

use crate::http::{ApiError, StorageTaskError, storage_task};
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::{HistoricalReadError, Storage};

const MAX_HISTORICAL_RESPONSES: usize = 8;

struct HistoryState {
    db_path: String,
    shutdown: RuntimeScope,
    responses: Arc<Semaphore>,
}

pub(super) fn router(db_path: String, shutdown: RuntimeScope) -> Router {
    Router::new()
        .route("/history", get(history))
        .route("/historical-l1-inputs", get(historical_l1_inputs))
        .with_state(Arc::new(HistoryState {
            db_path,
            shutdown,
            responses: Arc::new(Semaphore::new(MAX_HISTORICAL_RESPONSES)),
        }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    era_id: Option<EraId>,
    from_generation: Option<RecoveryGeneration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalInputsQuery {
    era_id: EraId,
    next_input_index: Option<u64>,
    after_block: Option<u64>,
    limit: Option<usize>,
}

impl HistoricalInputsQuery {
    fn start(&self) -> Result<HistoricalL1InputStart, ApiError> {
        match (self.next_input_index, self.after_block) {
            (Some(index), None) => Ok(HistoricalL1InputStart::NextInputIndex(index)),
            (None, Some(block)) => Ok(HistoricalL1InputStart::AfterBlock(block)),
            _ => Err(ApiError::bad_request(
                "provide exactly one of next_input_index or after_block",
            )),
        }
    }
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
    if state.shutdown.is_shutdown_requested() {
        return ApiError::unavailable("sequencer shutting down").into_response();
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

async fn historical_l1_inputs(
    State(state): State<Arc<HistoryState>>,
    query: Result<Query<HistoricalInputsQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(error) => return ApiError::bad_request(error.body_text()).into_response(),
    };
    let start = match query.start() {
        Ok(start) => start,
        Err(error) => return error.into_response(),
    };
    if state.shutdown.is_shutdown_requested() {
        return ApiError::unavailable("sequencer shutting down").into_response();
    }
    let permit = match state.responses.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return ApiError::overloaded("historical response limit reached").into_response(),
    };
    let db_path = state.db_path.clone();
    let result = storage_task(
        state.shutdown.clone(),
        "read historical L1 inputs",
        move |_| {
            // The blocking task owns admission even if its HTTP request is cancelled.
            let page = Storage::open_read_only(&db_path)?.historical_l1_inputs(
                query.era_id,
                start,
                query.limit.unwrap_or(HISTORICAL_INPUT_MAX_ITEMS),
            )?;
            let bytes = serde_json::to_vec(&page)?;
            Ok((bytes, permit))
        },
    )
    .await;
    match result {
        Ok((bytes, permit)) => (
            [(header::CONTENT_TYPE, "application/json")],
            Body::from_stream(ReaderStream::new(HistoricalResponseBody {
                bytes: Cursor::new(bytes),
                _permit: permit,
            })),
        )
            .into_response(),
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

struct HistoricalResponseBody {
    bytes: Cursor<Vec<u8>>,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for HistoricalResponseBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.bytes).poll_read(cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERA: &str = "11111111-1111-4111-8111-111111111111";

    fn state() -> Arc<HistoryState> {
        Arc::new(HistoryState {
            db_path: "unused: invalid queries do not access storage".to_owned(),
            shutdown: RuntimeScope::default(),
            responses: Arc::new(Semaphore::new(1)),
        })
    }

    #[tokio::test]
    async fn malformed_queries_return_bad_request_json_before_storage() {
        for query in [
            "",
            "?era_id=bad&next_input_index=0",
            &format!("?era_id={ERA}"),
            &format!("?era_id={ERA}&next_input_index=0&after_block=0"),
            &format!("?era_id={ERA}&next_input_index=-1"),
            &format!("?era_id={ERA}&next_input_index=0&unrecognized=1"),
        ] {
            let uri = format!("/historical-l1-inputs{query}").parse().unwrap();
            let response = historical_l1_inputs(State(state()), Query::try_from_uri(&uri)).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(error["code"], "BAD_REQUEST");
        }
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
    async fn shutdown_and_overload_refuse_before_opening_storage() {
        let state = state();
        let uri = format!("/historical-l1-inputs?era_id={ERA}&next_input_index=0")
            .parse()
            .unwrap();
        let permit = state.responses.clone().try_acquire_owned().unwrap();
        let response = historical_l1_inputs(State(state.clone()), Query::try_from_uri(&uri)).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        drop(permit);
        state.shutdown.request_shutdown();
        let response = historical_l1_inputs(State(state.clone()), Query::try_from_uri(&uri)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
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
    async fn response_body_holds_admission_until_consumed_or_dropped() {
        let permits = Arc::new(Semaphore::new(1));
        for consume in [false, true] {
            let body = Body::from_stream(ReaderStream::new(HistoricalResponseBody {
                bytes: Cursor::new(vec![1; 100_000]),
                _permit: permits.clone().try_acquire_owned().unwrap(),
            }));
            assert!(permits.clone().try_acquire_owned().is_err());
            if consume {
                assert_eq!(
                    axum::body::to_bytes(body, 100_000).await.unwrap().len(),
                    100_000
                );
            } else {
                drop(body);
            }
            assert_eq!(permits.available_permits(), 1);
        }
    }
}
