// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Operator-only comparison files and complete restore/recovery archives.
//! Snapshot selection, history metadata, and leases are one SQLite transaction.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::ingress::inclusion_lane::dump_info::{self, CheckpointInfo};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::io::{ReaderStream, SyncIoBridge};

use crate::http::{ApiError, StorageTaskError, storage_task};
use crate::runtime::shutdown::{RuntimeScope, abort_terminal};
use crate::storage::{
    FinalizedLease, FinalizedSelectionError, LeaseGuard, LeasedDump, ReleaseScheduler, Storage,
};

type BoxError = StorageTaskError;

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
        .route("/finalized_state/digest", get(finalized_state_digest))
        .route("/latest_snapshot", get(latest_snapshot))
        .route("/finalized_snapshot", get(finalized_snapshot))
        .with_state(state)
}

#[derive(Serialize)]
struct InclusionBlockResponse {
    inclusion_block: u64,
    executed_input_count: u64,
}

/// `GET /finalized_state/inclusion_block` — cheap read, no lease (no file is
/// opened). 404 if no finalized snapshot exists.
async fn finalized_inclusion_block(State(state): State<Arc<SnapshotApiState>>) -> Response {
    let db_path = state.snapshot.db_path.clone();
    let result = storage_task(
        state.shutdown.clone(),
        "read finalized inclusion block",
        move |_scope| Ok(Storage::open_read_only(&db_path)?.finalized_dump()?),
    )
    .await;
    match result {
        Ok(Some(finalized)) => Json(InclusionBlockResponse {
            inclusion_block: finalized.inclusion_block,
            executed_input_count: finalized.executed_input_count.get(),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => finalized_error("read finalized inclusion block", err),
    }
}

#[derive(Serialize)]
struct DigestResponse {
    inclusion_block: u64,
    executed_input_count: u64,
    sha256: String,
}

/// `GET /finalized_state/digest` — SHA-256 of the file `GET /finalized_state`
/// streams, hashed under the same lease so the block and digest describe one
/// checkpoint. The watchdog compares digests; the bytes are only evidence.
async fn finalized_state_digest(State(state): State<Arc<SnapshotApiState>>) -> Response {
    let FinalizedLease {
        inclusion_block,
        dump: leased,
    } = match acquire_finalized(&state).await {
        Ok(Some(leased)) => leased,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return finalized_error("acquire finalized lease", err),
    };
    let path = state_file_path(&state.snapshot, &leased.prefix);
    let executed_input_count = leased.executed_input_count.get();
    let LeasedDump { guard, .. } = leased;
    let scope = state.shutdown.clone();
    let hashed = tokio::task::spawn_blocking(move || {
        let _runtime_lifetime = scope;
        let _lease = guard;
        sha256_file(&path)
    })
    .await;
    match hashed {
        Ok(Ok(digest)) => Json(DigestResponse {
            inclusion_block,
            executed_input_count,
            sha256: alloy_primitives::hex::encode(digest),
        })
        .into_response(),
        Ok(Err(err)) => internal_error("hash finalized state file", err),
        Err(err) if err.is_panic() => abort_terminal("comparison digest task panicked"),
        Err(err) => internal_error("hash finalized state file", err),
    }
}

fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = comparison_io(std::fs::File::open(path), path)?;
    let mut hasher = Sha256::new();
    // The hasher never fails a write, so any error is the comparison file's.
    comparison_io(std::io::copy(&mut file, &mut hasher), path)?;
    Ok(hasher.finalize().into())
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
        Err(err) => return finalized_error("acquire finalized lease", err),
    };

    let etag = format!("\"block-{inclusion_block}\"");
    if if_none_match(&headers, &etag) {
        // 304: dropping `leased` here releases the lease via its guard.
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let path = state_file_path(&state.snapshot, &leased.prefix);
    let executed_input_count = leased.executed_input_count.get();
    let history = leased.history_version;
    let LeasedDump { guard, .. } = leased;

    match comparison_io(File::open(&path).await, &path) {
        Ok(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::ETAG, etag)
            .header("X-Inclusion-Block", inclusion_block.to_string())
            .header("X-Executed-Input-Count", executed_input_count.to_string())
            .header("X-History-Era", history.era_id.to_string())
            .header(
                "X-Recovery-Generation",
                history.recovery_generation.get().to_string(),
            )
            .body(stream_body(file, guard, path))
            .expect("snapshot response headers are well-formed"),
        // `guard` is a local here; on this error path it drops → lease released.
        Err(err) => internal_error("open finalized state file", err),
    }
}

/// Full app-owned restore artifact, which may still be optimistic.
async fn latest_snapshot(State(state): State<Arc<SnapshotApiState>>) -> Response {
    match acquire_latest(&state).await {
        Ok(Some(leased)) => archive_response(&state, leased, None),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => internal_error("acquire latest snapshot lease", err),
    }
}

/// A self-contained operator backup: immutable restore artifact plus a receipt
/// identifying the accepted canonical boundary, selected under the same lease.
async fn finalized_snapshot(State(state): State<Arc<SnapshotApiState>>) -> Response {
    match acquire_finalized(&state).await {
        Ok(Some(FinalizedLease {
            inclusion_block,
            dump,
        })) => {
            let checkpoint = CheckpointInfo {
                format_version: dump_info::FORMAT_VERSION,
                next_batch_nonce: dump.next_batch_nonce,
                inclusion_block,
            };
            archive_response(&state, dump, Some(checkpoint))
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => finalized_error("acquire accepted snapshot lease", err),
    }
}

fn archive_response(
    state: &SnapshotApiState,
    leased: LeasedDump,
    checkpoint: Option<CheckpointInfo>,
) -> Response {
    let LeasedDump {
        prefix,
        executed_input_count,
        history_version,
        next_batch_nonce,
        guard,
    } = leased;
    let guard = Arc::new(guard);
    let producer_guard = guard.clone();
    let scope = state.shutdown.clone();
    let inclusion_block = checkpoint.as_ref().map(|c| c.inclusion_block);
    let (writer, reader) = tokio::io::duplex(64 * 1024);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _runtime_lifetime = scope;
        let _lease = producer_guard;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dump_info::write_archive(
                SyncIoBridge::new(writer),
                &prefix,
                next_batch_nonce,
                checkpoint.as_ref(),
            )
        }));
        let result = match result {
            Ok(Err(err)) if dump_info::referenced_artifact_io_is_terminal(&err) => {
                abort_terminal(format!(
                    "durable snapshot archive is unusable: {}: {err}",
                    prefix.display()
                ))
            }
            Ok(result) => result,
            Err(_) => abort_terminal("snapshot archive producer panicked"),
        };
        let _ = done_tx.send(result);
    });
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-tar")
        .header("X-History-Era", history_version.era_id.to_string())
        .header(
            "X-Recovery-Generation",
            history_version.recovery_generation.get().to_string(),
        )
        .header(
            "X-Executed-Input-Count",
            executed_input_count.get().to_string(),
        );
    if let Some(block) = inclusion_block {
        response = response.header("X-Inclusion-Block", block.to_string());
    }
    response
        .body(Body::from_stream(ReaderStream::new(GuardedReader {
            file: ArchiveReader {
                reader,
                done: Some(done_rx),
            },
            _guard: guard,
        })))
        .expect("snapshot headers are well-formed")
}

