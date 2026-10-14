// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

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
