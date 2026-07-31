// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Integration tests for CORS on the public write path.
//!
//! Browser dapps call `POST /tx` cross-origin, so the ingress router carries a
//! permissive CORS layer. These tests pin both halves of that contract: the
//! preflight is answered without reaching the handler, and the actual response
//! carries `Access-Control-Allow-Origin` — including on the error paths, since
//! a missing header there would surface in the browser as an opaque network
//! failure instead of the JSON error body.
//!
//! They also pin the *scope*: the egress (internal read) side stays
//! same-origin.

use std::io::ErrorKind;
use std::net::SocketAddr;

use alloy_sol_types::Eip712Domain;
use app_core::application::{MAX_METHOD_PAYLOAD_BYTES, WalletApp};
use reqwest::Method;
use reqwest::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN,
};
use sequencer::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use sequencer::http::{self, ApiConfig};
use sequencer::ingress::inclusion_lane::PendingUserOp;
use sequencer::ingress::inclusion_lane::dump_info;
use sequencer::storage::Storage;
use sequencer_core::application::Application;
use tokio::sync::mpsc;

mod common;
use common::temp_db;

const TEST_ORIGIN: &str = "https://dapp.example";

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
    _shutdown: sequencer::runtime::shutdown::ShutdownSignal,
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
            eprintln!("skipping CORS test: cannot bind test listener");
            return None;
        }
        Err(err) => panic!("bind test listener: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    // Capacity 1 with no consumer: every test here is answered by CORS or by
    // request validation, so nothing should ever reach the inclusion lane.
    let (tx_sender, _rx) = mpsc::channel::<PendingUserOp>(1);
    let shutdown = sequencer::runtime::shutdown::ShutdownSignal::default();
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

#[tokio::test]
async fn preflight_on_tx_is_allowed_from_any_origin() {
    let db = temp_db("cors-preflight");
    Storage::open(db.path.as_str()).expect("open storage");
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let response = reqwest::Client::new()
        .request(Method::OPTIONS, server.url("/tx"))
        .header(ORIGIN, TEST_ORIGIN)
        .header(ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
        .send()
        .await
        .expect("preflight request");

    assert!(
        response.status().is_success(),
        "preflight must not fall through to the router's 405 for OPTIONS: {}",
        response.status()
    );
    let headers = response.headers();
    assert_eq!(
        headers
            .get(ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|value| value.to_str().expect("ascii header")),
        Some("*"),
    );
    let allow_methods = headers
        .get(ACCESS_CONTROL_ALLOW_METHODS)
        .expect("allow-methods on preflight")
        .to_str()
        .expect("ascii header");
    assert!(
        allow_methods.contains("POST"),
        "preflight must advertise POST, got {allow_methods}"
    );
    assert!(
        headers.contains_key(ACCESS_CONTROL_ALLOW_HEADERS),
        "preflight must echo an allow-headers value so content-type passes"
    );
    assert!(
        headers.contains_key(ACCESS_CONTROL_MAX_AGE),
        "preflight should be cacheable"
    );
}

/// The error paths matter more than the happy path here: without the header on
/// a 400, the browser reports a CORS failure and the caller never sees the
/// `BAD_REQUEST` body.
#[tokio::test]
async fn tx_error_response_carries_allow_origin() {
    let db = temp_db("cors-error-path");
    Storage::open(db.path.as_str()).expect("open storage");
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let response = reqwest::Client::new()
        .post(server.url("/tx"))
        .header(ORIGIN, TEST_ORIGIN)
        .header("content-type", "application/json")
        .body("{\"not\":\"a user op\"}")
        .send()
        .await
        .expect("post request");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get(ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|value| value.to_str().expect("ascii header")),
        Some("*"),
    );
}

/// Egress is the internal read path; the CORS layer is deliberately scoped to
/// ingress. If a public browser-facing read endpoint is ever added there, this
/// assertion is the one to revisit.
#[tokio::test]
async fn egress_routes_are_not_cors_enabled() {
    let db = temp_db("cors-egress-scope");
    Storage::open(db.path.as_str()).expect("open storage");
    let Some(server) = start_server(db.path.as_str()).await else {
        return;
    };

    let response = reqwest::Client::new()
        .get(server.url("/livez"))
        .header(ORIGIN, TEST_ORIGIN)
        .send()
        .await
        .expect("livez request");

    assert!(response.status().is_success());
    assert!(
        !response.headers().contains_key(ACCESS_CONTROL_ALLOW_ORIGIN),
        "egress must stay same-origin"
    );
}
