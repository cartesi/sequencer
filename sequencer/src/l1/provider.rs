// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::Duration;

use alloy::{
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::{
        client::RpcClient,
        json_rpc::{RequestPacket, ResponsePacket},
    },
    signers::local::PrivateKeySigner,
    transports::http::{Http, reqwest, reqwest::Url},
};
use alloy_transport::{
    RpcError, TransportError, TransportErrorKind, TransportFut,
    layers::{RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy},
};
use tower::{Layer, Service};
use url::Host;

// Per-attempt HTTP timeout, shared by every L1 client. alloy never resends a
// timeout, and the `eth_getLogs` partition (`l1/partition.rs`) splits on it like
// on any failure, so a query too heavy to answer in time narrows until it fits.
// 60 s favors finishing heavy but healthy queries over noticing a hung endpoint:
// a persistently hanging `eth_getLogs` costs about `(log2(range) + 1) × 60 s`
// per sync, the startup sync included. Revisit with measured latencies of our
// filtered query on hosted providers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Resends of one request that [`RateLimitOnly`] lets through, a second apart
/// (alloy's backoff is not exponential) unless the server asks for a wait of up
/// to [`MAX_RETRY_WAIT`], so a resend lands in a fresh window of a per-second
/// cap.
const MAX_RESENDS: u32 = 2;
const RESEND_BACKOFF_MS: u64 = 1_000;
/// The longest server-requested wait a resend honors.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5);
/// `-32005` ("limit exceeded") rejections that only a narrower `eth_getLogs`
/// range clears; alloy resends every `-32005` as an Infura rate limit.
const NARROWING_ONLY_REJECTIONS: &[&str] = &[
    "query returned more than",        // Infura result-count cap
    "Too many logs requested",         // Nethermind JsonRpc.MaxLogsPerResponse
    "exceeds maximum RPC range limit", // Besu --rpc-max-logs-range
];

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
            eth_rpc_origin = %url.origin().ascii_serialization(),
            "insecure plaintext RPC to a non-loopback host — explicitly allowed via allow-insecure-rpc"
        );
    }

    let http_client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let redact = RedactUrlLayer(url.clone());
    let transport = Http::with_client(http_client, url);
    let is_local = transport.guess_local();

    // Innermost, so the retry layer and every caller see redacted errors.
    Ok(RpcClient::builder()
        .layer(retry_layer())
        .layer(redact)
        .transport(transport, is_local))
}

/// The retry layer every L1 client shares. `u64::MAX` compute units per second
/// turns off alloy's compute-unit queueing, which models a provider plan we do
/// not know: each worker builds its own client and keeps about one request in
/// flight.
fn retry_layer() -> RetryBackoffLayer<RateLimitOnly> {
    RetryBackoffLayer::new_with_policy(
        MAX_RESENDS,
        RESEND_BACKOFF_MS,
        u64::MAX,
        RateLimitOnly::default(),
    )
}

/// alloy's rate-limit policy minus the resends that cannot help: a
/// server-requested wait over [`MAX_RETRY_WAIT`] (alloy honors up to 300 s per
/// resend and logs it only at TRACE), and a [`NARROWING_ONLY_REJECTIONS`]
/// `-32005`, which would cost each failing `eth_getLogs` split two more full
/// queries. A refused error reaches the caller at once instead of sleeping:
/// worker loops retry on their next tick, the `eth_getLogs` partition narrows
/// the range at once (`l1/partition.rs`), and a one-shot command fails.
///
/// Liveness and visibility only: which inputs are accepted is decided by the
/// reader's witnesses, whatever was resent. Assumes the listed wordings; a
/// reworded rejection is resent like any other `-32005`, wasteful but never
/// stuck. Revisit
/// when a crawl is seen failing on refused waits, or when alloy changes its
/// `-32005` rule or its hint clamp.
#[derive(Clone, Copy, Debug, Default)]
struct RateLimitOnly(RateLimitRetryPolicy);

impl RetryPolicy for RateLimitOnly {
    fn should_retry(&self, error: &TransportError) -> bool {
        let narrowing_only = error.as_error_resp().is_some_and(|payload| {
            payload.code == -32005
                && NARROWING_ONLY_REJECTIONS
                    .iter()
                    .any(|wording| payload.message.contains(wording))
        });
        let long_wait = self
            .0
            .backoff_hint(error)
            .is_some_and(|wait| wait > MAX_RETRY_WAIT);
        !narrowing_only && !long_wait && self.0.should_retry(error)
    }

