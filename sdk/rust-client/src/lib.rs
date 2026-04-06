// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod errors;

pub use errors::{ClientBuildError, SubmitRejected, SubmitTxError, SubscribeError};

use sequencer_core::api::{TxRequest, TxResponse};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub type SubscribeStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct SequencerClient {
    endpoint: String,
    http_client: reqwest::Client,
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
        // Validate URL format (must be http:// or https://)
        validate_http_url(&endpoint).map_err(ClientBuildError::InvalidEndpoint)?;
        let http_client = build_http_client(request_timeout)
            .map_err(|e| ClientBuildError::InvalidEndpoint(e.to_string()))?;
        Ok(Self {
            endpoint,
            http_client,
            request_timeout,
        })
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub fn host_port(&self) -> &str {
        self.endpoint
            .strip_prefix("https://")
            .or_else(|| self.endpoint.strip_prefix("http://"))
            .unwrap_or(&self.endpoint)
            .split('/')
            .next()
            .unwrap_or(&self.endpoint)
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self.http_client =
            build_http_client(request_timeout).expect("failed to rebuild reqwest client");
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
        let url = format!("{}/tx", self.endpoint.trim_end_matches('/'));

        let response = self
            .http_client
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| SubmitTxError::IoRead(e.to_string()))?;

        Ok((status, body))
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

fn build_http_client(request_timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(request_timeout)
        .timeout(request_timeout)
        .pool_max_idle_per_host(64)
        .build()
}

fn map_reqwest_error(err: reqwest::Error) -> SubmitTxError {
    if err.is_timeout() {
        if err.is_connect() {
            SubmitTxError::TimeoutConnect
        } else {
            SubmitTxError::TimeoutRead
        }
    } else if err.is_connect() {
        SubmitTxError::IoConnect(error_chain(&err))
    } else {
        SubmitTxError::IoRead(error_chain(&err))
    }
}

/// Walk the full error chain via `.source()` to produce a detailed message.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        msg.push_str(": ");
        msg.push_str(&cause.to_string());
        source = cause.source();
    }
    msg
}

fn validate_http_url(http_url: &str) -> Result<(), String> {
    let trimmed = http_url.trim_end_matches('/');
    let stripped = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| "only http:// and https:// URLs are supported".to_string())?;
    let host = stripped.split('/').next().unwrap_or("");
    if host.is_empty() {
        return Err("missing host in http URL".to_string());
    }
    Ok(())
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
