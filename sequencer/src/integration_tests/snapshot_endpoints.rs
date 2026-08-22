// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Integration tests for the operator-only snapshot HTTP endpoints:
//! `/finalized_state`, `/finalized_state/inclusion_block`, `/latest_snapshot`.
//!
//! These drive a real listener (so client disconnect is modelled faithfully)
//! and assert the lease lifecycle — held while streaming, released on
//! completion, 304, and crucially on mid-stream client disconnect.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use alloy_sol_types::Eip712Domain;
use app_core::application::{MAX_METHOD_PAYLOAD_BYTES, WalletApp};
use futures_util::StreamExt;
use sequencer::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use sequencer::http::{self, ApiConfig};
use sequencer::ingress::inclusion_lane::PendingUserOp;
use sequencer::ingress::inclusion_lane::dump_info;
use sequencer::runtime::shutdown::RuntimeScope;
use sequencer::storage::Storage;
use sequencer_core::application::Application;
use tokio::sync::mpsc;

use super::common::temp_db;

fn dummy_domain() -> Eip712Domain {
    Eip712Domain {
        name: None,
        version: None,
        chain_id: None,
        verifying_contract: None,
        salt: None,
    }
}

/// Holds the server task + channel alive for the duration of a test.
struct TestServer {
    addr: SocketAddr,
    _rx: mpsc::Receiver<PendingUserOp>,
    _shutdown: RuntimeScope,
    _task: http::ApiServerTask,
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

async fn start_server(db_path: &str) -> Option<TestServer> {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(value) => value,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping snapshot endpoint test: cannot bind test listener");
            return None;
        }
        Err(err) => panic!("bind test listener: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let (tx_sender, _rx) = mpsc::channel::<PendingUserOp>(1);
    let shutdown = RuntimeScope::default();
    let tx_feed = L2TxFeed::new(
        db_path.to_string(),
        shutdown.clone(),
        L2TxFeedConfig::default(),
    );
    let task = http::start_on_listener(
        listener,
        tx_sender,
        dummy_domain(),
        MAX_METHOD_PAYLOAD_BYTES,
        shutdown.clone(),
        tx_feed,
        ApiConfig::default(),
        http::SnapshotState {
            db_path: db_path.to_string(),
            state_file_in_dump: |dump_dir| {
                WalletApp::state_file_in_dump(&dump_info::app_prefix(dump_dir))
            },
        },
    );
    Some(TestServer {
        addr,
        _rx,
        _shutdown: shutdown,
        _task: task,
    })
}

/// Build a structured dump dir mirroring production layout: the app's
/// state under `state`, plus a minimal `info.toml`.
fn write_state(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let dump_dir = dir.join(name);
    let app_prefix = dump_info::app_prefix(&dump_dir);
    std::fs::create_dir_all(&app_prefix).expect("mkdir dump");
    std::fs::write(WalletApp::state_file_in_dump(&app_prefix), bytes).expect("write state file");
    dump_info::write_info(
        &dump_dir,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce: 0,
            l2_tx_index: 0,
            promoted_inclusion_block: None,
        },
    )
    .expect("write info.toml");
    dump_dir
}

fn register_finalized(
    db_path: &str,
    dir: &Path,
    name: &str,
    bytes: &[u8],
    inclusion_block: u64,
    l2_tx_index: u64,
) -> i64 {
    let prefix = write_state(dir, name, bytes);
    Storage::open(db_path)
        .expect("open")
        .insert_finalized_dump(&prefix, inclusion_block, l2_tx_index)
        .expect("register finalized")
}

fn register_pending(
    db_path: &str,
    dir: &Path,
    name: &str,
    bytes: &[u8],
    nonce: u64,
    l2_tx_index: u64,
) -> i64 {
    let prefix = write_state(dir, name, bytes);
    Storage::open(db_path)
        .expect("open")
        .insert_pending_dump(&prefix, nonce, l2_tx_index)
        .expect("register pending")
}

/// Transient WAL lock contention vs. a real read failure.
///
/// SQLite readers can see `SQLITE_BUSY` in WAL mode during last-connection
/// cleanup / checkpoint (see
/// <https://sqlite.org/wal.html#sometimes_queries_return_sqlite_busy_in_wal_mode>).
/// Production RO opens use a 50ms `busy_timeout` by design; tests must retry
/// that case rather than `.expect` through it. Non-lock failures are preserved
/// so diagnostics are not swallowed.
enum LeaseReadError {
    Busy,
    Other(String),
}

