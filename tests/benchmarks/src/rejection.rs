// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use sequencer_rust_client::SubmitTxError;
use serde_json;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct RejectionOutcome {
    pub key: String,
    pub detail: String,
}

pub fn classify_rejection(
    outcome: Result<(u16, String), SubmitTxError>,
) -> Option<RejectionOutcome> {
    match outcome {
        Ok((200, _body)) => None,
        Ok((status, body)) => {
            let key = extract_rejection_key(status, &body);
            Some(RejectionOutcome {
                key,
                detail: format!("status={status}, body={body}"),
            })
        }
        Err(err) => Some(RejectionOutcome {
            key: err.breakdown_key().to_string(),
            detail: err.to_string(),
        }),
    }
}

/// Build a breakdown key from the HTTP status and response body.
///
/// Tries to extract the `code` field from a JSON body (e.g.
/// `{"code":"EXECUTION_REJECTED","message":"..."}`) so that different
/// rejection reasons under the same HTTP status get separate buckets.
fn extract_rejection_key(status: u16, body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(code) = json.get("code").and_then(|v| v.as_str()) {
            return format!("http_{status}_{}", code.to_lowercase());
        }
    }
    format!("http_{status}")
}

pub fn http_rejection_count(breakdown: &BTreeMap<String, u64>) -> u64 {
    breakdown
        .iter()
        .filter(|(key, _)| is_http_breakdown_key(key))
        .map(|(_, count)| *count)
        .sum()
}

pub fn http_429_count(breakdown: &BTreeMap<String, u64>) -> u64 {
    breakdown
        .iter()
        .filter(|(key, _)| *key == "http_429" || key.starts_with("http_429_"))
        .map(|(_, count)| *count)
        .sum()
}

pub fn client_failure_count(rejected_count: u64, breakdown: &BTreeMap<String, u64>) -> u64 {
    rejected_count.saturating_sub(http_rejection_count(breakdown))
}

pub fn has_http_rejection(breakdown: &BTreeMap<String, u64>) -> bool {
    http_rejection_count(breakdown) > 0
}

pub fn has_http_429(breakdown: &BTreeMap<String, u64>) -> bool {
    http_429_count(breakdown) > 0
}

fn is_http_breakdown_key(key: &str) -> bool {
    key.starts_with("http_")
}

#[cfg(test)]
mod tests {
    use super::{classify_rejection, client_failure_count, http_429_count, http_rejection_count};
    use sequencer_rust_client::SubmitTxError;
    use std::collections::BTreeMap;

    #[test]
    fn classify_rejection_maps_http_and_transport() {
        // Plain non-JSON body: key is just http_{status}
        let http = classify_rejection(Ok((429, "overloaded".to_string()))).expect("http rejection");
        assert_eq!(http.key, "http_429");

        // JSON body with code field: key includes the code
        let json_body = r#"{"ok":false,"code":"OVERLOADED","message":"queue full"}"#;
        let http_json =
            classify_rejection(Ok((429, json_body.to_string()))).expect("http json rejection");
        assert_eq!(http_json.key, "http_429_overloaded");

        let transport =
            classify_rejection(Err(SubmitTxError::TimeoutRead)).expect("transport rejection");
        assert_eq!(transport.key, "timeout_read");
    }

    #[test]
    fn counts_http_and_client_failures_separately() {
        let breakdown = BTreeMap::from([
            ("http_429_overloaded".to_string(), 2_u64),
            ("http_422_execution_rejected".to_string(), 3_u64),
            ("io_connect".to_string(), 4_u64),
        ]);

        assert_eq!(http_rejection_count(&breakdown), 5);
        assert_eq!(http_429_count(&breakdown), 2);
        assert_eq!(client_failure_count(9, &breakdown), 4);
    }
}
