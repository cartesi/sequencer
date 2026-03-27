// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Child;

use crate::HarnessResult;

pub(crate) fn io_other(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

pub(crate) fn ensure_exists(path: &Path, missing_message: String) -> HarnessResult<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(io_other(missing_message).into())
    }
}

pub(crate) fn path_as_str(path: &Path) -> HarnessResult<&str> {
    path.to_str()
        .ok_or_else(|| io_other(format!("path is not valid UTF-8: {}", path.display())).into())
}

// NOTE: There is a small TOCTOU race between releasing this port and the spawned
// process binding to it. Fixing properly would require the sequencer binary to
// support `--http-port 0` and report its actual port, which is out of scope.
pub(crate) fn build_local_endpoint() -> HarnessResult<(String, String)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let http_addr = format!("127.0.0.1:{}", addr.port());
    let endpoint = format!("http://{http_addr}");
    Ok((endpoint, http_addr))
}

pub(crate) fn timestamped_log_path(logs_dir: &Path, prefix: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    logs_dir.join(format!("{prefix}-{ts}.log"))
}

pub(crate) async fn wait_for_http_readiness(
    endpoint: &str,
    child: &mut Child,
    timeout: Duration,
) -> HarnessResult<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io_other(format!(
                "sequencer exited before readiness: status={status}"
            ))
            .into());
        }
        if http_endpoint_is_ready(endpoint).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io_other(format!(
                "timed out waiting for sequencer readiness at {endpoint}"
            ))
            .into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) async fn wait_for_rpc_readiness(
    endpoint: &str,
    child: &mut Child,
    timeout: Duration,
) -> HarnessResult<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io_other(format!("anvil exited before readiness: status={status}")).into());
        }
        if rpc_endpoint_is_ready(endpoint).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io_other(format!(
                "timed out waiting for anvil readiness at {endpoint}"
            ))
            .into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) async fn send_graceful_terminate(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };

    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }

    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
}

async fn http_endpoint_is_ready(endpoint: &str) -> bool {
    let Some(host_port) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let mut stream =
        match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(host_port)).await
        {
            Ok(Ok(value)) => value,
            _ => return false,
        };

    let request = format!("GET /tx HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0_u8; 64];
    match tokio::time::timeout(Duration::from_millis(300), stream.read(&mut head)).await {
        Ok(Ok(read)) if read > 0 => std::str::from_utf8(&head[..read])
            .ok()
            .map(|text| text.starts_with("HTTP/1.1") || text.starts_with("HTTP/1.0"))
            .unwrap_or(false),
        _ => false,
    }
}

async fn rpc_endpoint_is_ready(endpoint: &str) -> bool {
    let Some(host_port) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let mut stream =
        match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(host_port)).await
        {
            Ok(Ok(value)) => value,
            _ => return false,
        };

    let body = r#"{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}"#;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0_u8; 128];
    match tokio::time::timeout(Duration::from_millis(300), stream.read(&mut head)).await {
        Ok(Ok(read)) if read > 0 => std::str::from_utf8(&head[..read])
            .ok()
            .map(|text| text.contains("200 OK"))
            .unwrap_or(false),
        _ => false,
    }
}
