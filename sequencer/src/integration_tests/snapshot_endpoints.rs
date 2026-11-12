// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Operator snapshot endpoints over a real listener: the lease is held while
//! streaming and released on completion, on 304, and on mid-stream disconnect.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use app_core::application::WalletApp;
use futures_util::StreamExt;
use sequencer::ingress::inclusion_lane::dump_info;
use sequencer::storage::Storage;
use sequencer_core::application::{Application, CanonicalState};

use crate::test_support::server::{PUBLIC_ROUTES, TestServer};
use crate::test_support::temp_db;

/// Build a structured dump dir mirroring production layout: the app's
/// state under `state`, plus a minimal `info.toml`.
fn write_state(dir: &Path, name: &str, bytes: &[u8], next_batch_nonce: u64) -> std::path::PathBuf {
    let dump_dir = dir.join(name);
    let app_prefix = dump_info::app_prefix(&dump_dir);
    std::fs::create_dir_all(&app_prefix).expect("mkdir dump");
    std::fs::write(WalletApp::state_file_in_dump(&app_prefix), bytes).expect("write state file");
    dump_info::write_info(
        &dump_dir,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce,
        },
    )
    .expect("write info.toml");
    dump_dir
}

fn register_accepted(
    db_path: &str,
    dir: &Path,
    name: &str,
    bytes: &[u8],
    inclusion_block: u64,
) -> i64 {
    let mut storage = Storage::open(db_path).unwrap();
    let sender = alloy_primitives::Address::repeat_byte(0x7f);
    // A test registers several accepted batches on one database.
    if storage.deployment_identity().unwrap().is_none() {
        crate::test_support::pin_test_deployment_identity(&mut storage, sender);
    }
    let mut head = storage.open_state().unwrap().unwrap_or_else(|| {
        storage
            .initialize_open_state(inclusion_block, crate::storage::SafeInputRange::empty_at(0))
            .unwrap()
    });
    let nonce = head.batch_nonce();
    let prefix = write_state(dir, name, bytes, nonce + 1);
    storage
        .close_frame_and_batch_with_snapshot(&mut head, &prefix, crate::storage::L2TxCount::ZERO)
        .unwrap();
    crate::storage::test_helpers::seed_safe_inputs_with_batch_nonces(
        &mut storage,
        sender,
        inclusion_block,
        &[nonce],
    );
    storage.finalized_dump().unwrap().unwrap().snapshot.id
}

fn register_optimistic(db_path: &str, dir: &Path, name: &str, bytes: &[u8], nonce: u64) -> i64 {
    let mut storage = Storage::open(db_path).unwrap();
    let mut head = storage.open_state().unwrap().unwrap();
    while head.batch_nonce() <= nonce {
        let current = head.batch_nonce();
        let dump_name = if current == nonce {
            name.to_owned()
        } else {
            format!("intermediate-{current}")
        };
        let prefix = write_state(dir, &dump_name, bytes, current + 1);
        storage
            .close_frame_and_batch_with_snapshot(
                &mut head,
                &prefix,
                crate::storage::L2TxCount::ZERO,
            )
            .unwrap();
    }
    storage.latest_snapshot().unwrap().unwrap().id
}

/// The response's `X-` headers, sorted: the claim travels in the era and the
/// count, and nothing else names the history.
fn history_headers(response: &reqwest::Response) -> Vec<&str> {
    let mut names: Vec<_> = response
        .headers()
        .keys()
        .map(|name| name.as_str())
        .filter(|name| name.starts_with("x-"))
        .collect();
    names.sort_unstable();
    names
}

