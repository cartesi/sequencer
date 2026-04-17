// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! TCP proxy with programmatic `disconnect()` / `reconnect()` for outage
//! simulation in tests.
//!
//! Layout:
//!
//! ```text
//! Sequencer ──→ TcpProxy (127.0.0.1:proxy_port) ──→ Anvil (upstream)
//!                       ↑
//!                 disconnect() / reconnect()
//!                 controlled from test code
//! ```
//!
//! Behavior:
//!
//! - `disconnect()` flips an internal flag. All existing forwarded connections
//!   are torn down (their forwarding tasks observe the flag and exit, dropping
//!   the sockets). New connection attempts still succeed at the TCP accept
//!   level, but are immediately closed. To the sequencer, this looks like the
//!   upstream aggressively resets every connection — the same client-visible
//!   behavior as a node that went down.
//!
//! - `reconnect()` flips the flag back. Subsequent connections forward
//!   normally; the sequencer's next retry after backoff reconnects as if the
//!   upstream is back.
//!
//! - Anvil (the real upstream) stays running behind the proxy the whole time,
//!   so the test can bypass the proxy to mine blocks on it directly via a
//!   separate client connected to the upstream port. That's how we simulate
//!   "L1 advanced while the sequencer's gateway was down."
//!
//! The proxy listens on `127.0.0.1:0` by default, picking an ephemeral port
//! the OS hands out; the actual port is read back via `endpoint()`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::HarnessResult;
use crate::util::io_other;

/// A programmable TCP proxy for L1 RPC outage simulation.
///
/// Construct with [`TcpProxy::spawn`]. Flip the outage flag via
/// [`TcpProxy::disconnect`] / [`TcpProxy::reconnect`]. Retrieve the HTTP
/// endpoint for the sequencer to connect to via [`TcpProxy::endpoint`].
pub struct TcpProxy {
    listen_addr: SocketAddr,
    upstream_addr: SocketAddr,
    connected: Arc<AtomicBool>,
    accept_task: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl TcpProxy {
    /// Spawn a proxy forwarding to `upstream_url` (e.g., `http://127.0.0.1:8545`).
    ///
    /// The proxy binds `127.0.0.1:0` (an ephemeral port) and starts accepting
    /// immediately. Use [`Self::endpoint`] to get the `http://127.0.0.1:<port>`
    /// URL for the sequencer to connect to.
    pub async fn spawn(upstream_url: &str) -> HarnessResult<Self> {
        let upstream_addr = parse_http_upstream(upstream_url)?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|err| io_other(format!("proxy bind failed: {err}")))?;
        let listen_addr = listener
            .local_addr()
            .map_err(|err| io_other(format!("proxy local_addr failed: {err}")))?;

        let connected = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));

        let accept_task = {
            let connected = connected.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                accept_loop(listener, upstream_addr, connected, shutdown).await;
            })
        };

        Ok(Self {
            listen_addr,
            upstream_addr,
            connected,
            accept_task: Some(accept_task),
            shutdown,
        })
    }

    /// HTTP URL the sequencer should dial (e.g., `http://127.0.0.1:54321`).
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.listen_addr)
    }

    /// TCP address the proxy listens on.
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Upstream Anvil TCP address (so tests can bypass the proxy to mine blocks).
    pub fn upstream_addr(&self) -> SocketAddr {
        self.upstream_addr
    }

    /// Simulate upstream outage. All active connections are torn down and
    /// future connection attempts are immediately closed.
    ///
    /// Idempotent: calling while already disconnected is a no-op.
    pub fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
    }

    /// Restore forwarding. Future connections forward to the upstream normally.
    ///
    /// Idempotent: calling while already connected is a no-op. Note that
    /// existing TCP sockets that were torn down during `disconnect()` remain
    /// closed; clients must establish new connections.
    pub fn reconnect(&self) {
        self.connected.store(true, Ordering::SeqCst);
    }

    /// Returns `true` if the proxy is currently forwarding.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Shutdown the proxy cleanly. Called automatically on drop.
    pub async fn shutdown(mut self) -> HarnessResult<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        // Nudge the accept loop by opening a self-connection so it observes
        // the shutdown flag on the next iteration.
        let _ = TcpStream::connect(self.listen_addr).await;
        if let Some(task) = self.accept_task.take() {
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }
}

impl Drop for TcpProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    upstream_addr: SocketAddr,
    connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let (client, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };

        // If the proxy is in "disconnected" mode, accept the TCP connection
        // and immediately drop it. This produces the same visible effect as
        // an upstream node refusing new connections.
        if !connected.load(Ordering::SeqCst) {
            drop(client);
            continue;
        }

        let connected = connected.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            forward_connection(client, upstream_addr, connected, shutdown).await;
        });
    }
}

async fn forward_connection(
    mut client: TcpStream,
    upstream_addr: SocketAddr,
    connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    let Ok(mut upstream) = TcpStream::connect(upstream_addr).await else {
        // Upstream is unreachable — drop client (mirrors a broken forward).
        return;
    };

    let (mut client_read, mut client_write) = client.split();
    let (mut upstream_read, mut upstream_write) = upstream.split();

    // Pump bytes both directions concurrently. Exit on:
    //  - either half closing cleanly
    //  - proxy disconnect() being called
    //  - proxy shutdown
    let client_to_upstream = async {
        copy_until_disconnected(&mut client_read, &mut upstream_write, &connected, &shutdown).await
    };
    let upstream_to_client = async {
        copy_until_disconnected(&mut upstream_read, &mut client_write, &connected, &shutdown).await
    };

    // Race: as soon as either direction ends, the whole connection is done.
    tokio::select! {
        _ = client_to_upstream => {}
        _ = upstream_to_client => {}
    }
}

