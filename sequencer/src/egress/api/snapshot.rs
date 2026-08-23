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

use crate::runtime::shutdown::RuntimeScope;
use crate::storage::{LeaseGuard, LeasedDump, ReleaseScheduler, Storage};

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

struct SnapshotApiState {
    snapshot: SnapshotState,
    shutdown: RuntimeScope,
    release_scheduler: ReleaseScheduler,
}

impl SnapshotApiState {
    /// Refuse to start a stream only after a terminal fault is contained.
    /// Streaming an already-immutable operator snapshot is not
    /// authority-bearing (ADR), so ordinary graceful shutdown does NOT gate
    /// these routes — the watchdog's byte-compare poll and indexer fetches
    /// keep working through an operator drain. Containment is checked
    /// at stream start; the state a contained fault may have poisoned must
    /// not be served.
    fn authorize_stream(&self) -> Option<crate::runtime::shutdown::Authorized<'_>> {
        self.shutdown.authorize()
    }
}

pub(crate) fn router(
    snapshot: SnapshotState,
    shutdown: RuntimeScope,
    release_scheduler: ReleaseScheduler,
) -> Router {
    let state = Arc::new(SnapshotApiState {
        snapshot,
        shutdown,
        release_scheduler,
    });
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
async fn finalized_inclusion_block(State(state): State<Arc<SnapshotApiState>>) -> Response {
    let Some(_auth) = state.authorize_stream() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let db_path = state.snapshot.db_path.clone();
    let result = storage_task(&state, "read finalized inclusion block", move |_scope| {
        Ok(Storage::open_read_only(&db_path)?.finalized_dump()?)
    })
    .await;
    match result {
        Ok(Some(finalized)) => Json(InclusionBlockResponse {
            inclusion_block: finalized.inclusion_block,
            l2_tx_index: finalized.l2_tx_index,
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => internal_error("read finalized inclusion block", err),
    }
}

/// `GET /finalized_state` — stream the finalized state file (watchdog
/// source). Supports `If-None-Match` against `"block-<n>"` for a 304.
async fn finalized_state(
    State(state): State<Arc<SnapshotApiState>>,
    headers: HeaderMap,
) -> Response {
    let Some(_auth) = state.authorize_stream() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let leased = match acquire_finalized(&state).await {
        Ok(Some(leased)) => leased,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return internal_error("acquire finalized lease", err),
    };

    let Some(inclusion_block) = leased.inclusion_block else {
        state
            .shutdown
            .contain_storage_invariant_failure("finalized snapshot carried no inclusion block");
        return internal_error(
            "read finalized snapshot",
            "finalized snapshot carried no inclusion block",
        );
    };
    let etag = format!("\"block-{inclusion_block}\"");
    if if_none_match(&headers, &etag) {
        // 304: dropping `leased` here releases the lease via its guard.
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let path = (state.snapshot.state_file_in_dump)(&leased.prefix);
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
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                tracing::error!(path = ?path, "durable finalized snapshot artifact is missing");
                state.shutdown.contain_storage_invariant_failure(format!(
                    "durable finalized snapshot artifact missing: {path:?}"
                ));
            }
            internal_error("open finalized state file", err)
        }
    }
}

/// `GET /latest_snapshot` — stream the latest snapshot dump (indexers: fetch
/// then subscribe at this offset). Latest pending if any, else finalized.
async fn latest_snapshot(State(state): State<Arc<SnapshotApiState>>) -> Response {
    let Some(_auth) = state.authorize_stream() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let leased = match acquire_latest(&state).await {
        Ok(Some(leased)) => leased,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return internal_error("acquire latest snapshot lease", err),
    };

    let path = (state.snapshot.state_file_in_dump)(&leased.prefix);
    let l2_tx_index = leased.l2_tx_index;
    let LeasedDump { guard, .. } = leased;

    match File::open(&path).await {
        Ok(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header("X-L2-Tx-Index", l2_tx_index.to_string())
            .body(stream_body(file, guard))
            .expect("snapshot response headers are well-formed"),
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                tracing::error!(path = ?path, "durable latest snapshot artifact is missing");
                state.shutdown.contain_storage_invariant_failure(format!(
                    "durable latest snapshot artifact missing: {path:?}"
                ));
            }
            internal_error("open latest snapshot file", err)
        }
    }
}