fn archive_state(bytes: &[u8]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    tar::Archive::new(bytes).unpack(dir.path()).unwrap();
    std::fs::read(WalletApp::state_file_in_dump(&dump_info::app_prefix(
        dir.path(),
    )))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_block_divergence_refuses_every_finalized_endpoint() {
    use crate::storage::test_helpers::local_batch_payload;
    use crate::storage::{L2TxCount, SafeInputRange, StoredSafeInput};
    use crate::test_support::pin_test_deployment_identity;
    use sequencer_core::batch::{Batch, Frame};
    use sequencer_core::scheduler::{CanonicalRunner, L1Input, SchedulerConfig};
    use ssz::Encode;

    let db = temp_db("same-block-divergence-http");
    let root = tempfile::tempdir().unwrap();
    let submitter = alloy_primitives::Address::repeat_byte(1);
    let mut storage = Storage::open(&db.path).unwrap();
    pin_test_deployment_identity(&mut storage, submitter);
    let mut head = storage
        .initialize_open_state(5, SafeInputRange::empty_at(0))
        .unwrap();
    let prefix = write_state(root.path(), "local", b"local checkpoint", 1);
    storage
        .close_frame_and_batch_with_snapshot(&mut head, &prefix, L2TxCount::ZERO)
        .unwrap();
    let snapshot = storage.latest_snapshot().unwrap().unwrap();
    let inputs = vec![
        StoredSafeInput {
            sender: alloy_primitives::Address::repeat_byte(2),
            payload: vec![42],
            block_number: 6,
        },
        StoredSafeInput {
            sender: submitter,
            payload: local_batch_payload(&mut storage, 0),
            block_number: 10,
        },
        StoredSafeInput {
            sender: submitter,
            payload: Batch {
                nonce: 1,
                frames: vec![Frame {
                    safe_block: 6,
                    fee_price: 0,
                    user_ops: vec![],
                }],
            }
            .as_ssz_bytes(),
            block_number: 10,
        },
    ];
    storage.append_safe_inputs(10, &inputs, submitter).unwrap();
    assert!(storage.canonical_divergence().unwrap().is_some());

    // The foreign batch consumes the next nonce and drains a direct that the
    // matching local batch did not: its snapshot is not the end of block 10.
    let mut canonical = CanonicalRunner::genesis(
        WalletApp::default(),
        SchedulerConfig::for_application::<WalletApp>(submitter),
    );
    for (index, input) in (0_u64..).zip(inputs) {
        canonical
            .process_input(L1Input {
                index,
                sender: input.sender,
                block: input.block_number,
                timestamp: 0,
                chain_id: 1,
                app_contract: alloy_primitives::Address::ZERO,
                payload: input.payload,
            })
            .unwrap();
    }
    let export = canonical.export();
    assert_eq!(export.next_batch_nonce, 2);
    assert_eq!(export.l2_tx_count, 1);
    assert_eq!(export.fridge_len, 0);
    assert_eq!(snapshot.l2_tx_count, L2TxCount::ZERO);

    let server = TestServer::start(&db.path).await;
    let client = reqwest::Client::new();
    for (path, conditional) in [
        ("/finalized_state/inclusion_block", false),
        ("/finalized_state", false),
        ("/finalized_state", true),
        ("/finalized_snapshot", false),
    ] {
        let mut request = client.get(server.url(path));
        if conditional {
            request = request.header("If-None-Match", "\"block-10\"");
        }
        let response = request.send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "{path}, conditional={conditional}"
        );
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["code"],
            "UNAVAILABLE"
        );
        assert_eq!(lease_count(&server, snapshot.id), 0);
    }
}

/// The server's lease count on one snapshot; a missing entry counts as zero.
fn lease_count(server: &TestServer, snapshot_id: i64) -> u32 {
    server.snapshot_leases.count(snapshot_id).unwrap_or(0)
}

