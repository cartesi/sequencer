// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::str::FromStr;
use std::time::Duration;

use alloy::{
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::client::RpcClient,
    signers::local::PrivateKeySigner,
    transports::http::{Http, reqwest, reqwest::Url},
};
use alloy_transport::layers::RetryBackoffLayer;

// Public Ethereum providers (Infura, Alchemy) commonly take 30–60s on heavy
// `eth_getLogs` queries under load. The partition-retry helper in
// `l1/partition.rs` only kicks on RPC error codes (e.g. -32005), not on
// transport timeouts — a request that silently chews past the timeout slips
// past partitioning. 60s is long enough that hitting it signals a genuine
// problem rather than a slow query.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RATE_LIMIT_RETRIES: u32 = 5;
const INITIAL_BACKOFF_MS: u64 = 200;
const COMPUTE_UNITS_PER_SEC: u64 = 500;

fn create_client(url: &str) -> Result<RpcClient, String> {
    let url = Url::parse(url).map_err(|e| format!("invalid RPC URL: {e}"))?;

    // Reject non-HTTPS for remote hosts to prevent accidental plaintext RPC.
    // `url::Url::host_str` returns bracket-wrapped IPv6 literals (e.g. "[::1]").
    if url.scheme() != "https" && !is_loopback_host(url.host_str().unwrap_or("")) {
        return Err(format!(
            "remote RPC must use https, got {}://",
            url.scheme()
        ));
    }

    let http_client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let transport = Http::with_client(http_client, url);
    let is_local = transport.guess_local();

    let retry = RetryBackoffLayer::new(
        MAX_RATE_LIMIT_RETRIES,
        INITIAL_BACKOFF_MS,
        COMPUTE_UNITS_PER_SEC,
    );

    Ok(RpcClient::builder()
        .layer(retry)
        .transport(transport, is_local))
}

/// Check whether a URL host string refers to a loopback address.
///
/// `url::Url::host_str` wraps IPv6 literals in brackets (e.g. `[::1]`), which
/// this helper normalizes alongside the IPv4 and DNS forms.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Create a read-only provider with retry and timeout.
pub fn create_provider(url: &str) -> Result<DynProvider, String> {
    let client = create_client(url)?;
    let provider = ProviderBuilder::new().connect_client(client);
    Ok(provider.erased())
}

/// Create a provider with a wallet signer, retry, and timeout.
pub fn create_signer_provider(url: &str, private_key: &str) -> Result<DynProvider, String> {
    let client = create_client(url)?;
    let signer =
        PrivateKeySigner::from_str(private_key).map_err(|_| "invalid private key".to_string())?;
    let provider = ProviderBuilder::new().wallet(signer).connect_client(client);
    Ok(provider.erased())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── H4 regression: URL scheme enforcement ─────────────

    #[test]
    fn create_client_rejects_http_for_remote_host() {
        let err = create_client("http://mainnet.infura.io/v3/abc123")
            .expect_err("http:// for remote host must be rejected");
        assert!(
            err.contains("https"),
            "error should explain https requirement, got: {err}"
        );
    }

    #[test]
    fn create_client_accepts_http_for_127_0_0_1() {
        create_client("http://127.0.0.1:8545").expect("loopback http:// must be accepted");
    }

    #[test]
    fn create_client_accepts_http_for_localhost() {
        create_client("http://localhost:8545").expect("localhost http:// must be accepted");
    }

    #[test]
    fn create_client_accepts_http_for_ipv6_loopback() {
        create_client("http://[::1]:8545").expect("IPv6 loopback http:// must be accepted");
    }

    #[test]
    fn create_client_accepts_https_for_remote_host() {
        create_client("https://mainnet.infura.io/v3/abc123").expect("https:// must be accepted");
    }

    // ── H3 regression: private-key parse error must not echo bytes ─

    #[test]
    fn create_signer_provider_does_not_echo_key_bytes_on_invalid_hex() {
        // A malformed key that would otherwise cause alloy's error Display to
        // embed a character from the input. The fix replaced {e} with a fixed
        // string. Assert the error is the fixed string exactly — not a prefix
        // match — so a future change that re-adds interpolation is caught.
        let bad_key =
            "0xZZZZ_zzzz_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff";
        let err = create_signer_provider("http://127.0.0.1:8545", bad_key)
            .expect_err("malformed hex key must be rejected");
        assert_eq!(
            err, "invalid private key",
            "error message must be the fixed constant — no key bytes, no hex excerpt"
        );
        // Belt-and-suspenders: no characters from the bad key should appear.
        assert!(
            !err.contains('Z') && !err.contains('z') && !err.contains('f'),
            "error must not reflect any byte of the input key: {err}"
        );
    }

    #[test]
    fn create_signer_provider_does_not_echo_key_bytes_on_odd_length() {
        // Odd-length hex would trigger a different error variant. Same
        // invariant: fixed error message, no key bytes leaked.
        let bad_key = "0xabc";
        let err = create_signer_provider("http://127.0.0.1:8545", bad_key)
            .expect_err("odd-length hex key must be rejected");
        assert_eq!(err, "invalid private key");
    }

    #[test]
    fn create_signer_provider_accepts_valid_key() {
        let good_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        create_signer_provider("http://127.0.0.1:8545", good_key)
            .expect("valid key must be accepted");
    }
}