fn stream_body(file: File, guard: LeaseGuard) -> Body {
    Body::from_stream(ReaderStream::new(GuardedReader {
        file,
        _guard: guard,
    }))
}

// ── Blocking storage tasks ─────────────────────────────────────────────────

/// One spawn/join/classify shape for this endpoint's blocking storage work.
/// The posture is deliberate and stays local: an HTTP handler has no
/// worker-exit channel to carry a typed error to the supervisor, so a
/// persistent row/schema failure or a storage-task panic contains
/// immediately.
async fn storage_task<T, F>(
    state: &SnapshotApiState,
    operation: &'static str,
    work: F,
) -> Result<T, BoxError>
where
    T: Send + 'static,
    F: FnOnce(crate::runtime::shutdown::RuntimeScope) -> Result<T, BoxError> + Send + 'static,
{
    let scope = state.shutdown.clone();
    match tokio::task::spawn_blocking(move || {
        // Independent retention, bound first so it drops last: the task owns
        // data-directory exclusivity for its REAL lifetime — including the
        // final drop of its SQLite connection (a WAL checkpoint writes to
        // the data dir) — regardless of what `work` does with its scope
        // argument. The lease closures consume theirs early (the reporter
        // Arc can die inside storage on the None/Err paths), which is
        // exactly the coupling this binding exists to break (ADR §1; found
        // by adversarial review).
        let _runtime_lifetime = scope.clone();
        work(scope)
    })
    .await
    {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            if persistent_storage_error(error.as_ref(), operation) {
                state
                    .shutdown
                    .contain_storage_invariant_failure(format!("{operation}: {error}"));
            }
            Err(error)
        }
        Err(join) => {
            if storage_task_panicked(&join, operation) {
                state.shutdown.contain_storage_invariant_failure(format!(
                    "{operation}: task panicked: {join}"
                ));
            }
            Err(Box::new(join))
        }
    }
}

// ── Lease acquisition (storage returns the dump bundled with its release) ──

async fn acquire_finalized(state: &SnapshotApiState) -> Result<Option<LeasedDump>, BoxError> {
    let db_path = state.snapshot.db_path.clone();
    let release_scheduler = state.release_scheduler.clone();
    storage_task(state, "acquire finalized snapshot lease", move |scope| {
        let report_persistent_failure: crate::storage::PersistentReleaseFailureReporter =
            Arc::new(move |cause: &str| scope.contain_storage_invariant_failure(cause));
        let mut storage = Storage::open_writer(&db_path)?;
        Ok(storage.acquire_finalized_lease(release_scheduler, report_persistent_failure)?)
    })
    .await
}

