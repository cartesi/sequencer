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
use url::Host;

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

fn create_client(url: &str, allow_insecure: bool) -> Result<RpcClient, String> {
    let url = Url::parse(url).map_err(|e| format!("invalid RPC URL: {e}"))?;

    // Reject non-HTTPS for remote hosts to prevent accidental plaintext RPC to a
    // public endpoint (eavesdropping / MITM of signed L1 traffic). Loopback is
    // always allowed; any other host requires `allow_insecure` — the explicit
    // operator opt-in for trusted private networks (a Docker/K8s service name,
    // `host.docker.internal`, a private-VPC IP) where anvil-style plaintext is
    // normal and safe. The opt-out is loud, never silent.
    let remote_plaintext = url.scheme() != "https" && !is_loopback_host(url.host());
    if remote_plaintext && !allow_insecure {
        return Err(format!(
            "remote RPC must use https, got {}:// (set \
             CARTESI_SEQUENCER_ALLOW_INSECURE_RPC=true / --allow-insecure-rpc if \
             this host is on a trusted private network, e.g. a Docker/K8s service)",
            url.scheme()
        ));
    }
    if remote_plaintext {
        tracing::warn!(
            url = %url,
            "insecure plaintext RPC to a non-loopback host — explicitly allowed via allow-insecure-rpc"
        );
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

/// Whether a parsed URL host refers to a loopback address.
///
/// Uses the already-parsed [`url::Host`] rather than the raw host string, so IP
/// literals are classified by [`std::net::Ipv4Addr::is_loopback`] /
/// [`std::net::Ipv6Addr::is_loopback`] (covering the whole `127.0.0.0/8` block
/// and canonical `::1`, with no bracket-stripping). `localhost` is the reserved
/// loopback name (RFC 6761) — a convention no library computes offline, so it
/// stays the one explicit string.
fn is_loopback_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Create a read-only provider with retry and timeout. `allow_insecure` opts
/// into plaintext RPC against a non-loopback host — see [`create_client`].
pub fn create_provider(url: &str, allow_insecure: bool) -> Result<DynProvider, String> {
    let client = create_client(url, allow_insecure)?;
    let provider = ProviderBuilder::new().connect_client(client);
    Ok(provider.erased())
}

/// Create a provider with a wallet signer, retry, and timeout. `allow_insecure`
/// opts into plaintext RPC against a non-loopback host — see [`create_client`].
pub fn create_signer_provider(
    url: &str,
    private_key: &str,
    allow_insecure: bool,
) -> Result<DynProvider, String> {
    let client = create_client(url, allow_insecure)?;
    let signer =
        PrivateKeySigner::from_str(private_key).map_err(|_| "invalid private key".to_string())?;
    let provider = ProviderBuilder::new().wallet(signer).connect_client(client);
    Ok(provider.erased())
}

/// Failure modes of [`create_verified_signer_provider`], split so callers can
/// project them onto the right exit class: a chain mismatch is a terminal
/// operator misconfig, an RPC failure is retryable, a build failure is a bad
/// URL/key.
#[derive(Debug, thiserror::Error)]
pub enum VerifiedSignerProviderError {
    /// The signing provider could not be built (bad URL or private key).
    #[error("{0}")]
    Create(String),
    /// The chain-id query itself failed — the RPC is unreachable (retryable).
    #[error("chain-id query failed: {0}")]
    ChainIdRpc(String),
    /// The RPC serves a different chain than the pinned one (terminal).
    #[error("rpc chain id {rpc} does not match pinned chain id {expected}")]
    ChainIdMismatch { rpc: u64, expected: u64 },
}

/// Create a signing provider, but only after confirming the RPC serves
/// `expected_chain_id`. The guarded constructor for **keyed L1 writers**: it
/// folds the wrong-chain check and the signer build into one call so a keyed
/// path cannot broadcast onto the wrong chain by forgetting to validate first.
/// The chain id is queried on the very provider that will sign, immediately
/// before it is returned and before any transaction is sent — so it also closes
/// the window where a long-lived process's earlier (boot-time / one-shot) check
/// has gone stale because a load-balanced RPC failed over to another chain.
pub async fn create_verified_signer_provider(
    url: &str,
    private_key: &str,
    expected_chain_id: u64,
    allow_insecure: bool,
) -> Result<DynProvider, VerifiedSignerProviderError> {
    let provider = create_signer_provider(url, private_key, allow_insecure)
        .map_err(VerifiedSignerProviderError::Create)?;
    let rpc_chain_id = provider
        .get_chain_id()
        .await
        .map_err(|e| VerifiedSignerProviderError::ChainIdRpc(e.to_string()))?;
    if rpc_chain_id != expected_chain_id {
        return Err(VerifiedSignerProviderError::ChainIdMismatch {
            rpc: rpc_chain_id,
            expected: expected_chain_id,
        });
    }
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── H4 regression: URL scheme enforcement ─────────────

    #[test]
    fn create_client_rejects_http_for_remote_host() {
        let err = create_client("http://mainnet.infura.io/v3/abc123", false)
            .expect_err("http:// for remote host must be rejected");
        assert!(
            err.contains("https"),
            "error should explain https requirement, got: {err}"
        );
    }

    #[test]
    fn create_client_accepts_http_for_127_0_0_1() {
        create_client("http://127.0.0.1:8545", false).expect("loopback http:// must be accepted");
    }

    #[test]
    fn create_client_accepts_http_for_localhost() {
        create_client("http://localhost:8545", false).expect("localhost http:// must be accepted");
    }

    #[test]
    fn create_client_accepts_http_for_ipv6_loopback() {
        create_client("http://[::1]:8545", false).expect("IPv6 loopback http:// must be accepted");
    }

    #[test]
    fn create_client_accepts_http_for_127_0_0_0_8_block() {
        // The whole 127.0.0.0/8 range is loopback — `is_loopback()` covers it,
        // whereas the old literal `== "127.0.0.1"` match rejected these.
        create_client("http://127.0.0.2:8545", false).expect("127.0.0.2 is loopback");
        create_client("http://127.1.2.3:8545", false).expect("127.1.2.3 is loopback");
    }

    #[test]
    fn create_client_accepts_https_for_remote_host() {
        create_client("https://mainnet.infura.io/v3/abc123", false)
            .expect("https:// must be accepted");
    }

    // ── Option A: explicit opt-in for plaintext to a trusted private host ──

    #[test]
    fn create_client_rejects_http_for_docker_service_name_by_default() {
        // A Docker Compose / K8s service name is not loopback; default-secure
        // rejects it, and the error points the operator at the opt-in.
        let err = create_client("http://anvil:8545", false)
            .expect_err("http:// to a bare service name must be rejected by default");
        assert!(
            err.contains("https") && err.contains("ALLOW_INSECURE_RPC"),
            "error should explain https requirement and the opt-in, got: {err}"
        );
    }

    #[test]
    fn create_client_allows_insecure_rpc_when_opted_in() {
        // The explicit operator opt-in permits plaintext to a non-loopback host
        // (Docker service name, private IP, host.docker.internal).
        create_client("http://anvil:8545", true)
            .expect("service name http:// allowed when opted in");
        create_client("http://10.0.1.5:8545", true)
            .expect("private IP http:// allowed when opted in");
        create_client("http://host.docker.internal:8545", true)
            .expect("host.docker.internal http:// allowed when opted in");
    }

    #[test]
    fn create_client_still_allows_loopback_without_the_opt_in() {
        // The opt-in is not required for loopback — that stays a zero-config
        // safe default even with allow_insecure = false.
        create_client("http://127.0.0.1:8545", false).expect("loopback needs no opt-in");
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
        let err = create_signer_provider("http://127.0.0.1:8545", bad_key, false)
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
        let err = create_signer_provider("http://127.0.0.1:8545", bad_key, false)
            .expect_err("odd-length hex key must be rejected");
        assert_eq!(err, "invalid private key");
    }

    #[test]
    fn create_signer_provider_accepts_valid_key() {
        let good_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        create_signer_provider("http://127.0.0.1:8545", good_key, false)
            .expect("valid key must be accepted");
    }

    // ── Keyed-write chain-id gate (review): the guarded constructor must
    //    confirm the served chain matches the pinned one before signing ─

    fn require_anvil() {
        assert!(
            std::process::Command::new("anvil")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok(),
            "anvil not found on PATH — install Foundry (https://getfoundry.sh)"
        );
    }

    /// Anvil account 0 — a valid signing key for the guarded constructor.
    const ANVIL_KEY_0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[tokio::test]
    async fn verified_signer_provider_gates_on_chain_id() {
        use alloy::node_bindings::Anvil;

        require_anvil();
        let anvil = Anvil::default().spawn();
        let chain_id = anvil.chain_id();

        // Matching chain id → a usable signing provider.
        create_verified_signer_provider(&anvil.endpoint(), ANVIL_KEY_0, chain_id, false)
            .await
            .expect("matching chain id yields a verified signer provider");

        // Wrong pinned chain id → terminal ChainIdMismatch, before any signing,
        // surfacing the served id vs the pinned one.
        let err =
            create_verified_signer_provider(&anvil.endpoint(), ANVIL_KEY_0, chain_id + 1, false)
                .await
                .expect_err("a mismatched pinned chain id must be rejected before signing");
        assert!(
            matches!(
                err,
                VerifiedSignerProviderError::ChainIdMismatch { rpc, expected }
                    if rpc == chain_id && expected == chain_id + 1
            ),
            "expected ChainIdMismatch {{ rpc: {chain_id}, expected: {} }}, got {err:?}",
            chain_id + 1
        );
    }
}