/// Copy bytes until EOF, error, or disconnect/shutdown flag flips.
async fn copy_until_disconnected<R, W>(
    mut reader: R,
    mut writer: W,
    connected: &AtomicBool,
    shutdown: &AtomicBool,
) where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    // Small buffer is fine; JSON-RPC messages are small. We poll the flags
    // between reads so a disconnect() is observed within one read of
    // additional latency.
    let mut buf = [0_u8; 8 * 1024];
    loop {
        if shutdown.load(Ordering::SeqCst) || !connected.load(Ordering::SeqCst) {
            return;
        }
        let read_result =
            tokio::time::timeout(std::time::Duration::from_millis(50), reader.read(&mut buf)).await;
        let n = match read_result {
            Err(_) => continue,  // timeout — poll the flags again
            Ok(Ok(0)) => return, // clean EOF
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return,
        };
        if writer.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

fn parse_http_upstream(url: &str) -> HarnessResult<SocketAddr> {
    // Expect `http://host:port` (optionally with a trailing slash). The proxy
    // operates at the TCP level, so the scheme must be http(s) and the
    // host:port pair must resolve to a single address synchronously.
    let stripped = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| io_other(format!("proxy upstream URL must be http(s)://, got: {url}")))?;
    let host_port = stripped
        .trim_end_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    host_port
        .parse::<SocketAddr>()
        .map_err(|err| {
            io_other(format!(
                "proxy upstream URL '{url}' has invalid host:port: {err}"
            ))
        })
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn start_echo_server() -> (tokio::task::JoinHandle<()>, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0_u8; 1024];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                        if stream.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (handle, addr)
    }

    #[tokio::test]
    async fn forwards_bytes_when_connected() {
        let (_echo, echo_addr) = start_echo_server().await;
        let proxy = TcpProxy::spawn(&format!("http://{echo_addr}"))
            .await
            .expect("spawn proxy");

        let mut client = TcpStream::connect(proxy.listen_addr())
            .await
            .expect("connect via proxy");
        client.write_all(b"hello").await.expect("write");

        let mut buf = [0_u8; 5];
        client.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn disconnect_closes_new_connections() {
        let (_echo, echo_addr) = start_echo_server().await;
        let proxy = TcpProxy::spawn(&format!("http://{echo_addr}"))
            .await
            .expect("spawn proxy");
        proxy.disconnect();

        // New connection is accepted at TCP level but immediately closed.
        let mut client = TcpStream::connect(proxy.listen_addr())
            .await
            .expect("connect");
        let _ = client.write_all(b"hello").await; // may succeed or fail
        let mut buf = [0_u8; 8];
        // Reading should end quickly. The OS may deliver this as EOF (n=0) or
        // as ConnectionReset depending on whether our write raced ahead of
        // the proxy's drop. Both are valid "connection closed" signals — we
        // just assert the read doesn't hang.
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(500), client.read(&mut buf))
                .await
                .expect("read did not hang");
        match result {
            Ok(0) => {} // clean EOF
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) => {} // RST, also valid
            other => panic!("disconnected proxy must close the connection, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn disconnect_tears_down_active_connections() {
        let (_echo, echo_addr) = start_echo_server().await;
        let proxy = TcpProxy::spawn(&format!("http://{echo_addr}"))
            .await
            .expect("spawn proxy");

        let mut client = TcpStream::connect(proxy.listen_addr())
            .await
            .expect("connect");
        client.write_all(b"hi").await.expect("write");
        let mut buf = [0_u8; 2];
        client.read_exact(&mut buf).await.expect("initial read");
        assert_eq!(&buf, b"hi");

        // Now disconnect. The active socket should be torn down.
        proxy.disconnect();
        let mut tail = [0_u8; 8];
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            client.read(&mut tail),
        )
        .await
        .expect("read did not hang");
        match result {
            Ok(0) => {} // clean EOF
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) => {} // RST
            other => {
                panic!("disconnected proxy must tear down existing connections, got: {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn reconnect_accepts_new_connections_again() {
        let (_echo, echo_addr) = start_echo_server().await;
        let proxy = TcpProxy::spawn(&format!("http://{echo_addr}"))
            .await
            .expect("spawn proxy");

        proxy.disconnect();
        // Old socket is dead. Reconnect and try a fresh one.
        proxy.reconnect();

        let mut client = TcpStream::connect(proxy.listen_addr())
            .await
            .expect("connect after reconnect");
        client.write_all(b"back").await.expect("write");
        let mut buf = [0_u8; 4];
        client
            .read_exact(&mut buf)
            .await
            .expect("read after reconnect");
        assert_eq!(&buf, b"back");
    }

    #[tokio::test]
    async fn is_connected_reflects_state() {
        let (_echo, echo_addr) = start_echo_server().await;
        let proxy = TcpProxy::spawn(&format!("http://{echo_addr}"))
            .await
            .expect("spawn proxy");
        assert!(proxy.is_connected());
        proxy.disconnect();
        assert!(!proxy.is_connected());
        proxy.reconnect();
        assert!(proxy.is_connected());
    }

    #[test]
    fn parse_upstream_url_forms() {
        assert!(parse_http_upstream("http://127.0.0.1:8545").is_ok());
        assert!(parse_http_upstream("http://127.0.0.1:8545/").is_ok());
        assert!(parse_http_upstream("https://127.0.0.1:8545").is_ok());
        assert!(parse_http_upstream("ws://127.0.0.1:8545").is_err());
        assert!(parse_http_upstream("127.0.0.1:8545").is_err());
        assert!(parse_http_upstream("http://not-a-host").is_err());
    }
}
