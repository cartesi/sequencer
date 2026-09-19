// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::{
    EraId, HistoricalL1InputStart, HistoricalL1InputsPage, HistoryInfo, HistoryPolicyError,
    HistoryReadError, RecoveryGeneration, SequencerClient,
};

impl SequencerClient {
    /// Discover history, optionally requiring the era selected for bootstrap.
    /// A compatibility query supplies both the checkpoint's era and its generation.
    pub async fn history(
        &self,
        expected_era: Option<EraId>,
        from_generation: Option<RecoveryGeneration>,
    ) -> Result<HistoryInfo, HistoryReadError> {
        let mut query: Vec<_> = expected_era
            .map(|era| ("era_id", era.to_string()))
            .into_iter()
            .collect();
        if let Some(generation) = from_generation {
            query.push(("from_generation", generation.get().to_string()));
        }
        let request = self
            .http_client
            .get(format!("{}/history", self.endpoint.trim_end_matches('/')))
            .query(&query)
            .timeout(self.request_timeout);
        let body = read_history_response(request).await?;
        Ok(serde_json::from_str(&body)?)
    }

    /// Read one complete raw-input page under the configured request deadline.
    /// Continue with its `next_input_index`; a short page is not necessarily EOF.
    /// Use `with_request_timeout` when historical transfers need a longer deadline.
    pub async fn historical_l1_inputs(
        &self,
        era: EraId,
        start: HistoricalL1InputStart,
        limit: Option<usize>,
    ) -> Result<HistoricalL1InputsPage, HistoryReadError> {
        let mut query = vec![("era_id", era.to_string())];
        query.push(match start {
            HistoricalL1InputStart::NextInputIndex(index) => {
                ("next_input_index", index.to_string())
            }
            HistoricalL1InputStart::AfterBlock(block) => ("after_block", block.to_string()),
        });
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let request = self
            .http_client
            .get(format!(
                "{}/historical-l1-inputs",
                self.endpoint.trim_end_matches('/')
            ))
            .query(&query)
            .timeout(self.request_timeout);
        let body = read_history_response(request).await?;
        Ok(serde_json::from_str(&body)?)
    }
}

