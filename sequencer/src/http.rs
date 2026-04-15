// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared HTTP surface: error type + JSON response shape used by both
//! ingress (`/tx`) and egress (`/ws/subscribe`, future routes), plus the
//! `axum::serve` orchestration that wires the two side routers together.
//!
//! Today both sides serve from one listener; the planned api split puts each
//! side on its own port (same binary, two listeners). When that lands, the
//! orchestration here becomes per-side `start_*` calls.

use std::io;
use std::sync::Arc;

use alloy_sol_types::Eip712Domain;
use axum::Json;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::mpsc;
use tower_http::trace::TraceLayer;

use crate::egress::api::SubscribeState;
use crate::egress::l2_tx_feed::L2TxFeed;
use crate::ingress::api::SubmitState;
use crate::ingress::inclusion_lane::{PendingUserOp, SequencerError};
use crate::runtime::shutdown::ShutdownSignal;
use sequencer_core::api::{TxRequest, TxRequestError};

#[derive(Debug, Error, Clone)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("{0}")]
    InvalidSignature(String),
    #[error("{0}")]
    ExecutionRejected(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    InternalError(String),
    #[error("{0}")]
    Overloaded(String),
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    ok: bool,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::PayloadTooLarge(message.into())
    }

    pub fn invalid_signature(message: impl Into<String>) -> Self {
        Self::InvalidSignature(message.into())
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::InternalError(message.into())
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::Overloaded(message.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) | Self::InvalidSignature(_) => StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::ExecutionRejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Overloaded(_) => StatusCode::TOO_MANY_REQUESTS,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::PayloadTooLarge(_) => "PAYLOAD_TOO_LARGE",
            Self::InvalidSignature(_) => "INVALID_SIGNATURE",
            Self::ExecutionRejected(_) => "EXECUTION_REJECTED",
            Self::Unavailable(_) => "UNAVAILABLE",
            Self::InternalError(_) => "INTERNAL_ERROR",
            Self::Overloaded(_) => "OVERLOADED",
        }
    }
}

impl From<SequencerError> for ApiError {
    fn from(value: SequencerError) -> Self {
        match value {
            SequencerError::Invalid(message) => Self::ExecutionRejected(message),
            SequencerError::Unavailable(message) => Self::Unavailable(message),
            SequencerError::Internal(message) => Self::InternalError(message),
        }
    }
}

impl From<TxRequestError> for ApiError {
    fn from(value: TxRequestError) -> Self {
        match value {
            TxRequestError::BadRequest(message) => Self::BadRequest(message),
            TxRequestError::InvalidSignature(message) => Self::InvalidSignature(message),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            ok: false,
            code: self.code(),
            message: self.to_string(),
        };
        (self.status(), Json(body)).into_response()
    }
}

// ── HTTP server orchestration ────────────────────────────────────────────────
//
// Combines ingress + egress routers into one axum::serve. The api split will
// replace this with per-side starts on different ports.

const DEFAULT_WS_MAX_SUBSCRIBERS: usize = 64;
const DEFAULT_WS_MAX_CATCHUP_EVENTS: u64 = 50_000;
const DEFAULT_MAX_BODY_BYTES: usize = TxRequest::MAX_JSON_BYTES_RECOMMENDED;

/// Reason returned in the WS Close frame when the subscriber's requested
/// `from_offset` is too old for the catch-up window to bridge.
pub const WS_CATCHUP_WINDOW_EXCEEDED_REASON: &str = "catch-up window exceeded";

pub type ApiServerTask = tokio::task::JoinHandle<std::io::Result<()>>;

#[derive(Debug, Clone, Copy)]
pub struct ApiConfig {
    pub max_body_bytes: usize,
    pub ws_max_subscribers: usize,
    pub ws_max_catchup_events: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            ws_max_subscribers: DEFAULT_WS_MAX_SUBSCRIBERS,
            ws_max_catchup_events: DEFAULT_WS_MAX_CATCHUP_EVENTS,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn start(
    http_addr: impl tokio::net::ToSocketAddrs,
    tx_sender: mpsc::Sender<PendingUserOp>,
    domain: Eip712Domain,
    max_user_op_data_bytes: usize,
    shutdown: ShutdownSignal,
    tx_feed: L2TxFeed,
    config: ApiConfig,
) -> io::Result<ApiServerTask> {
    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    Ok(start_on_listener(
        listener,
        tx_sender,
        domain,
        max_user_op_data_bytes,
        shutdown,
        tx_feed,
        config,
    ))
}

#[allow(clippy::too_many_arguments)]
pub fn start_on_listener(
    listener: tokio::net::TcpListener,
    tx_sender: mpsc::Sender<PendingUserOp>,
    domain: Eip712Domain,
    max_user_op_data_bytes: usize,
    shutdown: ShutdownSignal,
    tx_feed: L2TxFeed,
    config: ApiConfig,
) -> ApiServerTask {
    let health_state = Arc::new(crate::egress::api::HealthState {
        tx_sender: tx_sender.clone(),
        shutdown: shutdown.clone(),
    });
    let submit_state = Arc::new(SubmitState::new(
        tx_sender,
        domain,
        max_user_op_data_bytes,
        shutdown.clone(),
    ));
    let subscribe_state = Arc::new(SubscribeState::new(
        shutdown.clone(),
        tx_feed,
        config.ws_max_subscribers,
        config.ws_max_catchup_events,
    ));

    let app: Router = crate::ingress::api::router(submit_state)
        .merge(crate::egress::api::router(subscribe_state, health_state))
        // Enforces a raw request-body cap before JSON deserialization, including whitespace.
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        .layer(TraceLayer::new_for_http());

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.wait_for_shutdown().await;
            })
            .await
    })
}