    fn backoff_hint(&self, error: &TransportError) -> Option<Duration> {
        self.0.backoff_hint(error)
    }
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

/// The RPC URL as `scheme://host[:port]`, the only part safe to log: providers
/// put credentials in the userinfo, path or query.
pub fn rpc_origin(url: &str) -> String {
    match Url::parse(url) {
        Ok(url) => url.origin().ascii_serialization(),
        Err(_) => "<invalid URL>".to_owned(),
    }
}

/// Strips the RPC URL from every transport error (see [`rpc_origin`]), so any
/// caller may stringify a provider error. alloy's own DEBUG span
/// (`ReqwestTransport`) still records the full URL; it is out of reach here.
struct RedactUrlLayer(Url);

impl<S> Layer<S> for RedactUrlLayer {
    type Service = RedactUrl<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RedactUrl {
            inner,
            url: self.0.clone(),
        }
    }
}

#[derive(Clone)]
struct RedactUrl<S> {
    inner: S,
    url: Url,
}

impl<S> Service<RequestPacket> for RedactUrl<S>
where
    S: Service<
            RequestPacket,
            Response = ResponsePacket,
            Error = TransportError,
            Future = TransportFut<'static>,
        >,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let response = self.inner.call(request);
        let url = self.url.clone();
        Box::pin(async move {
            response
                .await
                .map_err(|err| redact_transport_error(err, &url))
        })
    }
}

/// reqwest's error text ends in `for url (<url>)`. alloy keeps it as a
/// `reqwest::Error`, or formats it into the body of an error-status response
/// whose body read failed.
fn redact_transport_error(err: TransportError, url: &Url) -> TransportError {
    let origin = url.origin().ascii_serialization();
    let redact_body = |body: &str| redact_url_spans(&body.replace(url.as_str(), &origin), &origin);
    match err {
        RpcError::Transport(TransportErrorKind::Custom(source)) => {
            match source.downcast::<reqwest::Error>() {
                Ok(source) => TransportErrorKind::custom((*source).without_url()),
                Err(source) => RpcError::Transport(TransportErrorKind::Custom(source)),
            }
        }
        RpcError::Transport(TransportErrorKind::HttpError(mut error)) => {
            error.body = redact_body(&error.body);
            RpcError::Transport(TransportErrorKind::HttpError(error))
        }
        RpcError::Transport(TransportErrorKind::HttpErrorWithRetryAfter {
            mut error,
            retry_after,
        }) => {
            error.body = redact_body(&error.body);
            RpcError::Transport(TransportErrorKind::HttpErrorWithRetryAfter { error, retry_after })
        }
        err => err,
    }
}