async fn read_history_response(
    request: reqwest::RequestBuilder,
) -> Result<String, HistoryReadError> {
    let response = request.send().await?;
    let status = response.status().as_u16();
    let body = response.text().await?;
    if status == 409
        && let Ok(policy) = serde_json::from_str::<HistoryPolicyError>(&body)
    {
        return Err(HistoryReadError::History(policy));
    }
    if status != 200 {
        return Err(HistoryReadError::Http { status, body });
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;

    const ERA: &str = "00112233-4455-4677-8899-aabbccddeeff";

    async fn request_headers(stream: &mut TcpStream) -> String {
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 1);
            headers.push(byte[0]);
            assert!(headers.len() <= 8192, "request headers exceed test bound");
        }
        String::from_utf8(headers).unwrap()
    }

    async fn serve_once(status: u16, body: String) -> (SequencerClient, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let headers = request_headers(&mut stream).await;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            headers
        });
        (SequencerClient::new(endpoint).unwrap(), server)
    }

    fn request_url(headers: &str) -> reqwest::Url {
        let target = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        reqwest::Url::parse(&format!("http://localhost{target}")).unwrap()
    }

    fn history_body() -> String {
        serde_json::json!({
            "deployment": {
                "chain_id": 31337,
                "app_address": "0x1111111111111111111111111111111111111111",
                "input_box_address": "0x2222222222222222222222222222222222222222",
                "app_deployment_block": 1,
                "batch_submitter_address": "0x3333333333333333333333333333333333333333"
            },
            "history": {
                "version": { "era_id": ERA, "recovery_generation": 2 },
                "available_from": 7,
                "head": 12
            },
            "baseline": {
                "l1_stop_block": 1240,
                "l1_end_input_index": 8,
                "next_batch_nonce": 2
            },
            "accepted_checkpoint": {
                "inclusion_block": 1250,
                "executed_input_count": 10,
                "next_batch_nonce": 3
            },
            "compatibility": null
        })
        .to_string()
    }

    fn page_body() -> String {
        serde_json::json!({
            "era_id": ERA,
            "l1_stop_block": 1240,
            "end_input_index": 8,
            "next_input_index": 6,
            "items": [{
                "input_index": 5,
                "sender": "0x3333333333333333333333333333333333333333",
                "payload": "0x00ff80",
                "block_number": 1230,
                "block_timestamp": 1700014760_u64,
                "transaction_hash": "0x4444444444444444444444444444444444444444444444444444444444444444"
            }]
        })
        .to_string()
    }

    #[tokio::test]
    async fn discovery_decodes_history_and_encodes_optional_era() {
        for expected_era in [None, Some(ERA.parse().unwrap())] {
            let (client, server) = serve_once(200, history_body()).await;
            let info = client.history(expected_era, None).await.unwrap();
            assert_eq!(info.history.available_from.get(), 7);
            assert_eq!(info.history.head.get(), 12);
            assert_eq!(info.history.version.era_id.to_string(), ERA);
            assert_eq!(info.baseline.l1_stop_block, 1240);
            assert!(info.compatibility.is_none());
            let url = request_url(&server.await.unwrap());
            assert_eq!(url.path(), "/history");
            let query: Vec<_> = url.query_pairs().collect();
            if expected_era.is_some() {
                assert_eq!(query, [("era_id".into(), ERA.into())]);
            } else {
                assert!(query.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn compatibility_encodes_checkpoint_generation_and_decodes_preserved_boundary() {
        let mut body: serde_json::Value = serde_json::from_str(&history_body()).unwrap();
        body["compatibility"] = serde_json::json!({
            "from_generation": 1,
            "preserved_input_count": 9
        });
        let (client, server) = serve_once(200, body.to_string()).await;
        let info = client
            .history(Some(ERA.parse().unwrap()), Some(RecoveryGeneration::new(1)))
            .await
            .unwrap();
        let compatibility = info.compatibility.unwrap();
        assert_eq!(compatibility.from_generation.get(), 1);
        assert_eq!(compatibility.preserved_input_count.get(), 9);
        assert_eq!(info.history.version.recovery_generation.get(), 2);
        let url = request_url(&server.await.unwrap());
        assert_eq!(url.path(), "/history");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            [
                ("era_id".into(), ERA.into()),
                ("from_generation".into(), "1".into())
            ]
        );
    }

    #[tokio::test]
    async fn pages_encode_one_selector_and_decode_original_binary_fields() {
        for (start, limit, selector, value) in [
            (
                HistoricalL1InputStart::NextInputIndex(5),
                Some(1),
                "next_input_index",
                "5",
            ),
            (
                HistoricalL1InputStart::AfterBlock(10),
                None,
                "after_block",
                "10",
            ),
        ] {
            let (client, server) = serve_once(200, page_body()).await;
            let page = client
                .historical_l1_inputs(ERA.parse().unwrap(), start, limit)
                .await
                .unwrap();
            assert_eq!(page.next_input_index, 6);
            assert_eq!(page.items[0].input_index, 5);
            assert_eq!(page.items[0].payload.as_ref(), &[0x00, 0xff, 0x80]);
            assert_eq!(page.items[0].sender.as_slice(), &[0x33; 20]);
            assert_eq!(page.items[0].transaction_hash.as_slice(), &[0x44; 32]);
            let url = request_url(&server.await.unwrap());
            assert_eq!(url.path(), "/historical-l1-inputs");
            let query: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
            assert_eq!(query.get("era_id").unwrap(), ERA);
            assert_eq!(query.get(selector).unwrap(), value);
            assert_eq!(query.len(), if limit.is_some() { 3 } else { 2 });
            if let Some(limit) = limit {
                assert_eq!(query.get("limit").unwrap(), &limit.to_string());
            }
        }
    }

    #[tokio::test]
    async fn era_change_is_typed_for_both_reads() {
        let policy = HistoryPolicyError::EraChanged {
            current: crate::HistoryVersion {
                era_id: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
                recovery_generation: sequencer_core::history::RecoveryGeneration::new(0),
            },
        };
        for page_request in [false, true] {
            let (client, server) = serve_once(409, serde_json::to_string(&policy).unwrap()).await;
            let result = if page_request {
                client
                    .historical_l1_inputs(
                        ERA.parse().unwrap(),
                        HistoricalL1InputStart::NextInputIndex(0),
                        None,
                    )
                    .await
                    .map(|_| ())
            } else {
                client
                    .history(Some(ERA.parse().unwrap()), None)
                    .await
                    .map(|_| ())
            };
            assert!(matches!(result, Err(HistoryReadError::History(actual)) if actual == policy));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn other_refusals_preserve_status_and_body() {
        for status in [400, 409, 429, 503] {
            let body = format!("refusal {status}");
            let (client, server) = serve_once(status, body.clone()).await;
            assert!(matches!(
                client.history(None, None).await,
                Err(HistoryReadError::Http { status: actual_status, body: actual_body })
                    if actual_status == status && actual_body == body
            ));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn malformed_success_is_a_decode_error() {
        let (client, server) = serve_once(200, "{}".into()).await;
        assert!(matches!(
            client.history(None, None).await,
            Err(HistoryReadError::Decode(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn history_deadline_covers_the_response_body() {
        for page_request in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                request_headers(&mut stream).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            });
            let client = SequencerClient::new(endpoint)
                .unwrap()
                .with_request_timeout(Duration::from_millis(100));
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                if page_request {
                    client
                        .historical_l1_inputs(
                            ERA.parse().unwrap(),
                            HistoricalL1InputStart::NextInputIndex(0),
                            None,
                        )
                        .await
                        .map(|_| ())
                } else {
                    client.history(None, None).await.map(|_| ())
                }
            })
            .await
            .expect("the configured deadline must include body transfer");
            server.abort();
            assert!(matches!(result, Err(HistoryReadError::Request(error)) if error.is_timeout()));
        }
    }
}
