// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod errors;
mod history;

pub use errors::{
    ClientBuildError, HistoryReadError, QueryError, SnapshotError, SubmitRejected, SubmitTxError,
    SubscribeError,
};

pub use sequencer_core::history::{
    EraId, ExecutedInputCount, HistoryBounds, HistoryClaim, HistoryPolicyError, HistoryVersion,
    RecoveryGeneration,
};
pub use sequencer_core::history_api::{
    AcceptedCheckpoint, HistoricalL1Input, HistoricalL1InputStart, HistoricalL1InputsPage,
    HistoryBaseline, HistoryCompatibility, HistoryDeployment, HistoryInfo,
};

use alloy_primitives::Address;
use sequencer_core::api::{DomainResponse, FeeResponse, NonceResponse, TxRequest, TxResponse};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub type SubscribeStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Metadata and streaming body from the same leased checkpoint response.
/// Restore the archive before using `claim` to resume its application history.
pub struct SnapshotResponse {
    pub claim: HistoryClaim,
    pub response: reqwest::Response,
}

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
        // Validate URL format (must be http://)
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
            .strip_prefix("http://")
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

    pub fn ws_subscribe_url(&self, claim: HistoryClaim) -> String {
        with_history_claim(
            default_ws_subscribe_url_for_http(self.endpoint.as_str()).as_str(),
            claim,
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
            .timeout(self.request_timeout)
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

    pub async fn get_fee(&self) -> Result<FeeResponse, QueryError> {
        self.get_json("/fee", "").await
    }

    /// The nonce `sender` signs next. A hint for one op in flight: re-query
    /// after a `422` bad-nonce rejection, since recovery can lower it.
    pub async fn get_nonce(&self, sender: Address) -> Result<NonceResponse, QueryError> {
        self.get_json("/nonce", &format!("?sender={sender:#x}"))
            .await
    }

    /// The EIP-712 domain the sequencer verifies against. Compare it with a
    /// pinned domain; never sign with it unchecked.
    pub async fn get_domain(&self) -> Result<DomainResponse, QueryError> {
        self.get_json("/domain", "").await
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        route: &'static str,
        query: &str,
    ) -> Result<T, QueryError> {
        let url = format!("{}{route}{query}", self.endpoint.trim_end_matches('/'));
        let transport = |source| QueryError::Transport { route, source };
        let response = self
            .http_client
            .get(&url)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|e| transport(map_reqwest_error(e)))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| transport(SubmitTxError::IoRead(e.to_string())))?;
        if status != 200 {
            return Err(QueryError::Http {
                route,
                status,
                body,
            });
        }
        serde_json::from_str::<T>(&body).map_err(|e| QueryError::Decode {
            route,
            reason: e.to_string(),
        })
    }

    /// Bounds response headers by the request timeout; callers own body cancellation.
    pub async fn latest_snapshot(&self) -> Result<SnapshotResponse, SnapshotError> {
        let request = self
            .http_client
            .get(format!(
                "{}/latest_snapshot",
                self.endpoint.trim_end_matches('/')
            ))
            .send();
        let response = tokio::time::timeout(self.request_timeout, request)
            .await
            .map_err(|_| SnapshotError::HeadersTimeout)??
            .error_for_status()?;
        let header = |name| {
            response
                .headers()
                .get(name)
                .ok_or_else(|| SnapshotError::Metadata(format!("missing {name}")))?
                .to_str()
                .map_err(|error| SnapshotError::Metadata(error.to_string()))
        };
        let era_id = header("X-History-Era")?.parse().map_err(
            |error: sequencer_core::history::EraIdParseError| {
                SnapshotError::Metadata(error.to_string())
            },
        )?;
        let generation: u64 = header("X-Recovery-Generation")?
            .parse()
            .map_err(|error: std::num::ParseIntError| SnapshotError::Metadata(error.to_string()))?;
        let count: u64 = header("X-Executed-Input-Count")?
            .parse()
            .map_err(|error: std::num::ParseIntError| SnapshotError::Metadata(error.to_string()))?;
        Ok(SnapshotResponse {
            claim: HistoryClaim {
                version: HistoryVersion {
                    era_id,
                    recovery_generation: sequencer_core::history::RecoveryGeneration::new(
                        generation,
                    ),
                },
                next_input: ExecutedInputCount::new(count),
            },
            response,
        })
    }

    pub async fn subscribe(&self, claim: HistoryClaim) -> Result<SubscribeStream, SubscribeError> {
        let url = self.ws_subscribe_url(claim);
        let (stream, _response) = connect_async(url.as_str())
            .await
            .map_err(map_subscribe_error)?;
        Ok(stream)
    }
}

fn build_http_client(request_timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(request_timeout)
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
        SubmitTxError::IoConnect(err.to_string())
    } else {
        SubmitTxError::IoRead(err.to_string())
    }
}

