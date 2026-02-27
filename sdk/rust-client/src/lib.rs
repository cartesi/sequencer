// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod errors;

pub use errors::{ClientBuildError, SubmitRejected, SubmitTxError, SubscribeError};

use sequencer_core::api::{TxRequest, TxResponse};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub type SubscribeStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct SequencerClient {
    endpoint: String,
    host_port: String,
    path_prefix: String,
    request_timeout: Duration,
}

impl SequencerClient {
    pub fn new(endpoint: impl Into<String>) -> Result<Self, ClientBuildError> {
        Self::new_with_timeout(endpoint, Duration::from_secs(3))
    }

    pub fn new_with_timeout(
        endpoint: impl Into<String>,
        request_timeout: Duration,
    ) -> Result<Self, ClientBuildError> {
        let endpoint = endpoint.into();
        let (host_port, path_prefix) =
            parse_http_url(endpoint.as_str()).map_err(ClientBuildError::InvalidEndpoint)?;
        Ok(Self {
            endpoint,
            host_port,
            path_prefix,
            request_timeout,
        })
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub fn host_port(&self) -> &str {
        self.host_port.as_str()
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    pub fn ws_subscribe_url(&self, from_offset: u64) -> String {
        with_from_offset(
            default_ws_subscribe_url_for_http(self.endpoint.as_str()).as_str(),
            from_offset,
        )
    }

    pub async fn submit_tx_with_status(
        &self,
        req: &TxRequest,
    ) -> Result<(u16, String), SubmitTxError> {
        submit_tx_with_status_parsed(
            self.host_port.as_str(),
            self.path_prefix.as_str(),
            req,
            self.request_timeout,
        )
        .await
    }

    pub async fn submit_tx(&self, req: &TxRequest) -> Result<TxResponse, SubmitRejected> {
        let (status, body) = self.submit_tx_with_status(req).await?;
        if status != 200 {
            return Err(SubmitRejected::Http { status, body });
        }
        serde_json::from_str::<TxResponse>(&body).map_err(|e| SubmitRejected::Decode(e.to_string()))
    }

    pub async fn subscribe(&self, from_offset: u64) -> Result<SubscribeStream, SubscribeError> {
        let url = self.ws_subscribe_url(from_offset);
        let (stream, _response) = connect_async(url.as_str())
            .await
            .map_err(|e| SubscribeError::Connect(e.to_string()))?;
        Ok(stream)
    }
}

fn default_ws_subscribe_url_for_http(http_url: &str) -> String {
    let scheme_replaced = if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{}", http_url.trim_end_matches('/'))
    };
    format!("{}/ws/subscribe", scheme_replaced.trim_end_matches('/'))
}

fn with_from_offset(ws_subscribe_url: &str, from_offset: u64) -> String {
    let separator = if ws_subscribe_url.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{ws_subscribe_url}{separator}from_offset={from_offset}")
}

async fn submit_tx_with_status_parsed(
    host_port: &str,
    path_prefix: &str,
    req: &TxRequest,
    timeout: Duration,
) -> Result<(u16, String), SubmitTxError> {
    let path = format!("{}/tx", path_prefix.trim_end_matches('/'));
    let body = serde_json::to_string(req).map_err(|e| SubmitTxError::Parse(e.to_string()))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let mut stream = tokio::time::timeout(timeout, TcpStream::connect(host_port))
        .await
        .map_err(|_| SubmitTxError::TimeoutConnect)?
        .map_err(|e| SubmitTxError::IoConnect(e.to_string()))?;

    tokio::time::timeout(timeout, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| SubmitTxError::TimeoutWrite)?
        .map_err(|e| SubmitTxError::IoWrite(e.to_string()))?;

    tokio::time::timeout(timeout, stream.flush())
        .await
        .map_err(|_| SubmitTxError::TimeoutFlush)?
        .map_err(|e| SubmitTxError::IoFlush(e.to_string()))?;

    let mut response = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = tokio::time::timeout(timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| SubmitTxError::TimeoutRead)?
            .map_err(|e| SubmitTxError::IoRead(e.to_string()))?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);

        if let Some((header_end, content_length)) = response_content_len(response.as_slice())
            && response.len() >= header_end.saturating_add(content_length)
        {
            break;
        }
    }

    parse_http_response(response.as_slice()).map_err(SubmitTxError::Parse)
}

fn parse_http_url(http_url: &str) -> Result<(String, String), String> {
    let stripped = http_url
        .trim_end_matches('/')
        .strip_prefix("http://")
        .ok_or_else(|| "only http:// URLs are supported".to_string())?;

    let (host_port, path_prefix) = if let Some((host, path)) = stripped.split_once('/') {
        (host.to_string(), format!("/{}", path))
    } else {
        (stripped.to_string(), String::new())
    };
    if host_port.is_empty() {
        return Err("missing host in http URL".to_string());
    }
    Ok((host_port, path_prefix))
}

fn parse_http_response(raw: &[u8]) -> Result<(u16, String), String> {
    let text = String::from_utf8(raw.to_vec()).map_err(|e| e.to_string())?;
    let mut sections = text.splitn(2, "\r\n\r\n");
    let headers = sections.next().unwrap_or_default();
    let body = sections.next().unwrap_or_default().to_string();

    let status_line = headers
        .lines()
        .next()
        .ok_or_else(|| "missing HTTP status line".to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "missing HTTP status code".to_string())?
        .parse::<u16>()
        .map_err(|e| e.to_string())?;
    Ok((status, body))
}

fn response_content_len(raw: &[u8]) -> Option<(usize, usize)> {
    let header_end = raw.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let headers = std::str::from_utf8(&raw[..header_end]).ok()?;
    let mut content_length = None;
    for line in headers.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
            break;
        }
    }
    content_length.map(|len| (header_end, len))
}
