// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared HTTP surface: error type + JSON response shape used by both
//! ingress (`/tx`) and egress (`/ws/subscribe`, future routes), plus the
//! `axum::serve` orchestration that wires the two side routers together.
//!
//! Today both sides serve from one listener; the planned api split puts each
//! side on its own port (same binary, two listeners). When that lands, the
//! orchestration here becomes per-side `start_*` calls.

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
use tokio::task::{JoinHandle, JoinSet};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub use crate::egress::api::SnapshotState;
use crate::egress::api::SubscribeState;
use crate::egress::l2_tx_feed::L2TxFeed;
use crate::ingress::api::SubmitState;
use crate::ingress::inclusion_lane::{PendingUserOp, SequencerError};
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::ReleaseScheduler;
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

/// Stable prefix of the WS Close-frame reason when the subscriber's requested
/// `from_offset` is too old for the catch-up window to bridge.
///
/// The full reason is `{WS_CATCHUP_WINDOW_EXCEEDED_REASON}: live_start_offset=<u64>`.
pub const WS_CATCHUP_WINDOW_EXCEEDED_REASON: &str = "catch-up window exceeded";

pub type ApiServerTask = JoinHandle<std::io::Result<()>>;

type SnapshotReleaseTask = Box<dyn FnOnce() + Send + 'static>;

/// Joins every blocking snapshot-lease release before the HTTP worker exits.
///
/// The scheduler passed into each lease guard owns a channel producer. The
/// receiver therefore cannot close while a guard can still submit a release,
/// including a guard dropping concurrently with graceful HTTP shutdown.
struct SnapshotReleaseDrain {
    supervisor: JoinHandle<()>,
    shutdown: RuntimeScope,
}

impl SnapshotReleaseDrain {
    async fn finish(self) {
        if let Err(join) = self.supervisor.await {
            tracing::error!(
                error = %join,
                "snapshot lease release supervisor task failed"
            );
            self.shutdown.contain_storage_invariant_failure(format!(
                "snapshot lease release supervisor task failed: {join}"
            ));
        }
    }
}

fn supervise_snapshot_releases(shutdown: RuntimeScope) -> (ReleaseScheduler, SnapshotReleaseDrain) {
    let (sender, receiver) = mpsc::unbounded_channel::<SnapshotReleaseTask>();
    let schedule_shutdown = shutdown.clone();
    let scheduler: ReleaseScheduler = Arc::new(move |release| {
        if sender.send(release).is_err() {
            tracing::error!("snapshot lease release supervisor is unavailable");
            // A closed receiver while this producer still exists means the
            // supervisor failed. Containment is sync and callable from any
            // thread — the old `tokio::spawn` wrapper was residue of the
            // deleted async containment API and would have panicked on a
            // non-runtime thread. `SnapshotReleaseDrain` also
            // classifies its join before the HTTP worker can finish.
            schedule_shutdown.contain_storage_invariant_failure(
                "snapshot lease release supervisor is unavailable",
            );
        }
    });
    let supervisor_shutdown = shutdown.clone();
    let supervisor = tokio::spawn(async move {
        run_snapshot_release_supervisor(receiver, supervisor_shutdown).await;
    });
    (
        scheduler,
        SnapshotReleaseDrain {
            supervisor,
            shutdown,
        },
    )
}

async fn run_snapshot_release_supervisor(
    mut receiver: mpsc::UnboundedReceiver<SnapshotReleaseTask>,
    shutdown: RuntimeScope,
) {
    let mut releases = JoinSet::new();
    let mut accepting = true;

    while accepting || !releases.is_empty() {
        tokio::select! {
            task = receiver.recv(), if accepting => {
                match task {
                    Some(release) => {
                        releases.spawn_blocking(release);
                    }
                    None => accepting = false,
                }
            }
            result = releases.join_next(), if !releases.is_empty() => {
                if let Some(Err(join)) = result {
                    tracing::error!(
                        error = %join,
                        "snapshot lease release task failed"
                    );
                    shutdown
                        .contain_storage_invariant_failure(format!(
                            "snapshot lease release task panicked: {join}"
                        ));
                }
            }
        }
    }
}

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
pub(crate) fn start_on_listener(
    listener: tokio::net::TcpListener,
    tx_sender: mpsc::Sender<PendingUserOp>,
    domain: Eip712Domain,
    max_user_op_data_bytes: usize,
    shutdown: RuntimeScope,
    tx_feed: L2TxFeed,
    config: ApiConfig,
    snapshot_state: SnapshotState,
) -> ApiServerTask {
    let (snapshot_release_scheduler, snapshot_release_drain) =
        supervise_snapshot_releases(shutdown.clone());
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
        .merge(crate::egress::api::router(
            subscribe_state,
            health_state,
            snapshot_state,
            shutdown.clone(),
            snapshot_release_scheduler,
        ))
        // Enforces a raw request-body cap before JSON deserialization, including whitespace.
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        .layer(TraceLayer::new_for_http())
        // Permissive CORS so browser wallets can POST /tx (and preflight OPTIONS).
        // Tighten when the ingress/egress port split lands and public exposure is narrower.
        .layer(CorsLayer::permissive());

    tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.wait_for_shutdown().await;
            })
            .await;
        // `Workers::finish` awaits this server handle before its final sticky
        // fault check, so no supervised release may outlive classification.
        snapshot_release_drain.finish().await;
        result
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_release_drain_waits_for_late_terminal_report() {
        let shutdown = RuntimeScope::default();
        let (schedule, drain) = supervise_snapshot_releases(shutdown.clone());
        let (started_tx, started_rx) = oneshot::channel();
        let (unblock_tx, unblock_rx) = std::sync::mpsc::channel();
        let release_shutdown = shutdown.clone();
        let mut drain_task = tokio::spawn(drain.finish());

        // Start draining first. The scheduler's producer token must keep the
        // queue open so this release, submitted concurrently with shutdown,
        // cannot be missed.
        tokio::task::yield_now().await;
        assert!(!drain_task.is_finished());

        schedule(Box::new(move || {
            let _ = started_tx.send(());
            unblock_rx.recv().expect("release test gate");
            release_shutdown.contain_storage_invariant_failure("test release fault");
        }));
        started_rx.await.expect("release task started");
        drop(schedule);

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut drain_task)
                .await
                .is_err(),
            "drain returned before the in-flight release completed"
        );

        unblock_tx.send(()).expect("unblock release");
        drain_task.await.expect("join release drain");
        assert!(
            shutdown.is_storage_invariant_contained(),
            "the terminal report must be sticky before the drain returns"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_release_task_panic_is_terminal_before_drain_returns() {
        let shutdown = RuntimeScope::default();
        let (schedule, drain) = supervise_snapshot_releases(shutdown.clone());

        schedule(Box::new(|| panic!("simulated lease release panic")));
        drop(schedule);
        drain.finish().await;

        assert!(shutdown.is_storage_invariant_contained());
    }
}