impl std::fmt::Display for LeaseReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "database busy"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

fn sqlite_is_lock_contention(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn open_is_lock_contention(err: &sequencer::storage::StorageOpenError) -> bool {
    match err {
        sequencer::storage::StorageOpenError::Sqlite(e) => sqlite_is_lock_contention(e),
        _ => false,
    }
}

fn try_lease_count(db_path: &str, dump_id: i64) -> Result<i64, LeaseReadError> {
    let mut storage = match Storage::open_read_only(db_path) {
        Ok(storage) => storage,
        Err(err) if open_is_lock_contention(&err) => return Err(LeaseReadError::Busy),
        Err(err) => return Err(LeaseReadError::Other(err.to_string())),
    };
    match storage.dump_lease_count(dump_id) {
        Ok(n) => Ok(n.unwrap_or(0)),
        Err(err) if sqlite_is_lock_contention(&err) => Err(LeaseReadError::Busy),
        Err(err) => Err(LeaseReadError::Other(err.to_string())),
    }
}

fn lease_count(db_path: &str, dump_id: i64) -> i64 {
    for _ in 0..50 {
        match try_lease_count(db_path, dump_id) {
            Ok(n) => return n,
            Err(LeaseReadError::Busy) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => panic!("read lease: {err}"),
        }
    }
    match try_lease_count(db_path, dump_id) {
        Ok(n) => n,
        Err(err) => panic!("read lease: {err}"),
    }
}

async fn wait_for_lease(db_path: &str, dump_id: i64, want: i64) -> bool {
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        match try_lease_count(db_path, dump_id) {
            Ok(got) if got == want => return true,
            // Still waiting for the drop-guard release, or transient WAL
            // cleanup busy — keep polling.
            Ok(_) | Err(LeaseReadError::Busy) => {}
            Err(err) => panic!("read lease: {err}"),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    match try_lease_count(db_path, dump_id) {
        Ok(got) => got == want,
        Err(LeaseReadError::Busy) => false,
        Err(err) => panic!("read lease: {err}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_round_trips_bytes_and_headers() {
    let db = temp_db("snap-finalized-roundtrip");
    let dir = tempfile::tempdir().expect("dumps dir");
    let state = b"the-canonical-finalized-state".to_vec();
    let dump_id = register_finalized(db.path.as_str(), dir.path(), "fin", &state, 4242, 7);
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::Client::new()
        .get(server.url("/finalized_state"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["content-type"], "application/octet-stream");
    assert_eq!(resp.headers()["x-inclusion-block"], "4242");
    assert_eq!(resp.headers()["x-l2-tx-index"], "7");
    assert_eq!(resp.headers()["etag"], "\"block-4242\"");
    let body = resp.bytes().await.expect("body");
    assert_eq!(
        body.as_ref(),
        state.as_slice(),
        "served bytes match the dump"
    );

    assert!(
        wait_for_lease(db.path.as_str(), dump_id, 0).await,
        "lease released after the stream completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_inclusion_block_returns_cheap_json() {
    let db = temp_db("snap-inclusion-block");
    let dir = tempfile::tempdir().expect("dumps dir");
    register_finalized(db.path.as_str(), dir.path(), "fin", b"x", 999, 42);
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::get(server.url("/finalized_state/inclusion_block"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.bytes().await.expect("body");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"inclusion_block\":999"), "got: {text}");
    assert!(text.contains("\"l2_tx_index\":42"), "got: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoints_404_when_no_finalized_snapshot() {
    let db = temp_db("snap-404");
    // Migrate the schema (as startup does) but register no snapshot: the
    // endpoints then see an empty `finalized_snapshot` and return 404.
    Storage::open(db.path.as_str()).expect("init schema");
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let finalized = reqwest::get(server.url("/finalized_state"))
        .await
        .expect("request");
    assert_eq!(finalized.status().as_u16(), 404);

    let cheap = reqwest::get(server.url("/finalized_state/inclusion_block"))
        .await
        .expect("request");
    assert_eq!(cheap.status().as_u16(), 404);

    let latest = reqwest::get(server.url("/latest_snapshot"))
        .await
        .expect("request");
    assert_eq!(latest.status().as_u16(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_if_none_match_returns_304() {
    let db = temp_db("snap-etag");
    let dir = tempfile::tempdir().expect("dumps dir");
    let dump_id = register_finalized(db.path.as_str(), dir.path(), "fin", b"abc", 500, 3);
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::Client::new()
        .get(server.url("/finalized_state"))
        .header("If-None-Match", "\"block-500\"")
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 304);
    let body = resp.bytes().await.expect("body");
    assert!(body.is_empty(), "304 has no body");

    assert!(
        wait_for_lease(db.path.as_str(), dump_id, 0).await,
        "the briefly-held lease is released even on a 304"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_snapshot_prefers_pending() {
    let db = temp_db("snap-latest-pending");
    let dir = tempfile::tempdir().expect("dumps dir");
    register_finalized(
        db.path.as_str(),
        dir.path(),
        "fin",
        b"finalized-bytes",
        100,
        5,
    );
    let pending_id = register_pending(
        db.path.as_str(),
        dir.path(),
        "pend",
        b"pending-bytes",
        3,
        11,
    );
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::get(server.url("/latest_snapshot"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["x-l2-tx-index"], "11");
    let body = resp.bytes().await.expect("body");
    assert_eq!(
        body.as_ref(),
        b"pending-bytes",
        "serves the latest pending, not the finalized fallback"
    );

    assert!(wait_for_lease(db.path.as_str(), pending_id, 0).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_streams_large_file() {
    let db = temp_db("snap-large");
    let dir = tempfile::tempdir().expect("dumps dir");
    let big = vec![0x5Au8; 8 * 1024 * 1024];
    let dump_id = register_finalized(db.path.as_str(), dir.path(), "big", &big, 1, 1);
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::get(server.url("/finalized_state"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.bytes().await.expect("body");
    assert_eq!(body.len(), big.len(), "full large file streamed");
    assert_eq!(body.as_ref(), big.as_slice());

    assert!(wait_for_lease(db.path.as_str(), dump_id, 0).await);
}

/// The test that justifies the drop-guard: a client that disconnects
/// mid-stream must still release the lease. A linear `acquire; await; release`
/// would skip the release on cancellation and leak the lease for the process
/// lifetime, permanently pinning the dump against GC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_disconnect_mid_stream_releases_lease() {
    let db = temp_db("snap-disconnect");
    let dir = tempfile::tempdir().expect("dumps dir");
    // 8 MiB so the body is still streaming (not fully in the socket buffer)
    // when we disconnect.
    let big = vec![0xABu8; 8 * 1024 * 1024];
    let dump_id = register_finalized(db.path.as_str(), dir.path(), "big", &big, 7, 9);
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::Client::new()
        .get(server.url("/finalized_state"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    // Lease is held for the duration of the stream (X semantics).
    assert_eq!(
        lease_count(db.path.as_str(), dump_id),
        1,
        "lease held while streaming"
    );

    // Read one chunk, then drop the response mid-stream — a client disconnect.
    let mut stream = resp.bytes_stream();
    let _first = stream
        .next()
        .await
        .expect("at least one chunk")
        .expect("chunk ok");
    drop(stream);

    assert!(
        wait_for_lease(db.path.as_str(), dump_id, 0).await,
        "drop-guard must release the lease on disconnect (still {})",
        lease_count(db.path.as_str(), dump_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_concurrent_streams_count_and_release_independently() {
    let db = temp_db("snap-concurrent");
    let dir = tempfile::tempdir().expect("dumps dir");
    let big = vec![0xCDu8; 8 * 1024 * 1024];
    let dump_id = register_finalized(db.path.as_str(), dir.path(), "big", &big, 1, 1);
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    // Two concurrent readers of the same finalized dump.
    let client = reqwest::Client::new();
    let r1 = client
        .get(server.url("/finalized_state"))
        .send()
        .await
        .expect("request 1");
    let r2 = client
        .get(server.url("/finalized_state"))
        .send()
        .await
        .expect("request 2");
    assert_eq!(r1.status().as_u16(), 200);
    assert_eq!(r2.status().as_u16(), 200);
    let mut s1 = r1.bytes_stream();
    let mut s2 = r2.bytes_stream();
    s1.next().await.expect("chunk 1").expect("ok");
    s2.next().await.expect("chunk 2").expect("ok");

    assert!(
        wait_for_lease(db.path.as_str(), dump_id, 2).await,
        "two live streams hold lease_count == 2 (got {})",
        lease_count(db.path.as_str(), dump_id)
    );

    drop(s1);
    assert!(
        wait_for_lease(db.path.as_str(), dump_id, 1).await,
        "dropping one stream steps the lease down to 1"
    );
    drop(s2);
    assert!(
        wait_for_lease(db.path.as_str(), dump_id, 0).await,
        "dropping the second steps it to 0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_stream_blocks_gc_until_it_drops() {
    let db = temp_db("snap-stream-blocks-gc");
    let dir = tempfile::tempdir().expect("dumps dir");
    let big = vec![0xABu8; 8 * 1024 * 1024];
    let served_id = register_finalized(db.path.as_str(), dir.path(), "served", &big, 1, 1);
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    // Open a stream and keep it alive.
    let resp = reqwest::Client::new()
        .get(server.url("/finalized_state"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    let mut stream = resp.bytes_stream();
    stream.next().await.expect("chunk").expect("ok");
    assert!(wait_for_lease(db.path.as_str(), served_id, 1).await);

    // Promote a new finalized so the dump being streamed becomes unreferenced.
    {
        let prefix = write_state(dir.path(), "new", b"new-finalized");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        storage.insert_pending_dump(&prefix, 0, 2).expect("pending");
        storage.promote_finalized(0, 2).expect("promote");
    }

    // GC must skip the superseded dump while the stream holds its lease.
    let removed = {
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        storage.gc_unreferenced_dumps().expect("gc")
    };
    assert!(
        !removed.iter().any(|row| row.id == served_id),
        "live stream's lease must block GC of the superseded dump"
    );
    assert_eq!(lease_count(db.path.as_str(), served_id), 1, "still leased");

    // Drop the stream → lease released → the next GC reaps it.
    drop(stream);
    assert!(wait_for_lease(db.path.as_str(), served_id, 0).await);
    let removed_after = {
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        storage.gc_unreferenced_dumps().expect("gc")
    };
    assert!(
        removed_after.iter().any(|row| row.id == served_id),
        "GC reaps the dump once the stream releases its lease"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_file_open_failure_releases_lease() {
    let db = temp_db("snap-open-fail");
    let dir = tempfile::tempdir().expect("dumps dir");
    let dump_id = register_finalized(db.path.as_str(), dir.path(), "fin", b"bytes", 1, 1);
    // Delete the state file out from under the registered row to force the
    // post-acquire `File::open` error path.
    std::fs::remove_file(WalletApp::state_file_in_dump(&dump_info::app_prefix(
        &dir.path().join("fin"),
    )))
    .expect("remove state file");
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::get(server.url("/finalized_state"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 500);

    assert!(
        wait_for_lease(db.path.as_str(), dump_id, 0).await,
        "lease acquired before the open must still be released when the open fails"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_snapshot_falls_back_to_finalized_when_no_pending() {
    let db = temp_db("snap-latest-fallback");
    let dir = tempfile::tempdir().expect("dumps dir");
    let fin_id = register_finalized(
        db.path.as_str(),
        dir.path(),
        "fin",
        b"finalized-bytes",
        100,
        5,
    );
    // No pending registered.
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::get(server.url("/latest_snapshot"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["x-l2-tx-index"], "5");
    let body = resp.bytes().await.expect("body");
    assert_eq!(body.as_ref(), b"finalized-bytes", "falls back to finalized");

    assert!(wait_for_lease(db.path.as_str(), fin_id, 0).await);
}

#[tokio::test]
async fn cors_permits_browser_preflight_on_tx() {
    let db = temp_db("cors-preflight");
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let resp = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, server.url("/tx"))
        .header("Origin", "https://wallet.example")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type")
        .send()
        .await
        .expect("OPTIONS /tx");

    assert!(
        resp.status().is_success(),
        "preflight status: {}",
        resp.status()
    );
    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin")
        .expect("Access-Control-Allow-Origin")
        .to_str()
        .expect("header utf8");
    assert_eq!(allow_origin, "*");
}
