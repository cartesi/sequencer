// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! HTTP / WebSocket surface. Two endpoints today:
//!
//! - `POST /tx` — submit a signed user op (validated, enqueued for the
//!   inclusion lane). See `tx`.
//! - `GET /ws/subscribe` — replay + live stream of the ordered L2 tx feed.
//!   See `ws`.
//!
//! Both endpoints share an [`ApiState`] and an [`ApiError`] today. The two
//! halves are otherwise independent and are intended to split into separate
//! processes/binaries — keep that in mind when adding cross-endpoint coupling
//! to `state.rs` or `error.rs`.

mod error;
mod state;
mod tx;
mod ws;

use std::io;
use std::sync::Arc;

use alloy_sol_types::Eip712Domain;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use tokio::sync::mpsc;
use tower_http::trace::TraceLayer;

pub use error::ApiError;
use state::ApiState;

use crate::inclusion_lane::PendingUserOp;
use crate::l2_tx_feed::L2TxFeed;
use crate::shutdown::ShutdownSignal;
use sequencer_core::api::TxRequest;

const DEFAULT_WS_MAX_SUBSCRIBERS: usize = 64;
const DEFAULT_WS_MAX_CATCHUP_EVENTS: u64 = 50_000;
const DEFAULT_MAX_BODY_BYTES: usize = TxRequest::MAX_JSON_BYTES_RECOMMENDED;
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

pub fn start_on_listener(
    listener: tokio::net::TcpListener,
    tx_sender: mpsc::Sender<PendingUserOp>,
    domain: Eip712Domain,
    max_user_op_data_bytes: usize,
    shutdown: ShutdownSignal,
    tx_feed: L2TxFeed,
    config: ApiConfig,
) -> ApiServerTask {
    let state = Arc::new(ApiState::new(
        tx_sender,
        domain,
        max_user_op_data_bytes,
        shutdown.clone(),
        tx_feed,
        config,
    ));
    let app = router(state, config.max_body_bytes);

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.wait_for_shutdown().await;
            })
            .await
    })
}

fn router(state: Arc<ApiState>, max_body_bytes: usize) -> Router {
    Router::new()
        .route("/tx", post(tx::submit_tx))
        .route("/ws/subscribe", get(ws::subscribe_l2_txs))
        .with_state(state)
        // Enforces a raw request-body cap before JSON deserialization, including whitespace.
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(TraceLayer::new_for_http())
}
