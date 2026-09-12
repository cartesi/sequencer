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

use crate::runtime::shutdown::{RuntimeScope, abort_terminal};
use crate::storage::{FinalizedLease, LeaseGuard, LeasedDump, ReleaseScheduler, Storage};

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
    let FinalizedLease {
        inclusion_block,
        dump: leased,
    } = match acquire_finalized(&state).await {
        Ok(Some(leased)) => leased,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return internal_error("acquire finalized lease", err),
    };

    let etag = format!("\"block-{inclusion_block}\"");
    if if_none_match(&headers, &etag) {
        // 304: dropping `leased` here releases the lease via its guard.
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let path = state_file_path(&state.snapshot, &leased.prefix);
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
                abort_terminal(format!(
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
    let leased = match acquire_latest(&state).await {
        Ok(Some(leased)) => leased,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return internal_error("acquire latest snapshot lease", err),
    };

    let path = state_file_path(&state.snapshot, &leased.prefix);
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
                abort_terminal(format!(
                    "durable latest snapshot artifact missing: {path:?}"
                ));
            }
            internal_error("open latest snapshot file", err)
        }
    }
}

fn state_file_path(state: &SnapshotState, prefix: &Path) -> PathBuf {
    // HTTP request panics are otherwise isolated from the worker supervisor.
    std::panic::catch_unwind(|| (state.state_file_in_dump)(prefix))
        .unwrap_or_else(|_| abort_terminal("application snapshot path callback panicked"))
}

fn stream_body(file: File, guard: LeaseGuard) -> Body {
    Body::from_stream(ReaderStream::new(GuardedReader {
        file,
        _guard: guard,
    }))
}

// ── Blocking storage tasks ─────────────────────────────────────────────────

/// Classify inside the blocking task: cancellation of the HTTP request must
/// not discard a persistent fault discovered by work that already started.
async fn storage_task<T, F>(
    state: &SnapshotApiState,
    operation: &'static str,
    work: F,
) -> Result<T, BoxError>
where
    T: Send + 'static,
    F: FnOnce(RuntimeScope) -> Result<T, BoxError> + Send + 'static,
{
    let scope = state.shutdown.clone();
    match tokio::task::spawn_blocking(move || {
        // The independent clone outlives both work and its SQLite connection,
        // even when work consumes its scope argument before returning.
        let _runtime_lifetime = scope.clone();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(scope))) {
            Ok(Err(error)) if persistent_storage_error(error.as_ref()) => {
                abort_terminal(format_args!("{operation}: {error}"));
            }
            Ok(result) => result,
            Err(_) => abort_terminal(format_args!("{operation}: storage task panicked")),
        }
    })
    .await
    {
        Ok(result) => result,
        Err(join) if join.is_panic() => abort_terminal(format_args!("{operation}: {join}")),
        Err(join) => Err(Box::new(join)),
    }
}

// ── Lease acquisition (storage returns the dump bundled with its release) ──