/// The URL in reqwest's `for url (...)` is not the configured string: reqwest
/// moves userinfo into a header, and a redirect names its target. Replace the
/// whole span; an unclosed one is dropped.
fn redact_url_spans(text: &str, origin: &str) -> String {
    const MARKER: &str = "for url (";
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(MARKER) {
        let url_start = start + MARKER.len();
        redacted.push_str(&rest[..url_start]);
        redacted.push_str(origin);
        rest = rest[url_start..]
            .find(')')
            .map_or("", |end| &rest[url_start + end..]);
    }
    redacted.push_str(rest);
    redacted
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
    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::transports::mock::{Asserter, MockTransport};
    use alloy_transport::{RpcError, TransportErrorKind};

    fn error_response(code: i64, message: &'static str) -> TransportError {
        RpcError::ErrorResp(ErrorPayload {
            code,
            message: message.into(),
            data: None,
        })
    }

    // ── Retry policy: resend rate limits only, briefly ────────

    #[test]
    fn the_retry_policy_resends_rate_limits_only() {
        let policy = RateLimitOnly::default();
        for retried in [
            error_response(429, "Too Many Requests"),
            error_response(-32005, "Rate limit exceeded"),
            error_response(-32005, "project ID request rate exceeded"),
            TransportErrorKind::http_error(503, String::new()),
            TransportErrorKind::http_error_with_retry_after(
                429,
                "Too Many Requests".into(),
                Some(Duration::from_secs(2)),
            ),
        ] {
            assert!(policy.should_retry(&retried), "{retried}");
        }
        let backoff = serde_json::value::to_raw_value(
            &serde_json::json!({ "rate": { "backoff_seconds": 30 } }),
        )
        .unwrap();
        for sent_once in [
            // Log caps that only a narrower range clears.
            error_response(
                -32005,
                "query returned more than 10000 results. Try with this block range [0x15ef3c0, 0x15ef4ff].",
            ),
            error_response(
                -32005,
                "Too many logs requested. Max logs per response is 20000.",
            ),
            error_response(-32005, "Requested range exceeds maximum RPC range limit"),
            // A longer wait is the caller's next tick.
            TransportErrorKind::http_error_with_retry_after(
                429,
                "Too Many Requests".into(),
                Some(Duration::from_secs(30)),
            ),
            RpcError::ErrorResp(ErrorPayload {
                code: -32005,
                message: "project ID request rate exceeded".into(),
                data: Some(backoff),
            }),
            // A hang or a dead endpoint is never resent.
            TransportErrorKind::custom_str("operation timed out"),
        ] {
            assert!(!policy.should_retry(&sent_once), "{sent_once}");
        }
    }

    fn mocked_client(asserter: &Asserter) -> RpcClient {
        RpcClient::builder()
            .layer(retry_layer())
            .transport(MockTransport::new(asserter.clone()), true)
    }

    #[tokio::test]
    async fn a_rate_limit_is_sent_three_times() {
        let asserter = Asserter::new();
        for _ in 0..3 {
            asserter.push_failure(ErrorPayload {
                code: 429,
                message: "Too Many Requests".into(),
                data: None,
            });
        }
        let error = mocked_client(&asserter)
            .request_noparams::<alloy_primitives::U64>("eth_blockNumber")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Max retries exceeded"),
            "{error}"
        );
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn a_narrowing_only_rejection_reaches_the_caller_unretried() {
        let asserter = Asserter::new();
        asserter.push_failure(ErrorPayload {
            code: -32005,
            message: "query returned more than 10000 results".into(),
            data: None,
        });
        // A resend would find the queue empty and fail differently.
        let error = mocked_client(&asserter)
            .request_noparams::<alloy_primitives::U64>("eth_getLogs")
            .await
            .unwrap_err();
        assert_eq!(
            error.as_error_resp().map(|payload| payload.code),
            Some(-32005)
        );
        assert!(asserter.read_q().is_empty());
    }

    // ── Regression: URL scheme enforcement ────────────────

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

    // ── Regression: private-key parse error must not echo bytes ────

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

    // ── RPC URL redaction: credentials never reach error text ──

    /// Nothing listens on port 1; loopback http needs no opt-in.
    const SECRET_URL: &str = "http://user:PASSWORD@127.0.0.1:1/v3/PATHSECRET?key=QUERYSECRET";

    fn assert_no_secret(text: &str) {
        for secret in ["PASSWORD", "PATHSECRET", "QUERYSECRET"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn rpc_origin_keeps_only_scheme_host_and_port() {
        assert_eq!(
            rpc_origin("https://user:PASSWORD@rpc.example:8443/v3/PATHSECRET?key=QUERYSECRET#f"),
            "https://rpc.example:8443"
        );
        assert_eq!(
            rpc_origin("https://rpc.example/v3/PATHSECRET"),
            "https://rpc.example"
        );
        assert_eq!(rpc_origin("http://[::1]:8545/"), "http://[::1]:8545");
        assert_eq!(rpc_origin("PATHSECRET"), "<invalid URL>");
    }

    #[tokio::test]
    async fn transport_errors_carry_no_rpc_url_secrets() {
        let provider = create_provider(SECRET_URL, false).expect("loopback http is accepted");
        let err = provider
            .get_chain_id()
            .await
            .expect_err("nothing listens on port 1");
        assert!(
            matches!(err, RpcError::Transport(TransportErrorKind::Custom(_))),
            "{err:?}"
        );
        assert_no_secret(&format!("{err} {err:?}"));

        let err = create_verified_signer_provider(SECRET_URL, ANVIL_KEY_0, 1, false)
            .await
            .expect_err("nothing listens on port 1");
        assert!(
            matches!(&err, VerifiedSignerProviderError::ChainIdRpc(_)),
            "{err:?}"
        );
        assert_no_secret(&err.to_string());
    }

    #[test]
    fn redaction_strips_the_url_from_an_error_status_body() {
        let url = Url::parse(SECRET_URL).unwrap();
        // reqwest reports the URL it sent, with the userinfo moved to a header.
        let mut sent = url.clone();
        sent.set_username("").unwrap();
        sent.set_password(None).unwrap();
        let body = format!(
            "<failed to read response body: error decoding response body for url ({sent})>"
        );
        let err = redact_transport_error(TransportErrorKind::http_error(503, body), &url);
        let text = err.to_string();
        assert_no_secret(&text);
        assert!(text.contains("for url (http://127.0.0.1:1)>"), "{text}");
        assert!(
            matches!(&err, RpcError::Transport(kind) if kind.is_retry_err()),
            "a 503 stays retryable: {err:?}"
        );
    }

    #[test]
    fn redaction_replaces_every_url_span_and_drops_an_unclosed_one() {
        let origin = "https://rpc.example";
        assert_eq!(
            redact_url_spans(
                "a for url (https://other.example/PATHSECRET) b for url (x?key=QUERYSECRET) c",
                origin
            ),
            "a for url (https://rpc.example) b for url (https://rpc.example) c"
        );
        assert_eq!(
            redact_url_spans("cut for url (https://rpc.example/PATHSECRET", origin),
            "cut for url (https://rpc.example"
        );
    }
}