fn state_file_path(state: &SnapshotState, prefix: &Path) -> PathBuf {
    // HTTP request panics are otherwise isolated from the worker supervisor.
    std::panic::catch_unwind(|| (state.state_file_in_dump)(prefix))
        .unwrap_or_else(|_| abort_terminal("application snapshot path callback panicked"))
}

fn stream_body(file: File, guard: LeaseGuard, path: PathBuf) -> Body {
    Body::from_stream(ReaderStream::new(GuardedReader {
        file: ComparisonReader { reader: file, path },
        _guard: Arc::new(guard),
    }))
}

fn comparison_io<T>(result: std::io::Result<T>, path: &Path) -> std::io::Result<T> {
    if let Err(error) = &result
        && dump_info::referenced_artifact_io_is_terminal(error)
    {
        abort_terminal(format!(
            "durable comparison artifact is unusable: {}: {error}",
            path.display()
        ));
    }
    result
}

struct ComparisonReader<R> {
    reader: R,
    path: PathBuf,
}

impl<R: AsyncRead + Unpin> AsyncRead for ComparisonReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.reader)
            .poll_read(cx, buf)
            .map(|result| comparison_io(result, &this.path))
    }
}

/// Do not turn a producer failure into a successful truncated archive response.
struct ArchiveReader {
    reader: tokio::io::DuplexStream,
    done: Option<tokio::sync::oneshot::Receiver<std::io::Result<()>>>,
}