async fn acquire_finalized(state: &SnapshotApiState) -> Result<Option<FinalizedLease>, BoxError> {
    let db_path = state.snapshot.db_path.clone();
    let release_scheduler = state.release_scheduler.clone();
    storage_task(state, "acquire finalized snapshot lease", move |scope| {
        let report_persistent_failure: crate::storage::PersistentReleaseFailureReporter =
            Arc::new(move |cause: &str| {
                let _runtime_lifetime = &scope;
                abort_terminal(cause)
            });
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
            Arc::new(move |cause: &str| {
                let _runtime_lifetime = &scope;
                abort_terminal(cause)
            });
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
fn persistent_storage_error(mut error: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        let persistent = error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(crate::storage::is_persistent_storage_error)
            || error
                .downcast_ref::<crate::storage::StorageOpenError>()
                .is_some_and(crate::storage::is_persistent_storage_open_error);
        if persistent {
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

    #[cfg(unix)]
    async fn panicking_state_path_aborts(test_name: &str, finalized: bool) {
        if !crate::runtime::shutdown::abort_test_child(test_name) {
            return;
        }
        let db = temp_db("panicking-snapshot-path");
        let mut storage = Storage::open(&db.path).expect("open storage");
        storage
            .insert_finalized_dump(Path::new("/tmp/panicking-snapshot-path"), 12, 34)
            .expect("insert finalized snapshot");
        drop(storage);
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path,
                state_file_in_dump: |_| panic!("application returned no state path"),
            },
            shutdown: RuntimeScope::default(),
            release_scheduler: Arc::new(|release| release()),
        });

        // Exercise the request-task boundary that would swallow this panic.
        let result = tokio::spawn(async move {
            if finalized {
                finalized_state(State(state), HeaderMap::new()).await
            } else {
                latest_snapshot(State(state)).await
            }
        })
        .await;
        panic!("snapshot path callback panic did not abort the process: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn finalized_state_path_panic_aborts_process() {
        panicking_state_path_aborts(
            "egress::api::snapshot::tests::finalized_state_path_panic_aborts_process",
            true,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn latest_snapshot_path_panic_aborts_process() {
        panicking_state_path_aborts(
            "egress::api::snapshot::tests::latest_snapshot_path_panic_aborts_process",
            false,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn corrupt_finalized_snapshot_trips_terminal_storage_fault() {
        if !crate::runtime::shutdown::abort_test_child(
            "egress::api::snapshot::tests::corrupt_finalized_snapshot_trips_terminal_storage_fault",
        ) {
            return;
        }
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

        panic!(
            "terminal snapshot fault returned HTTP {}",
            response.status()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_keeps_snapshot_reads_available() {
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn dangling_finalized_snapshot_row_trips_terminal_storage_fault() {
        if !crate::runtime::shutdown::abort_test_child(
            "egress::api::snapshot::tests::dangling_finalized_snapshot_row_trips_terminal_storage_fault",
        ) {
            return;
        }
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
        panic!(
            "terminal snapshot fault returned HTTP {}",
            response.status()
        );
    }

    #[test]
    fn transient_storage_open_error_does_not_trip_terminal_fault() {
        let error = crate::storage::StorageOpenError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            None,
        ));

        assert!(!persistent_storage_error(&error));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn missing_lease_row_aborts_through_runtime_reporter() {
        if !crate::runtime::shutdown::abort_test_child(
            "egress::api::snapshot::tests::missing_lease_row_aborts_through_runtime_reporter",
        ) {
            return;
        }
        let db = temp_db("missing-lease-row");
        let mut storage = Storage::open(&db.path).expect("open storage");
        storage
            .insert_finalized_dump(Path::new("/tmp/lease-probe"), 12, 34)
            .expect("register finalized dump");
        drop(storage);
        let state = SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path.clone(),
                state_file_in_dump: |prefix| prefix.join("state"),
            },
            shutdown: RuntimeScope::default(),
            release_scheduler: Arc::new(|release| release()),
        };
        let lease = acquire_finalized(&state)
            .await
            .expect("acquire lease")
            .expect("finalized dump");
        let conn = Storage::open_connection(&db.path).expect("raw connection");
        conn.pragma_update(None, "foreign_keys", "OFF")
            .expect("corruption fixture");
        conn.execute("DELETE FROM dumps", [])
            .expect("remove leased row");
        drop(conn);
        drop(lease);
        panic!("persistent release failure returned instead of aborting");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn cancelled_snapshot_request_cannot_discard_started_storage_fault() {
        if !crate::runtime::shutdown::abort_test_child(
            "egress::api::snapshot::tests::cancelled_snapshot_request_cannot_discard_started_storage_fault",
        ) {
            return;
        }
        let state = SnapshotApiState {
            snapshot: SnapshotState {
                db_path: String::new(),
                state_file_in_dump: |prefix| prefix.join("state"),
            },
            shutdown: RuntimeScope::default(),
            release_scheduler: Arc::new(|release| release()),
        };
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let request = tokio::spawn(async move {
            storage_task::<(), _>(
                &state,
                "cancelled request corruption probe",
                move |_scope| {
                    started_tx.send(()).expect("started storage task");
                    release_rx.recv().expect("release storage task");
                    Err(Box::new(rusqlite::Error::InvalidQuery))
                },
            )
            .await
        });
        started_rx.await.expect("storage work has started");
        request.abort();
        assert!(
            request
                .await
                .expect_err("cancelled HTTP task")
                .is_cancelled()
        );
        release_tx.send(()).expect("release detached storage work");
        std::future::pending::<()>().await;
    }
}