async fn acquire_latest(state: &SnapshotApiState) -> Result<Option<LeasedDump>, BoxError> {
    let db_path = state.snapshot.db_path.clone();
    let release_scheduler = state.release_scheduler.clone();
    storage_task(state, "acquire latest snapshot lease", move |scope| {
        let report_persistent_failure: crate::storage::PersistentReleaseFailureReporter =
            Arc::new(move |cause: &str| scope.contain_storage_invariant_failure(cause));
        let mut storage = Storage::open_writer(&db_path)?;
        Ok(storage.acquire_latest_snapshot_lease(release_scheduler, report_persistent_failure)?)
    })
    .await
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

fn storage_task_panicked(join: &tokio::task::JoinError, operation: &'static str) -> bool {
    if join.is_panic() {
        tracing::error!(operation, "persistent storage invariant violation");
        true
    } else {
        false
    }
}

fn persistent_storage_error(
    mut error: &(dyn std::error::Error + 'static),
    operation: &'static str,
) -> bool {
    loop {
        let persistent = error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(crate::storage::is_persistent_storage_error)
            || error
                .downcast_ref::<crate::storage::StorageOpenError>()
                .is_some_and(crate::storage::is_persistent_storage_open_error);
        if persistent {
            tracing::error!(operation, error = %error, "persistent storage invariant violation");
            return true;
        }
        let Some(source) = error.source() else {
            return false;
        };
        error = source;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::temp_db;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corrupt_finalized_snapshot_trips_terminal_storage_fault() {
        let db = temp_db("corrupt-finalized-endpoint");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .insert_finalized_dump(Path::new("/tmp/corrupt-finalized"), 12, 34)
            .expect("insert finalized snapshot");
        drop(storage);

        let conn = Storage::open_connection(db.path.as_str()).expect("raw connection");
        conn.pragma_update(None, "ignore_check_constraints", "ON")
            .expect("allow corruption fixture");
        conn.execute(
            "UPDATE finalized_snapshot SET l2_tx_index = -1 WHERE singleton_id = 0",
            [],
        )
        .expect("corrupt finalized cursor");
        drop(conn);

        let shutdown = RuntimeScope::default();
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path,
                state_file_in_dump: |prefix| prefix.join("state"),
            },
            shutdown: shutdown.clone(),
            release_scheduler: Arc::new(|release| release()),
        });

        let response = finalized_inclusion_block(State(state)).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(shutdown.is_storage_invariant_contained());
        assert!(shutdown.is_shutdown_requested());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_does_not_gate_snapshot_reads_but_containment_does() {
        let db = temp_db("snapshot-gate-predicate");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .insert_finalized_dump(Path::new("/tmp/gate-finalized"), 12, 34)
            .expect("insert finalized snapshot");
        drop(storage);

        let shutdown = RuntimeScope::default();
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path,
                state_file_in_dump: |prefix| prefix.join("state"),
            },
            shutdown: shutdown.clone(),
            release_scheduler: Arc::new(|release| release()),
        });

        // Immutable operator reads are not authority-bearing (ADR): an
        // ordinary graceful drain keeps serving the watchdog's poll.
        shutdown.request_shutdown();
        let response = finalized_inclusion_block(State(state.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);

        // A contained terminal fault is the one condition that refuses a
        // stream start: the state it may have poisoned must not be served.
        shutdown.contain_storage_invariant_failure("test containment");
        let response = finalized_inclusion_block(State(state)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dangling_finalized_snapshot_row_trips_terminal_storage_fault() {
        let db = temp_db("dangling-finalized-endpoint");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .insert_finalized_dump(Path::new("/tmp/dangling-finalized"), 12, 34)
            .expect("insert finalized snapshot");
        drop(storage);

        let conn = Storage::open_connection(db.path.as_str()).expect("raw connection");
        conn.pragma_update(None, "foreign_keys", "OFF")
            .expect("disable foreign keys for corruption fixture");
        conn.execute("DELETE FROM dumps", [])
            .expect("remove referenced dump row");
        drop(conn);

        let shutdown = RuntimeScope::default();
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path,
                state_file_in_dump: |prefix| prefix.join("state"),
            },
            shutdown: shutdown.clone(),
            release_scheduler: Arc::new(|release| release()),
        });

        let response = finalized_inclusion_block(State(state)).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(shutdown.is_storage_invariant_contained());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_finalized_snapshot_file_trips_terminal_storage_fault() {
        let db = temp_db("missing-finalized-file");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let missing_root = tempfile::tempdir().expect("missing snapshot parent");
        let missing_prefix = missing_root.path().join("not-created");
        storage
            .insert_finalized_dump(&missing_prefix, 12, 34)
            .expect("insert finalized snapshot");
        drop(storage);

        let shutdown = RuntimeScope::default();
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path,
                state_file_in_dump: |prefix| prefix.join("state"),
            },
            shutdown: shutdown.clone(),
            release_scheduler: Arc::new(|release| release()),
        });

        let response = finalized_state(State(state), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(shutdown.is_storage_invariant_contained());
    }

    #[tokio::test]
    async fn transient_storage_open_error_does_not_trip_terminal_fault() {
        let shutdown = RuntimeScope::default();
        let error = crate::storage::StorageOpenError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            None,
        ));

        assert!(!persistent_storage_error(
            &error,
            "test transient storage contention"
        ));

        assert!(!shutdown.is_storage_invariant_contained());
        assert!(!shutdown.is_shutdown_requested());
    }
}