fn validate_http_url(http_url: &str) -> Result<(), String> {
    let stripped = http_url
        .trim_end_matches('/')
        .strip_prefix("http://")
        .ok_or_else(|| "only http:// URLs are supported".to_string())?;
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

fn with_history_claim(ws_subscribe_url: &str, claim: HistoryClaim) -> String {
    let separator = if ws_subscribe_url.contains('?') {
        '&'
    } else {
        '?'
    };
    format!(
        "{ws_subscribe_url}{separator}era_id={}&recovery_generation={}&next_input={}",
        claim.version.era_id,
        claim.version.recovery_generation.get(),
        claim.next_input.get()
    )
}

fn map_subscribe_error(error: tokio_tungstenite::tungstenite::Error) -> SubscribeError {
    if let tokio_tungstenite::tungstenite::Error::Http(response) = &error
        && response.status().as_u16() == 409
        && let Some(bytes) = response
            .headers()
            .get("X-History-Error")
            .map(|value| value.as_bytes())
            .or_else(|| response.body().as_deref())
        && let Ok(policy) = serde_json::from_slice::<HistoryPolicyError>(bytes)
    {
        return SubscribeError::History(policy);
    }
    SubscribeError::Connect(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::history::{EraId, RecoveryGeneration};

    #[tokio::test]
    async fn fee_request_keeps_its_request_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = SequencerClient::new_with_timeout(
            format!("http://{address}"),
            Duration::from_millis(100),
        )
        .unwrap();
        let request = client.get_fee();
        let stalled_server = async {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        };
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = request => result,
                () = stalled_server => unreachable!(),
            }
        })
        .await
        .expect("fee request must retain its deadline independently of snapshot streaming");
        assert!(matches!(
            result,
            Err(QueryError::Transport {
                source: SubmitTxError::TimeoutRead,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn snapshot_headers_keep_the_configured_request_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = SequencerClient::new(format!("http://{address}"))
            .unwrap()
            .with_request_timeout(Duration::from_millis(100));
        let stalled_server = async {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        };
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = client.latest_snapshot() => result,
                () = stalled_server => unreachable!(),
            }
        })
        .await
        .expect("an accepted connection with no headers must reach the configured deadline");
        assert!(matches!(result, Err(SnapshotError::HeadersTimeout)));
    }

    #[tokio::test]
    async fn snapshot_body_outlives_the_transaction_request_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let mut received = 0;
            while !request[..received].ends_with(b"\r\n\r\n") {
                let count = stream.read(&mut request[received..]).await.unwrap();
                assert_ne!(count, 0, "complete request headers");
                received += count;
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nX-History-Era: 00112233-4455-4677-8899-aabbccddeeff\r\nX-Recovery-Generation: 0\r\nX-Executed-Input-Count: 7\r\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            stream.write_all(b"dump").await.unwrap();
        });
        let client = SequencerClient::new_with_timeout(
            format!("http://{address}"),
            Duration::from_millis(100),
        )
        .unwrap();
        let snapshot = client.latest_snapshot().await.unwrap();
        assert_eq!(snapshot.claim.next_input.get(), 7);
        assert_eq!(snapshot.response.bytes().await.unwrap().as_ref(), b"dump");
        server.await.unwrap();
    }

    #[test]
    fn subscription_url_carries_the_exact_resume_claim() {
        let claim = HistoryClaim {
            version: HistoryVersion {
                era_id: "00112233-4455-4677-8899-aabbccddeeff"
                    .parse::<EraId>()
                    .unwrap(),
                recovery_generation: RecoveryGeneration::new(9),
            },
            next_input: ExecutedInputCount::new(50_001),
        };
        let client = SequencerClient::new("http://localhost:8080").unwrap();
        assert_eq!(
            client.ws_subscribe_url(claim),
            "ws://localhost:8080/ws/subscribe?era_id=00112233-4455-4677-8899-aabbccddeeff&recovery_generation=9&next_input=50001"
        );
    }

    #[test]
    fn typed_refusal_survives_an_http_body_in_a_later_packet() {
        let policy = HistoryPolicyError::AheadOfHead {
            head: ExecutedInputCount::new(12),
        };
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(409)
            .header("X-History-Error", serde_json::to_string(&policy).unwrap())
            .body(Some(Vec::new()))
            .unwrap();
        assert!(
            matches!(map_subscribe_error(tokio_tungstenite::tungstenite::Error::Http(Box::new(response))),
            SubscribeError::History(actual) if actual == policy)
        );
    }

    #[tokio::test]
    async fn subscription_below_nonzero_baseline_decodes_history_unavailable() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).await.unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() <= 8192);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /ws/subscribe?"));
            assert!(request.contains("next_input=40 "));
            stream.write_all(b"HTTP/1.1 409 Conflict\r\nX-History-Error: {\"code\":\"HISTORY_UNAVAILABLE\",\"available_from\":41}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let client = SequencerClient::new(format!("http://{address}")).unwrap();
        let result = client
            .subscribe(HistoryClaim {
                version: HistoryVersion {
                    era_id: "00112233-4455-4677-8899-aabbccddeeff".parse().unwrap(),
                    recovery_generation: RecoveryGeneration::new(0),
                },
                next_input: ExecutedInputCount::new(40),
            })
            .await;
        assert!(matches!(result,
            Err(SubscribeError::History(HistoryPolicyError::HistoryUnavailable { available_from }))
                if available_from == ExecutedInputCount::new(41)));
        server.await.unwrap();
    }
}
