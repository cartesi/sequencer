// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Operator-only snapshot read endpoints.
//!
//! **These routes are operator-internal.** `/finalized_state` and
//! `/latest_snapshot` stream full application state with no authentication
//! and MUST NOT be exposed to the public internet — they serve the watchdog
//! and the operator's indexers from the internal tier, gated by network
//! controls today (and bound to the internal listener once the per-port api
//! split lands). See `AGENTS.md` and the threat model.
//!
//! - `GET /finalized_state/inclusion_block` — cheap JSON
//!   `{ inclusion_block, l2_tx_index }` the watchdog polls to detect advance.
//! - `GET /finalized_state` — streams the finalized state file (watchdog).
//! - `GET /latest_snapshot` — streams the latest snapshot dump (indexers).
//!
//! The two streaming routes lease the dump for the lifetime of the response —
//! acquired atomically with the row read, so GC can't delete it between the
//! read and the file open — and release it via a drop-guard that fires even
//! on client disconnect. `Storage::reset_dump_leases` at startup is the crash
//! backstop.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::fs::File;
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::io::ReaderStream;

use crate::storage::{LeaseGuard, LeasedDump, Storage};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Wiring for the snapshot endpoints: where the DB is, and how to find the
/// canonical state file inside a dump. `state_file_in_dump` is threaded as a
/// fn pointer (`A::state_file_in_dump`) so this layer stays free of an `A`
/// type parameter.
#[derive(Clone)]
pub struct SnapshotState {
    pub db_path: String,
    pub state_file_in_dump: fn(&Path) -> PathBuf,
}

pub(crate) fn router(state: Arc<SnapshotState>) -> Router {
    Router::new()
        .route("/finalized_state", get(finalized_state))
        .route(
            "/finalized_state/inclusion_block",
            get(finalized_inclusion_block),
        )
        .route("/latest_snapshot", get(latest_snapshot))
        .with_state(state)
}

#[derive(Serialize)]
struct InclusionBlockResponse {
    inclusion_block: u64,
    l2_tx_index: u64,
}

/// `GET /finalized_state/inclusion_block` — cheap read, no lease (no file is
/// opened). 404 if no finalized snapshot exists.
async fn finalized_inclusion_block(State(state): State<Arc<SnapshotState>>) -> Response {
    let db_path = state.db_path.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_, BoxError> {
        Ok(Storage::open_read_only(&db_path)?.finalized_dump()?)
    })
    .await;
    match result {
        Ok(Ok(Some(finalized))) => Json(InclusionBlockResponse {
            inclusion_block: finalized.inclusion_block,
            l2_tx_index: finalized.l2_tx_index,
        })
        .into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(err)) => internal_error("read finalized inclusion block", err),
        Err(join) => internal_error("inclusion-block task join", join),
    }
}

/// `GET /finalized_state` — stream the finalized state file (watchdog
/// source). Supports `If-None-Match` against `"block-<n>"` for a 304.
async fn finalized_state(State(state): State<Arc<SnapshotState>>, headers: HeaderMap) -> Response {
    let leased = match acquire_finalized(&state).await {
        Ok(Some(leased)) => leased,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return internal_error("acquire finalized lease", err),
    };

    let inclusion_block = leased
        .inclusion_block
        .expect("finalized snapshot carries an inclusion block");
    let etag = format!("\"block-{inclusion_block}\"");
    if if_none_match(&headers, &etag) {
        // 304: dropping `leased` here releases the lease via its guard.
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let path = (state.state_file_in_dump)(&leased.prefix);
    let l2_tx_index = leased.l2_tx_index;
    let LeasedDump { guard, .. } = leased;

    match File::open(&path).await {
        Ok(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::ETAG, etag)
            .header("X-Inclusion-Block", inclusion_block.to_string())
            .header("X-L2-Tx-Index", l2_tx_index.to_string())
            .body(stream_body(file, guard))
            .expect("snapshot response headers are well-formed"),
        // `guard` is a local here; on this error path it drops → lease released.
        Err(err) => internal_error("open finalized state file", err),
    }
}

/// `GET /latest_snapshot` — stream the latest snapshot dump (indexers: fetch
/// then subscribe at this offset). Latest pending if any, else finalized.
async fn latest_snapshot(State(state): State<Arc<SnapshotState>>) -> Response {
    let leased = match acquire_latest(&state).await {
        Ok(Some(leased)) => leased,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return internal_error("acquire latest snapshot lease", err),
    };

    let path = (state.state_file_in_dump)(&leased.prefix);
    let l2_tx_index = leased.l2_tx_index;
    let LeasedDump { guard, .. } = leased;

    match File::open(&path).await {
        Ok(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header("X-L2-Tx-Index", l2_tx_index.to_string())
            .body(stream_body(file, guard))
            .expect("snapshot response headers are well-formed"),
        Err(err) => internal_error("open latest snapshot file", err),
    }
}

fn stream_body(file: File, guard: LeaseGuard) -> Body {
    Body::from_stream(ReaderStream::new(GuardedReader {
        file,
        _guard: guard,
    }))
}

// ── Lease acquisition (storage returns the dump bundled with its release) ──

async fn acquire_finalized(state: &SnapshotState) -> Result<Option<LeasedDump>, BoxError> {
    let db_path = state.db_path.clone();
    tokio::task::spawn_blocking(move || -> Result<Option<LeasedDump>, BoxError> {
        let mut storage = Storage::open_writer(&db_path)?;
        Ok(storage.acquire_finalized_lease(spawn_blocking_release)?)
    })
    .await?
}

async fn acquire_latest(state: &SnapshotState) -> Result<Option<LeasedDump>, BoxError> {
    let db_path = state.db_path.clone();
    tokio::task::spawn_blocking(move || -> Result<Option<LeasedDump>, BoxError> {
        let mut storage = Storage::open_writer(&db_path)?;
        Ok(storage.acquire_latest_snapshot_lease(spawn_blocking_release)?)
    })
    .await?
}

// ── Release scheduler: keep the lease release off the async worker ─────────

/// The `ReleaseScheduler` the egress layer hands to `acquire_*_lease`. The
/// lease release is a write-lock-contended SQLite write, and on client
/// disconnect the guard drops on an async worker thread — so offload it to the
/// blocking pool rather than stall the worker. Off-runtime (shouldn't happen
/// from here), run it inline.
fn spawn_blocking_release(release: Box<dyn FnOnce() + Send + 'static>) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(release);
        }
        Err(_) => release(),
    }
}

// ── Streaming body that owns the lease guard ───────────────────────────────

/// A file reader that also owns the lease guard. When the response body is
/// dropped — stream completion, I/O error, or client disconnect — the guard
/// drops with it and releases the lease.
struct GuardedReader {
    file: File,
    _guard: LeaseGuard,
}

impl AsyncRead for GuardedReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // `GuardedReader` is `Unpin` (both fields are), so this is sound.
        let this = self.get_mut();
        Pin::new(&mut this.file).poll_read(cx, buf)
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag)
}

fn internal_error(context: &str, err: impl std::fmt::Display) -> Response {
    tracing::warn!(error = %err, context, "snapshot endpoint failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
