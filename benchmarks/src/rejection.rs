// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use sequencer_rust_client::SubmitTxError;
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
        Ok((status, body)) => Some(RejectionOutcome {
            key: format!("http_{status}"),
            detail: format!("status={status}, body={body}"),
        }),
        Err(err) => Some(RejectionOutcome {
            key: err.breakdown_key().to_string(),
            detail: err.to_string(),
        }),
    }
}

pub fn http_rejection_count(breakdown: &BTreeMap<String, u64>) -> u64 {
    breakdown
        .iter()
        .filter(|(key, _)| is_http_breakdown_key(key))
        .map(|(_, count)| *count)
        .sum()
}

pub fn http_429_count(breakdown: &BTreeMap<String, u64>) -> u64 {
    breakdown.get("http_429").copied().unwrap_or(0)
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
        let http = classify_rejection(Ok((429, "overloaded".to_string()))).expect("http rejection");
        assert_eq!(http.key, "http_429");

        let transport =
            classify_rejection(Err(SubmitTxError::TimeoutRead)).expect("transport rejection");
        assert_eq!(transport.key, "timeout_read");
    }

    #[test]
    fn counts_http_and_client_failures_separately() {
        let breakdown = BTreeMap::from([
            ("http_429".to_string(), 2_u64),
            ("http_422".to_string(), 3_u64),
            ("io_connect".to_string(), 4_u64),
        ]);

        assert_eq!(http_rejection_count(&breakdown), 5);
        assert_eq!(http_429_count(&breakdown), 2);
        assert_eq!(client_failure_count(9, &breakdown), 4);
    }
}
