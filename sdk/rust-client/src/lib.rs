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
        let path_prefix =
            parse_http_url(endpoint.as_str()).map_err(ClientBuildError::InvalidEndpoint)?;
        Ok(Self {
            endpoint,
            path_prefix,
            request_timeout,
        })
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
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
            self.endpoint.as_str(),
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
    endpoint: &str,
    path_prefix: &str,
    req: &TxRequest,
    timeout: Duration,
) -> Result<(u16, String), SubmitTxError> {
    let mut submit_url =
        reqwest::Url::parse(endpoint).map_err(|e| SubmitTxError::Parse(e.to_string()))?;
    let tx_path = if path_prefix.is_empty() {
        "/tx".to_string()
    } else {
        format!("{}/tx", path_prefix.trim_end_matches('/'))
    };
    submit_url.set_path(tx_path.as_str());
    submit_url.set_query(None);

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| SubmitTxError::Parse(e.to_string()))?;

    let response = client
        .post(submit_url)
        .json(req)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                SubmitTxError::TimeoutConnect
            } else {
                SubmitTxError::IoConnect(e.to_string())
            }
        })?;

    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|e| SubmitTxError::IoRead(e.to_string()))?;
    Ok((status, body))
}

fn parse_http_url(http_url: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(http_url).map_err(|e| e.to_string())?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err("only http:// or https:// URLs are supported".to_string());
    }
    if url.host_str().is_none() {
        return Err("missing host in URL".to_string());
    }
    let path = url.path().trim_end_matches('/').to_string();
    if path.is_empty() || path == "/" {
        Ok(String::new())
    } else {
        Ok(path)
    }
}