/// The server drops a body, and its lease, after the client's last read.
async fn wait_for_lease(server: &TestServer, snapshot_id: i64, want: u32) -> bool {
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        if lease_count(server, snapshot_id) == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    lease_count(server, snapshot_id) == want
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_round_trips_bytes_and_headers() {
    let db = temp_db("snap-finalized-roundtrip");
    let dir = tempfile::tempdir().expect("dumps dir");
    let state = b"the-canonical-finalized-state".to_vec();
    let snapshot_id = register_accepted(db.path.as_str(), dir.path(), "fin", &state, 4242);
    let server = TestServer::start(db.path.as_str()).await;

    let resp = reqwest::Client::new()
        .get(server.url("/finalized_state"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["content-type"], "application/octet-stream");
    assert_eq!(resp.headers()["x-inclusion-block"], "4242");
    assert_eq!(resp.headers()["x-executed-input-count"], "0");
    assert!(resp.headers().contains_key("x-history-era"));
    assert_eq!(
        history_headers(&resp),
        [
            "x-executed-input-count",
            "x-history-era",
            "x-inclusion-block"
        ]
    );
    assert_eq!(resp.headers()["etag"], "\"block-4242\"");
    let body = resp.bytes().await.expect("body");
    assert_eq!(
        body.as_ref(),
        state.as_slice(),
        "served bytes match the dump"
    );

    assert!(
        wait_for_lease(&server, snapshot_id, 0).await,
        "lease released after the stream completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_inclusion_block_returns_cheap_json() {
    let db = temp_db("snap-inclusion-block");
    let dir = tempfile::tempdir().expect("dumps dir");
    register_accepted(db.path.as_str(), dir.path(), "fin", b"x", 999);
    let server = TestServer::start(db.path.as_str()).await;

    let resp = reqwest::get(server.url("/finalized_state/inclusion_block"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.bytes().await.expect("body");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"inclusion_block\":999"), "got: {text}");
    assert!(text.contains("\"executed_input_count\":0"), "got: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoints_404_when_no_finalized_snapshot() {
    let db = temp_db("snap-404");
    // Migrate the schema (as startup does) but register no snapshot: the
    // endpoints then see no registered snapshot and return 404.
    Storage::open(db.path.as_str()).expect("init schema");
    let server = TestServer::start(db.path.as_str()).await;

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
    let snapshot_id = register_accepted(db.path.as_str(), dir.path(), "fin", b"abc", 500);
    let server = TestServer::start(db.path.as_str()).await;

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
        wait_for_lease(&server, snapshot_id, 0).await,
        "the briefly-held lease is released even on a 304"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_snapshot_prefers_newest_valid_batch() {
    let db = temp_db("snap-latest-pending");
    let dir = tempfile::tempdir().expect("dumps dir");
    register_accepted(db.path.as_str(), dir.path(), "fin", b"finalized-bytes", 100);
    let pending_id = register_optimistic(db.path.as_str(), dir.path(), "pend", b"pending-bytes", 3);
    let server = TestServer::start(db.path.as_str()).await;

    let resp = reqwest::get(server.url("/latest_snapshot"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["x-executed-input-count"], "0");
    assert!(resp.headers().contains_key("x-history-era"));
    assert_eq!(
        history_headers(&resp),
        ["x-executed-input-count", "x-history-era"]
    );
    let body = resp.bytes().await.expect("body");
    assert_eq!(
        archive_state(body.as_ref()).as_slice(),
        b"pending-bytes",
        "serves the latest pending, not the finalized fallback"
    );

    assert!(wait_for_lease(&server, pending_id, 0).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_streams_large_file() {
    let db = temp_db("snap-large");
    let dir = tempfile::tempdir().expect("dumps dir");
    let big = vec![0x5Au8; 8 * 1024 * 1024];
    let snapshot_id = register_accepted(db.path.as_str(), dir.path(), "big", &big, 1);
    let server = TestServer::start(db.path.as_str()).await;

    let resp = reqwest::get(server.url("/finalized_state"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.bytes().await.expect("body");
    assert_eq!(body.len(), big.len(), "full large file streamed");
    assert_eq!(body.as_ref(), big.as_slice());

    assert!(wait_for_lease(&server, snapshot_id, 0).await);
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
    let snapshot_id = register_accepted(db.path.as_str(), dir.path(), "big", &big, 7);
    let server = TestServer::start(db.path.as_str()).await;

    let resp = reqwest::Client::new()
        .get(server.url("/finalized_state"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    // Lease is held for the duration of the stream (X semantics).
    assert_eq!(
        lease_count(&server, snapshot_id),
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
        wait_for_lease(&server, snapshot_id, 0).await,
        "drop-guard must release the lease on disconnect (still {})",
        lease_count(&server, snapshot_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_state_concurrent_streams_count_and_release_independently() {
    let db = temp_db("snap-concurrent");
    let dir = tempfile::tempdir().expect("dumps dir");
    let big = vec![0xCDu8; 8 * 1024 * 1024];
    let snapshot_id = register_accepted(db.path.as_str(), dir.path(), "big", &big, 1);
    let server = TestServer::start(db.path.as_str()).await;

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
        wait_for_lease(&server, snapshot_id, 2).await,
        "two live streams hold two leases (got {})",
        lease_count(&server, snapshot_id)
    );

    drop(s1);
    assert!(
        wait_for_lease(&server, snapshot_id, 1).await,
        "dropping one stream steps the lease down to 1"
    );
    drop(s2);
    assert!(
        wait_for_lease(&server, snapshot_id, 0).await,
        "dropping the second steps it to 0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_stream_blocks_gc_until_it_drops() {
    let db = temp_db("snap-stream-blocks-gc");
    let dir = tempfile::tempdir().expect("dumps dir");
    let big = vec![0xABu8; 8 * 1024 * 1024];
    let served_id = register_accepted(db.path.as_str(), dir.path(), "served", &big, 1);
    let server = TestServer::start(db.path.as_str()).await;

    // Open a stream and keep it alive.
    let resp = reqwest::Client::new()
        .get(server.url("/finalized_state"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    let mut stream = resp.bytes_stream();
    stream.next().await.expect("chunk").expect("ok");
    assert!(wait_for_lease(&server, served_id, 1).await);

    // Acceptance advances independently of the lane's L1 reconciliation.
    register_accepted(db.path.as_str(), dir.path(), "new", b"new-finalized", 2);

    // GC, sharing the server's registry, must skip the superseded snapshot
    // while the stream holds its lease.
    let served = dir.path().join("served");
    let removed = {
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        storage
            .collect_snapshots(&server.snapshot_leases)
            .expect("gc")
    };
    assert!(
        !removed.contains(&served),
        "live stream's lease must block GC of the superseded snapshot"
    );
    assert_eq!(lease_count(&server, served_id), 1, "still leased");

    // Drop the stream → lease released → the next GC reaps it.
    drop(stream);
    assert!(wait_for_lease(&server, served_id, 0).await);
    let removed_after = {
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        storage
            .collect_snapshots(&server.snapshot_leases)
            .expect("gc")
    };
    assert_eq!(
        removed_after,
        vec![served],
        "GC reaps the snapshot once the stream releases its lease"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn finalized_state_missing_registered_artifact_aborts_process() {
    if !crate::runtime::shutdown::abort_test_child(
        "integration_tests::snapshot_endpoints::finalized_state_missing_registered_artifact_aborts_process",
    ) {
        return;
    }
    let db = temp_db("snap-missing-artifact");
    let dir = tempfile::tempdir().expect("dumps dir");
    register_accepted(db.path.as_str(), dir.path(), "fin", b"bytes", 1);
    // The durable row promises the artifact exists. This is a terminal
    // invariant failure, so the process stops before HTTP or lease cleanup.
    std::fs::remove_file(WalletApp::state_file_in_dump(&dump_info::app_prefix(
        &dir.path().join("fin"),
    )))
    .expect("remove state file");
    let server = TestServer::start(db.path.as_str()).await;

    let _ = reqwest::get(server.url("/finalized_state")).await;
    panic!("missing registered snapshot returned instead of aborting");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_snapshot_restores_accepted_batch_when_it_is_newest() {
    let db = temp_db("snap-latest-fallback");
    let dir = tempfile::tempdir().expect("dumps dir");
    let fin_id = register_accepted(db.path.as_str(), dir.path(), "fin", b"finalized-bytes", 100);
    // No pending registered.
    let server = TestServer::start(db.path.as_str()).await;

    let resp = reqwest::get(server.url("/latest_snapshot"))
        .await
        .expect("request");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["x-executed-input-count"], "0");
    assert!(resp.headers().contains_key("x-history-era"));
    assert_eq!(
        history_headers(&resp),
        ["x-executed-input-count", "x-history-era"]
    );
    let body = resp.bytes().await.expect("body");
    assert_eq!(
        archive_state(body.as_ref()).as_slice(),
        b"finalized-bytes",
        "restores finalized artifact"
    );

    assert!(wait_for_lease(&server, fin_id, 0).await);
}

/// Each route answers only on its own listener: a wrong-method probe gets 405
/// where the route exists and 404 on the other listener, whatever the
/// handler's state.
#[tokio::test]
async fn every_route_is_served_on_exactly_its_own_listener() {
    const INTERNAL_ROUTES: [&str; 10] = [
        "/ws/subscribe",
        "/history",
        "/finalized_state",
        "/finalized_state/inclusion_block",
        "/finalized_state/digest",
        "/latest_snapshot",
        "/finalized_snapshot",
        "/livez",
        "/readyz",
        "/healthz",
    ];
    let db = temp_db("listener-placement");
    let server = TestServer::start(db.path.as_str()).await;
    let client = reqwest::Client::new();
    let probe = |addr: SocketAddr, route: &str| {
        client
            .request(reqwest::Method::DELETE, format!("http://{addr}{route}"))
            .send()
    };
    for (routes, own, other) in [
        (&PUBLIC_ROUTES[..], server.public_addr, server.internal_addr),
        (
            &INTERNAL_ROUTES[..],
            server.internal_addr,
            server.public_addr,
        ),
    ] {
        for route in routes {
            let served = probe(own, route).await.expect("probe own listener");
            assert_eq!(served.status().as_u16(), 405, "{route} on its own listener");
            let absent = probe(other, route).await.expect("probe other listener");
            assert_eq!(
                absent.status().as_u16(),
                404,
                "{route} on the other listener"
            );
        }
    }
}

#[tokio::test]
async fn cors_permits_browser_preflight_on_tx() {
    let db = temp_db("cors-preflight");
    let server = TestServer::start(db.path.as_str()).await;

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
    let allow_methods = resp
        .headers()
        .get("access-control-allow-methods")
        .expect("Access-Control-Allow-Methods")
        .to_str()
        .expect("header utf8");
    assert!(
        allow_methods
            .split(',')
            .any(|method| method.trim() == "POST"),
        "preflight must allow POST, got {allow_methods}"
    );
    assert_eq!(resp.headers()["access-control-allow-headers"], "*");
    assert_eq!(resp.headers()["access-control-max-age"], "3600");
}

#[tokio::test]
async fn cors_permits_browser_preflight_on_fee() {
    let db = temp_db("cors-fee-preflight");
    let server = TestServer::start(db.path.as_str()).await;

    let resp = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, server.url("/fee"))
        .header("Origin", "https://wallet.example")
        .header("Access-Control-Request-Method", "GET")
        .send()
        .await
        .expect("OPTIONS /fee");

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
    let allow_methods = resp
        .headers()
        .get("access-control-allow-methods")
        .expect("Access-Control-Allow-Methods")
        .to_str()
        .expect("header utf8");
    assert!(
        allow_methods
            .split(',')
            .any(|method| method.trim() == "GET"),
        "preflight must allow GET, got {allow_methods}"
    );
}

#[tokio::test]
async fn cors_is_limited_to_ingress_and_covers_rejections() {
    let db = temp_db("cors-route-scope");
    let dumps = tempfile::tempdir().expect("snapshot directory");
    let snapshot_id = register_accepted(&db.path, dumps.path(), "finalized", b"canonical", 1);
    let server = TestServer::start(db.path.as_str()).await;
    let client = reqwest::Client::new();
    let rejected = client
        .post(server.url("/tx"))
        .header("Origin", "https://wallet.example")
        .header("Content-Type", "application/json")
        .body("{")
        .send()
        .await
        .expect("invalid POST /tx");
    assert_eq!(rejected.status().as_u16(), 400);
    assert_eq!(rejected.headers()["access-control-allow-origin"], "*");

    let fee = client
        .get(server.url("/fee"))
        .header("Origin", "https://wallet.example")
        .send()
        .await
        .expect("GET /fee");
    assert_eq!(fee.headers()["access-control-allow-origin"], "*");

    for (route, status) in [
        (
            "/nonce?sender=0x00000000000000000000000000000000000000aa",
            200,
        ),
        ("/nonce?sender=0xaa", 400),
        ("/domain", 200),
    ] {
        let response = client
            .get(server.url(route))
            .header("Origin", "https://wallet.example")
            .send()
            .await
            .expect("ingress read");
        assert_eq!(response.status().as_u16(), status, "{route}");
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "*",
            "{route}"
        );
    }

    for route in ["/livez", "/finalized_state"] {
        let response = client
            .get(server.url(route))
            .header("Origin", "https://wallet.example")
            .send()
            .await
            .expect("egress request");
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none(),
            "egress must not opt into browser cross-origin reads: {route}"
        );
        assert!(response.status().is_success());
        let _ = response.bytes().await.expect("response body");
    }
    assert!(wait_for_lease(&server, snapshot_id, 0).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_snapshot_restores_application_and_recovery_export_is_self_contained() {
    let db = temp_db("snapshot-archive-roundtrip");
    let dir = tempfile::tempdir().unwrap();
    let app = WalletApp::default();
    let encoded = app_core::wallet_snapshot::encode(&app);
    register_accepted(&db.path, dir.path(), "accepted", &encoded, 1);
    let server = TestServer::start(&db.path).await;
    let client =
        sequencer_rust_client::SequencerClient::new(format!("http://{}", server.internal_addr))
            .unwrap();
    let snapshot = client.latest_snapshot().await.unwrap();
    let claim = snapshot.claim;
    assert_eq!(
        snapshot.response.headers()["content-type"],
        "application/x-tar"
    );
    let bytes = snapshot.response.bytes().await.unwrap();
    let restored_dir = tempfile::tempdir().unwrap();
    tar::Archive::new(bytes.as_ref())
        .unpack(restored_dir.path())
        .unwrap();
    let restored = WalletApp::from_dump(&dump_info::app_prefix(restored_dir.path())).unwrap();
    assert_eq!(restored.l2_tx_count(), claim.next_input);
    assert!(!restored_dir.path().join("checkpoint.toml").exists());
    let mut stream = client.subscribe(claim).await.unwrap();
    stream.close(None).await.unwrap();

    let export = reqwest::get(server.url("/finalized_snapshot"))
        .await
        .unwrap();
    assert_eq!(export.headers()["x-inclusion-block"], "1");
    assert_eq!(
        history_headers(&export),
        [
            "x-executed-input-count",
            "x-history-era",
            "x-inclusion-block"
        ]
    );
    assert_eq!(
        export.headers()["x-history-era"].to_str().unwrap(),
        claim.era_id.to_string()
    );
    let export_dir = tempfile::tempdir().unwrap();
    tar::Archive::new(export.bytes().await.unwrap().as_ref())
        .unpack(export_dir.path())
        .unwrap();
    let receipt = dump_info::read_checkpoint_info(export_dir.path()).unwrap();
    assert_eq!(receipt.inclusion_block, 1);
    assert_eq!(receipt.next_batch_nonce, 1);
    assert_eq!(
        dump_info::read_info(export_dir.path())
            .unwrap()
            .next_batch_nonce,
        receipt.next_batch_nonce
    );
    let exported = WalletApp::from_dump(&dump_info::app_prefix(export_dir.path())).unwrap();
    assert_eq!(exported.l2_tx_count(), app.l2_tx_count());
    assert_eq!(
        exported.canonical_snapshot_bytes().unwrap(),
        app.canonical_snapshot_bytes().unwrap()
    );
    assert!(
        !dir.path().join("accepted/checkpoint.toml").exists(),
        "export must leave immutable local artifacts unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_disconnect_releases_both_stream_and_producer_lease_owners() {
    let db = temp_db("archive-disconnect");
    let dir = tempfile::tempdir().unwrap();
    let id = register_accepted(&db.path, dir.path(), "large", &vec![1; 8 * 1024 * 1024], 1);
    let server = TestServer::start(&db.path).await;
    let response = reqwest::get(server.url("/latest_snapshot")).await.unwrap();
    let mut stream = response.bytes_stream();
    stream.next().await.unwrap().unwrap();
    assert!(wait_for_lease(&server, id, 1).await);
    drop(stream);
    assert!(
        wait_for_lease(&server, id, 0).await,
        "disconnected archive producer must terminate and release its lease"
    );
}