impl AsyncRead for ArchiveReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.reader).poll_read(cx, buf) {
            Poll::Ready(Ok(())) if buf.filled().len() == before => {
                let Some(done) = this.done.as_mut() else {
                    return Poll::Ready(Ok(()));
                };
                match Pin::new(done).poll(cx) {
                    Poll::Ready(result) => {
                        this.done = None;
                        Poll::Ready(result.unwrap_or_else(|_| {
                            Err(std::io::Error::other("snapshot producer terminated"))
                        }))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            other => other,
        }
    }
}

// ── Lease acquisition (storage returns the dump bundled with its release) ──

async fn acquire_finalized(state: &SnapshotApiState) -> Result<Option<FinalizedLease>, BoxError> {
    let db_path = state.snapshot.db_path.clone();
    let release_scheduler = state.release_scheduler.clone();
    storage_task(
        state.shutdown.clone(),
        "acquire finalized snapshot lease",
        move |scope| {
            let report_persistent_failure: crate::storage::PersistentReleaseFailureReporter =
                Arc::new(move |cause: &str| {
                    let _runtime_lifetime = &scope;
                    abort_terminal(cause)
                });
            let mut storage = Storage::open_writer(&db_path)?;
            Ok(storage.acquire_finalized_lease(release_scheduler, report_persistent_failure)?)
        },
    )
    .await
}

async fn acquire_latest(state: &SnapshotApiState) -> Result<Option<LeasedDump>, BoxError> {
    let db_path = state.snapshot.db_path.clone();
    let release_scheduler = state.release_scheduler.clone();
    storage_task(
        state.shutdown.clone(),
        "acquire latest snapshot lease",
        move |scope| {
            let report_persistent_failure: crate::storage::PersistentReleaseFailureReporter =
                Arc::new(move |cause: &str| {
                    let _runtime_lifetime = &scope;
                    abort_terminal(cause)
                });
            let mut storage = Storage::open_writer(&db_path)?;
            Ok(storage
                .acquire_latest_snapshot_lease(release_scheduler, report_persistent_failure)?)
        },
    )
    .await
}

// ── Streaming body that owns the lease guard ───────────────────────────────

/// A file reader that also owns the lease guard. When the response body is
/// dropped — stream completion, I/O error, or client disconnect — the guard
/// drops with it and releases the lease.
struct GuardedReader<R> {
    file: R,
    _guard: Arc<LeaseGuard>,
}

impl<R: AsyncRead + Unpin> AsyncRead for GuardedReader<R> {
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

fn finalized_error(context: &str, err: StorageTaskError) -> Response {
    if matches!(
        err.downcast_ref::<FinalizedSelectionError>(),
        Some(FinalizedSelectionError::CanonicalDivergence)
    ) {
        return ApiError::unavailable(
            "canonical divergence prevents accepted checkpoint selection",
        )
        .into_response();
    }
    internal_error(context, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::temp_db;

    async fn read_corrupt_comparison(path: &Path) {
        let db = temp_db("corrupt-comparison-path");
        let mut storage = Storage::open(&db.path).unwrap();
        storage
            .insert_baseline_snapshot(path, crate::storage::ExecutedInputCount::ZERO)
            .unwrap();
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path,
                state_file_in_dump: Path::to_path_buf,
            },
            shutdown: RuntimeScope::default(),
            release_scheduler: Arc::new(|release| release()),
        });
        let response = finalized_state(State(state), HeaderMap::new()).await;
        // Unix can open a directory successfully: the structural error first
        // appears when the response body reads it.
        let _ = axum::body::to_bytes(response.into_body(), 1024).await;
        panic!("corrupt comparison artifact did not abort");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn comparison_directory_aborts_when_streamed() {
        if !crate::runtime::shutdown::abort_test_child(
            "egress::api::snapshot::tests::comparison_directory_aborts_when_streamed",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        read_corrupt_comparison(root.path()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn comparison_file_parent_aborts_on_open() {
        if !crate::runtime::shutdown::abort_test_child(
            "egress::api::snapshot::tests::comparison_file_parent_aborts_on_open",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("file");
        std::fs::write(&parent, b"not a directory").unwrap();
        read_corrupt_comparison(&parent.join("comparison")).await;
    }

    #[tokio::test]
    async fn comparison_operational_read_error_remains_nonterminal() {
        struct Unavailable;
        impl AsyncRead for Unavailable {
            fn poll_read(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Err(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                )))
            }
        }
        let mut reader = ComparisonReader {
            reader: Unavailable,
            path: PathBuf::from("temporarily-unavailable"),
        };
        let error = tokio::io::AsyncReadExt::read(&mut reader, &mut [0; 1])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    struct BaselineFixture {
        _db: crate::storage::test_helpers::TestDb,
        _root: tempfile::TempDir,
        state: Arc<SnapshotApiState>,
    }

    fn baseline_state(name: &str, contents: &[u8]) -> BaselineFixture {
        let db = temp_db(name);
        let root = tempfile::tempdir().expect("snapshot root");
        let dump_dir = root.path().join("dump");
        std::fs::create_dir(&dump_dir).expect("create dump directory");
        std::fs::write(dump_dir.join("state"), contents).expect("write comparison file");
        let mut storage = Storage::open(&db.path).expect("open storage");
        storage
            .insert_baseline_snapshot(&dump_dir, crate::storage::ExecutedInputCount::ZERO)
            .expect("insert baseline snapshot");
        drop(storage);
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path.clone(),
                state_file_in_dump: |prefix| prefix.join("state"),
            },
            shutdown: RuntimeScope::default(),
            release_scheduler: Arc::new(|release| release()),
        });
        BaselineFixture {
            _db: db,
            _root: root,
            state,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digest_hashes_the_streamed_comparison_file() {
        let contents = vec![0xab_u8; 3 * 1024 * 1024 + 17];
        let fixture = baseline_state("digest-matches-stream", &contents);
        let state = fixture.state.clone();

        let response = finalized_state_digest(State(state.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("digest body");
        let digest: serde_json::Value = serde_json::from_slice(&body).expect("digest JSON");
        assert_eq!(digest["inclusion_block"], 0);
        assert_eq!(digest["executed_input_count"], 0);

        let streamed = finalized_state(State(state), HeaderMap::new()).await;
        let bytes = axum::body::to_bytes(streamed.into_body(), contents.len() + 1)
            .await
            .expect("state body");
        assert_eq!(bytes.as_ref(), contents.as_slice());
        assert_eq!(
            digest["sha256"],
            alloy_primitives::hex::encode(Sha256::digest(&bytes))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digest_is_absent_without_a_comparable_checkpoint() {
        let db = temp_db("digest-absent");
        drop(Storage::open(&db.path).expect("open storage"));
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path,
                state_file_in_dump: |prefix| prefix.join("state"),
            },
            shutdown: RuntimeScope::default(),
            release_scheduler: Arc::new(|release| release()),
        });
        let response = finalized_state_digest(State(state)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn finalized_state_path_panic_aborts_process() {
        if !crate::runtime::shutdown::abort_test_child(
            "egress::api::snapshot::tests::finalized_state_path_panic_aborts_process",
        ) {
            return;
        }
        let db = temp_db("panicking-snapshot-path");
        let mut storage = Storage::open(&db.path).expect("open storage");
        storage
            .insert_baseline_snapshot(
                Path::new("/tmp/panicking-snapshot-path"),
                crate::storage::ExecutedInputCount::ZERO,
            )
            .expect("insert baseline snapshot");
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
        let result =
            tokio::spawn(async move { finalized_state(State(state), HeaderMap::new()).await })
                .await;
        panic!("snapshot path callback panic did not abort the process: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_snapshot_archives_opaque_state_without_comparison_callback() {
        let db = temp_db("archive-without-comparison-path");
        let root = tempfile::tempdir().expect("snapshot root");
        let dump_dir = root.path().join("dump");
        std::fs::create_dir(&dump_dir).expect("create dump directory");
        std::fs::write(dump_info::app_prefix(&dump_dir), b"opaque restore artifact")
            .expect("write app-owned state");
        dump_info::write_info(&dump_dir, &dump_info::DumpInfo::at_baseline(0))
            .expect("write immutable dump metadata");
        let mut storage = Storage::open(&db.path).expect("open storage");
        storage
            .insert_baseline_snapshot(&dump_dir, crate::storage::ExecutedInputCount::ZERO)
            .expect("insert baseline snapshot");
        drop(storage);
        let state = Arc::new(SnapshotApiState {
            snapshot: SnapshotState {
                db_path: db.path,
                state_file_in_dump: |_| panic!("archive must not ask for comparison bytes"),
            },
            shutdown: RuntimeScope::default(),
            release_scheduler: Arc::new(|release| release()),
        });
        let response = latest_snapshot(State(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/x-tar"
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read complete archive");
        let restored = root.path().join("restored");
        tar::Archive::new(body.as_ref())
            .unpack(&restored)
            .expect("unpack snapshot");
        assert_eq!(
            std::fs::read(dump_info::app_prefix(&restored)).unwrap(),
            b"opaque restore artifact",
        );
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
            .insert_baseline_snapshot(
                Path::new("/tmp/corrupt-finalized"),
                crate::storage::ExecutedInputCount::ZERO,
            )
            .expect("insert finalized snapshot");
        drop(storage);

        let conn = Storage::open_connection(db.path.as_str()).expect("raw connection");
        conn.pragma_update(None, "ignore_check_constraints", "ON")
            .expect("allow corruption fixture");
        conn.execute_batch("DROP TRIGGER trg_snapshot_immutable")
            .unwrap();
        conn.execute(
            "UPDATE snapshots SET executed_input_count = -1 WHERE batch_index IS NULL",
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
            .insert_baseline_snapshot(
                Path::new("/tmp/gate-finalized"),
                crate::storage::ExecutedInputCount::ZERO,
            )
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
            .insert_baseline_snapshot(
                Path::new("/tmp/dangling-finalized"),
                crate::storage::ExecutedInputCount::ZERO,
            )
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

        assert!(!crate::http::persistent_storage_error(&error));
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
            .insert_baseline_snapshot(
                Path::new("/tmp/lease-probe"),
                crate::storage::ExecutedInputCount::ZERO,
            )
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
                state.shutdown.clone(),
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
