// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared fixtures for crate-internal integration-style tests.

use tempfile::TempDir;

pub struct TestDb {
    pub _dir: TempDir,
    pub path: String,
}

pub fn temp_db(name: &str) -> TestDb {
    let dir = tempfile::Builder::new()
        .prefix(format!("sequencer-{name}-").as_str())
        .tempdir()
        .expect("create temporary test directory");
    let path = dir.path().join("sequencer.sqlite");
    TestDb {
        _dir: dir,
        path: path.to_string_lossy().into_owned(),
    }
}

/// Two ephemeral loopback listeners; returns (listeners, public, internal).
pub(crate) async fn bind_api_listeners() -> (
    crate::http::ApiListeners,
    std::net::SocketAddr,
    std::net::SocketAddr,
) {
    let public = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind public listener");
    let internal = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind internal listener");
    let (public_addr, internal_addr) = (
        public.local_addr().expect("public addr"),
        internal.local_addr().expect("internal addr"),
    );
    (
        crate::http::ApiListeners { public, internal },
        public_addr,
        internal_addr,
    )
}
