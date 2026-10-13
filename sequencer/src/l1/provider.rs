// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! RPC clients for the one configured L1 endpoint: read providers for every
//! query, and keyed writers that also hold a submit provider for signed sends.
//! Every client shares the https rule, URL redaction and the rate-limit-only
//! retry.

use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use alloy::{
    primitives::TxHash,
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    rpc::{
        client::RpcClient,
        json_rpc::{RequestPacket, ResponsePacket},
    },
    signers::local::PrivateKeySigner,
    transports::http::{Http, reqwest, reqwest::Url},
};
use alloy_transport::layers::{RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy};
use alloy_transport::{
    HttpError, IntoBoxTransport, RpcError, TransportError, TransportErrorKind, TransportFut,
};
use url::Host;

/// Per-attempt HTTP deadline, connect through body, shared by every L1 client.
/// alloy never resends a timeout, and the `eth_getLogs` partition
/// (`l1/partition.rs`) splits on it like on any failure, so a query too heavy
/// to answer in time narrows until it fits. 35 s is geth's default server cap
/// (a 30 s write timeout, answered with `-32002` at 29.9 s) plus room for
/// connect, TLS and transfer: every answer a default geth gives arrives, and a
/// hung request costs at most this. An anchor, not a measurement: on a server
/// with a longer cap (Erigon's is 30 min), a filtered `eth_getLogs` slower
/// than 35 s is split instead of awaited. Revisit with measured latencies of
/// our filtered query on hosted providers, or on seeing healthy queries split.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
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

    // Otherwise reqwest's defaults: a 90 s idle pool and TCP keepalive (each
    // worker reuses its connection within seconds, and hyper drops one the
    // server closed while idle), and no connect or read timeout (the attempt
    // deadline bounds both). HTTP/1.1 because every worker keeps one request in
    // flight, and a timed-out HTTP/1.1 request closes its own connection; over
    // HTTP/2 a dead shared connection would need keep-alive PINGs to be
    // evicted. Pinned here, since any crate enabling reqwest's `http2` feature
    // would otherwise turn it on for this client too. No redirects: a JSON-RPC
    // endpoint has no reason to send one, and following it could downgrade an
    // https endpoint to plaintext past the rule above.
    let http_client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| {
            format!(
                "failed to build the HTTP client (on Linux it needs the system's CA \
                 certificates): {}",
                error_chain(&e)
            )
        })?;

    let transport = Http::with_client(http_client, url.clone());
    let is_local = transport.guess_local();
    Ok(layered_client(transport, &url, is_local))
}

/// The layers every L1 client stacks over its transport. The first layer added
/// is outermost: retry sees only redacted errors, including in the "Max
/// retries exceeded {err}" text it builds from them.
fn layered_client<T: IntoBoxTransport>(transport: T, url: &Url, is_local: bool) -> RpcClient {
    RpcClient::builder()
        .layer(retry_layer())
        .layer(RedactUrlLayer::new(url))
        .transport(transport, is_local)
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

/// What logs may show of an RPC endpoint: its scheme, host and port. Its path,
/// query and userinfo can carry an API key or a password. Assumes the host is
/// not itself the credential.
pub(crate) fn rpc_endpoint_origin(url: &str) -> String {
    Url::parse(url).map_or_else(
        |_| "<invalid url>".to_owned(),
        |url| url.origin().ascii_serialization(),
    )
}

/// An error with its source chain, for logs and string-carrying error
/// variants. reqwest's `Display` names only the request kind ("error sending
/// request"); the cause (timeout, refused connection, DNS, TLS) lives in
/// `source()`, under alloy wrappers that are transparent. A cause whose text
/// the line already holds is skipped.
pub(crate) fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut line = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        let text = next.to_string();
        if !line.contains(&text) {
            line.push_str(": ");
            line.push_str(&text);
        }
        cause = next.source();
    }
    line
}

/// Keeps the endpoint URL out of every transport error before any caller logs
/// one: a hosted provider's URL carries its API key in the path or query.
/// reqwest quotes the URL it sent (userinfo moved into an `Authorization`
/// header) in its send, timeout and body errors; alloy wraps those as
/// `Custom`, and on a non-2xx status copies a failed body read's text into an
/// HTTP error. The error kind is kept, so retry decisions and the callers'
/// classification see the same error.
///
/// Not covered: a server that echoes the request path, or a redirect target,
/// in an error body or in a JSON-RPC error's message or data (alloy builds
/// those above this layer), and alloy's DEBUG-level `alloy_transport_http`
/// span, which records the full URL. Revisit when bumping alloy-transport-http,
/// whose `reqwest_transport.rs` decides how reqwest errors are wrapped.
#[derive(Clone, Debug)]
struct RedactUrlLayer {
    /// The URL as configured and, when it has userinfo, as reqwest quotes it.
    needles: Arc<[String]>,
}

impl RedactUrlLayer {
    fn new(url: &Url) -> Self {
        let mut needles = vec![url.to_string()];
        let mut sent = url.clone();
        if sent.set_username("").is_ok() && sent.set_password(None).is_ok() && sent != *url {
            needles.push(sent.to_string());
        }
        Self {
            needles: needles.into(),
        }
    }
}

impl<S> tower::Layer<S> for RedactUrlLayer {
    type Service = RedactUrl<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RedactUrl {
            inner,
            needles: Arc::clone(&self.needles),
        }
    }
}

#[derive(Clone, Debug)]
struct RedactUrl<S> {
    inner: S,
    needles: Arc<[String]>,
}

impl<S> tower::Service<RequestPacket> for RedactUrl<S>
where
    S: tower::Service<
            RequestPacket,
            Response = ResponsePacket,
            Error = TransportError,
            Future = TransportFut<'static>,
        >,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        let needles = &self.needles;
        self.inner
            .poll_ready(cx)
            .map_err(|error| redact_url(error, needles))
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let response = self.inner.call(request);
        let needles = Arc::clone(&self.needles);
        Box::pin(async move { response.await.map_err(|error| redact_url(error, &needles)) })
    }
}

/// `reqwest` is alloy's re-export, the version the transport uses: a downcast
/// to another version never matches.
fn redact_url(error: TransportError, needles: &[String]) -> TransportError {
    let scrub = |body: String| {
        needles.iter().fold(body, |body, needle| {
            body.replace(needle.as_str(), "<rpc url>")
        })
    };
    match error {
        RpcError::Transport(TransportErrorKind::Custom(inner)) => {
            match inner.downcast::<reqwest::Error>() {
                Ok(error) => TransportErrorKind::custom(error.without_url()),
                Err(inner) => RpcError::Transport(TransportErrorKind::Custom(inner)),
            }
        }
        RpcError::Transport(TransportErrorKind::NonRetryable(inner)) => {
            match inner.downcast::<reqwest::Error>() {
                Ok(error) => TransportErrorKind::non_retryable(error.without_url()),
                Err(inner) => RpcError::Transport(TransportErrorKind::NonRetryable(inner)),
            }
        }
        RpcError::Transport(TransportErrorKind::HttpError(HttpError { status, body })) => {
            TransportErrorKind::http_error(status, scrub(body))
        }
        RpcError::Transport(TransportErrorKind::HttpErrorWithRetryAfter {
            error: HttpError { status, body },
            retry_after,
        }) => {
            TransportErrorKind::http_error_with_retry_after(status, scrub(body), Some(retry_after))
        }
        other => other,
    }
}

/// A read provider: every L1 query, with no transaction fillers.
/// `allow_insecure` opts into plaintext RPC against a non-loopback host — see
/// [`create_client`].
pub fn create_read_provider(url: &str, allow_insecure: bool) -> Result<DynProvider, String> {
    Ok(read_provider(create_client(url, allow_insecure)?))
}

fn read_provider(client: RpcClient) -> DynProvider {
    ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_client(client)
        .erased()
}

/// The providers of a keyed L1 writer (the batch poster, the mempool flusher).
/// `read` serves all of its queries: the chain-id gate, fees, nonces, gas
/// estimates and confirmation watches. `submit` only broadcasts transactions
/// the writer fully specified, signed for the pinned chain. Both ride one
/// client while the sequencer has one endpoint; a separate submit endpoint (a
/// private relay) would change only [`create_keyed_providers`].
#[derive(Clone, Debug)]
pub struct KeyedProviders {
    pub read: DynProvider,
    pub submit: Submitter,
}

/// Signs and broadcasts transactions, and nothing else. Its provider fills only
/// the pinned chain id and the signature, so nonce, gas and fees always come
/// from the caller: nothing allocates a nonce outside write-before-broadcast,
/// and the endpoint's own chain id never reaches a signature.
#[derive(Clone, Debug)]
pub struct Submitter(DynProvider);

/// Why [`Submitter::send`] failed.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// The transaction could not be signed locally — a field the caller must
    /// set is missing. A defect in the caller, never a transient condition.
    #[error("transaction not signable: {0}")]
    Unsignable(String),
    /// The endpoint did not accept the broadcast.
    #[error(transparent)]
    Rpc(TransportError),
}

impl Submitter {
    /// Sign `tx` and send it with `eth_sendRawTransaction`, returning its hash.
    /// Resending the same signed bytes is chain-idempotent: at most one copy
    /// mines, and a resend after an accepted send is answered "already known".
    pub async fn send(&self, tx: TransactionRequest) -> Result<TxHash, SendError> {
        match self.0.send_transaction(tx).await {
            Ok(pending) => Ok(*pending.tx_hash()),
            Err(RpcError::LocalUsageError(error)) => Err(SendError::Unsignable(error.to_string())),
            Err(error) => Err(SendError::Rpc(error)),
        }
    }
}

fn keyed_providers(client: RpcClient, signer: PrivateKeySigner, chain_id: u64) -> KeyedProviders {
    let submit = ProviderBuilder::new()
        .disable_recommended_fillers()
        .with_chain_id(chain_id)
        .wallet(signer)
        .connect_client(client.clone())
        .erased();
    KeyedProviders {
        read: read_provider(client),
        submit: Submitter(submit),
    }
}

/// Create a keyed writer's providers, signing for `chain_id`. `allow_insecure`
/// opts into plaintext RPC against a non-loopback host — see [`create_client`].
pub fn create_keyed_providers(
    url: &str,
    private_key: &str,
    chain_id: u64,
    allow_insecure: bool,
) -> Result<KeyedProviders, String> {
    let client = create_client(url, allow_insecure)?;
    let signer =
        PrivateKeySigner::from_str(private_key).map_err(|_| "invalid private key".to_string())?;
    Ok(keyed_providers(client, signer, chain_id))
}

/// Failure modes of [`create_verified_keyed_providers`], split so callers can
/// project them onto the right exit class: a chain mismatch is a terminal
/// operator misconfig, an RPC failure is retryable, a build failure is a bad
/// URL/key.
#[derive(Debug, thiserror::Error)]
pub enum VerifiedSignerProviderError {
    /// The providers could not be built (bad URL or private key).
    #[error("{0}")]
    Create(String),
    /// The chain-id query itself failed — the RPC is unreachable (retryable).
    #[error("chain-id query failed: {0}")]
    ChainIdRpc(String),
    /// The RPC serves a different chain than the pinned one (terminal).
    #[error("rpc chain id {rpc} does not match pinned chain id {expected}")]
    ChainIdMismatch { rpc: u64, expected: u64 },
}

/// [`create_keyed_providers`], but only after confirming the endpoint serves
/// `expected_chain_id`. Signatures carry the pinned id whatever the endpoint
/// says; this check, made right before a one-shot flush starts, keeps another
/// chain's nonces and fees out of it. The flush itself does not recheck, so a
/// chain swap behind the same URL during a flush is the residual the threat
/// model accepts for the reader.
pub async fn create_verified_keyed_providers(
    url: &str,
    private_key: &str,
    expected_chain_id: u64,
    allow_insecure: bool,
) -> Result<KeyedProviders, VerifiedSignerProviderError> {
    let providers = create_keyed_providers(url, private_key, expected_chain_id, allow_insecure)
        .map_err(VerifiedSignerProviderError::Create)?;
    let rpc_chain_id = providers
        .read
        .get_chain_id()
        .await
        .map_err(|e| VerifiedSignerProviderError::ChainIdRpc(error_chain(&e)))?;
    if rpc_chain_id != expected_chain_id {
        return Err(VerifiedSignerProviderError::ChainIdMismatch {
            rpc: rpc_chain_id,
            expected: expected_chain_id,
        });
    }
    Ok(providers)
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

    // ── Endpoint redaction: no error or log quotes the URL ───

    const URL_SECRETS: [&str; 3] = ["hunter2", "not-a-real-api-key", "query-secret"];

    #[tokio::test]
    async fn request_errors_do_not_quote_the_rpc_url_but_keep_the_cause() {
        let client = create_client(
            "http://user:hunter2@127.0.0.1:1/v2/not-a-real-api-key?key=query-secret",
            false,
        )
        .expect("loopback http is accepted");
        let error = client
            .request_noparams::<alloy_primitives::U64>("eth_blockNumber")
            .await
            .expect_err("nothing listens on port 1");
        let chain = error_chain(&error);
        for rendered in [chain.as_str(), &format!("{error:?}")] {
            for secret in URL_SECRETS {
                assert!(!rendered.contains(secret), "{rendered}");
            }
        }
        assert!(chain.starts_with("error sending request"), "{chain}");
        assert!(chain.contains("Connect"), "the cause survives: {chain}");
        assert_eq!(chain.matches("error sending request").count(), 1, "{chain}");
        assert!(
            matches!(error, RpcError::Transport(TransportErrorKind::Custom(_))),
            "the error kind is kept: {error:?}"
        );

        let error = create_verified_keyed_providers(
            "http://user:hunter2@127.0.0.1:1/v2/not-a-real-api-key?key=query-secret",
            ANVIL_KEY_0,
            1,
            false,
        )
        .await
        .expect_err("nothing listens on port 1");
        assert!(
            matches!(&error, VerifiedSignerProviderError::ChainIdRpc(_)),
            "{error:?}"
        );
        let text = error.to_string();
        for secret in URL_SECRETS {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("Connect"), "the cause survives: {text}");
    }

    #[test]
    fn http_error_bodies_lose_the_rpc_url() {
        let url =
            Url::parse("https://user:hunter2@rpc.example/v2/not-a-real-api-key?key=query-secret")
                .unwrap();
        let layer = RedactUrlLayer::new(&url);
        // reqwest quotes the URL it sent, without the userinfo.
        let body = "<failed to read response body: error decoding response body for url \
                    (https://rpc.example/v2/not-a-real-api-key?key=query-secret)>"
            .to_owned();
        let error = redact_url(
            TransportErrorKind::http_error(502, body.clone()),
            &layer.needles,
        );
        assert_eq!(
            error.to_string(),
            "HTTP error 502 with body: <failed to read response body: error decoding \
             response body for url (<rpc url>)>"
        );
        // alloy retries a plain HTTP error only on 429 and 503; redaction keeps that.
        let unavailable = redact_url(
            TransportErrorKind::http_error(503, body.clone()),
            &layer.needles,
        );
        assert!(
            matches!(&unavailable, RpcError::Transport(kind) if kind.is_retry_err()),
            "a 503 stays retryable: {unavailable:?}"
        );

        let delay = Duration::from_secs(3);
        match redact_url(
            TransportErrorKind::http_error_with_retry_after(
                429,
                format!("{body} {url}"),
                Some(delay),
            ),
            &layer.needles,
        ) {
            RpcError::Transport(TransportErrorKind::HttpErrorWithRetryAfter {
                error,
                retry_after,
            }) => {
                assert_eq!(error.status, 429);
                assert_eq!(retry_after, delay);
                for secret in URL_SECRETS {
                    assert!(!error.body.contains(secret), "{}", error.body);
                }
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Swapping the two layers would let retry quote an unredacted body in
    /// its "Max retries exceeded" text, which no later layer scrubs.
    #[tokio::test]
    async fn retried_errors_are_redacted_before_retry_wraps_them() {
        #[derive(Clone)]
        struct Failing(String);
        impl tower::Service<RequestPacket> for Failing {
            type Response = ResponsePacket;
            type Error = TransportError;
            type Future = TransportFut<'static>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, _: RequestPacket) -> Self::Future {
                let body = self.0.clone();
                Box::pin(async move {
                    Err(TransportErrorKind::http_error_with_retry_after(
                        429,
                        body,
                        Some(Duration::ZERO),
                    ))
                })
            }
        }

        let url =
            Url::parse("https://user:hunter2@rpc.example/v2/not-a-real-api-key?key=query-secret")
                .unwrap();
        let body = format!("upstream refused {url}");
        let error = layered_client(Failing(body), &url, false)
            .request_noparams::<alloy_primitives::U64>("eth_blockNumber")
            .await
            .expect_err("every attempt fails");
        let chain = error_chain(&error);
        assert!(chain.contains("Max retries exceeded"), "{chain}");
        for secret in URL_SECRETS {
            assert!(!chain.contains(secret), "{chain}");
        }
    }

    #[test]
    fn logs_show_only_the_endpoint_origin() {
        assert_eq!(
            rpc_endpoint_origin(
                "https://user:hunter2@rpc.example:8443/v2/not-a-real-api-key?key=query-secret"
            ),
            "https://rpc.example:8443"
        );
        assert_eq!(
            rpc_endpoint_origin("https://rpc.example/v2/k"),
            "https://rpc.example"
        );
        assert_eq!(
            rpc_endpoint_origin("http://127.0.0.1:8545"),
            "http://127.0.0.1:8545"
        );
        assert_eq!(
            rpc_endpoint_origin("http://[::1]:8545/"),
            "http://[::1]:8545"
        );
        assert_eq!(
            rpc_endpoint_origin("https://rpc.example:8443/v2/k#fragment"),
            "https://rpc.example:8443"
        );
        assert_eq!(rpc_endpoint_origin("not-a-valid-url"), "<invalid url>");
    }

    /// The endpoint answers 308 to a second server that would answer the call:
    /// a followed redirect succeeds, a refused one fails with the 308.
    #[tokio::test]
    async fn no_client_follows_a_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn answer_once(listener: tokio::net::TcpListener, response: String) {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read request");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }

        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target.local_addr().unwrap();
        let body = r#"{"jsonrpc":"2.0","id":0,"result":"0x1"}"#;
        tokio::spawn(answer_once(
            target,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            ),
        ));
        let endpoint = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint_address = endpoint.local_addr().unwrap();
        tokio::spawn(answer_once(
            endpoint,
            format!(
                "HTTP/1.1 308 Permanent Redirect\r\nLocation: http://{target_address}/\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        ));

        let error = create_client(&format!("http://{endpoint_address}/"), false)
            .unwrap()
            .request_noparams::<alloy_primitives::U64>("eth_chainId")
            .await
            .expect_err("the redirect must not be followed");
        assert!(
            matches!(
                &error,
                RpcError::Transport(TransportErrorKind::HttpError(HttpError { status: 308, .. }))
            ),
            "{error:?}"
        );
    }

    #[test]
    fn error_chain_appends_each_new_cause_once() {
        #[derive(Debug, thiserror::Error)]
        #[error("leaf failure")]
        struct Leaf;
        #[derive(Debug, thiserror::Error)]
        #[error("{0}")]
        struct Echo(#[source] Leaf);
        #[derive(Debug, thiserror::Error)]
        #[error("request failed")]
        struct Outer(#[source] Echo);

        assert_eq!(
            error_chain(&Outer(Echo(Leaf))),
            "request failed: leaf failure"
        );
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
    fn keyed_providers_do_not_echo_key_bytes_on_invalid_hex() {
        // A malformed key that would otherwise cause alloy's error Display to
        // embed a character from the input. The fix replaced {e} with a fixed
        // string. Assert the error is the fixed string exactly — not a prefix
        // match — so a future change that re-adds interpolation is caught.
        let bad_key =
            "0xZZZZ_zzzz_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff";
        let err = create_keyed_providers("http://127.0.0.1:8545", bad_key, 31337, false)
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
    fn keyed_providers_do_not_echo_key_bytes_on_odd_length() {
        // Odd-length hex would trigger a different error variant. Same
        // invariant: fixed error message, no key bytes leaked.
        let bad_key = "0xabc";
        let err = create_keyed_providers("http://127.0.0.1:8545", bad_key, 31337, false)
            .expect_err("odd-length hex key must be rejected");
        assert_eq!(err, "invalid private key");
    }

    #[test]
    fn keyed_providers_accept_a_valid_key() {
        let good_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        create_keyed_providers("http://127.0.0.1:8545", good_key, 31337, false)
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
    async fn verified_keyed_providers_gate_on_chain_id() {
        use alloy::node_bindings::Anvil;

        require_anvil();
        let anvil = Anvil::default().spawn();
        let chain_id = anvil.chain_id();

        // Matching chain id → a usable signing provider.
        create_verified_keyed_providers(&anvil.endpoint(), ANVIL_KEY_0, chain_id, false)
            .await
            .expect("matching chain id yields verified keyed providers");

        // Wrong pinned chain id → terminal ChainIdMismatch, before any signing,
        // surfacing the served id vs the pinned one.
        let err =
            create_verified_keyed_providers(&anvil.endpoint(), ANVIL_KEY_0, chain_id + 1, false)
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

    // ── Read and submit providers ─────────────────────────────

    /// A transport answering from `asserter` that records each request's
    /// method and params.
    fn recording_client(
        asserter: &Asserter,
    ) -> (RpcClient, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
        #[derive(Clone)]
        struct Recording {
            inner: MockTransport,
            requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        }
        impl tower::Service<RequestPacket> for Recording {
            type Response = ResponsePacket;
            type Error = TransportError;
            type Future = TransportFut<'static>;
            fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
                self.inner.poll_ready(cx)
            }
            fn call(&mut self, request: RequestPacket) -> Self::Future {
                self.requests
                    .lock()
                    .expect("request log")
                    .push(serde_json::to_value(&request).expect("request serializes"));
                self.inner.call(request)
            }
        }
        let requests = Arc::default();
        let transport = Recording {
            inner: MockTransport::new(asserter.clone()),
            requests: Arc::clone(&requests),
        };
        (RpcClient::new(transport, true), requests)
    }

    fn complete_transfer(from: alloy_primitives::Address) -> TransactionRequest {
        use alloy::network::TransactionBuilder;
        TransactionRequest::default()
            .with_from(from)
            .with_to(from)
            .with_value(alloy_primitives::U256::ZERO)
            .with_gas_limit(21_000)
            .with_nonce(7)
            .with_max_fee_per_gas(2_000_000_000)
            .with_max_priority_fee_per_gas(1_000_000_000)
    }

    #[tokio::test]
    async fn the_submitter_sends_one_raw_transaction_signed_for_the_pinned_chain() {
        use alloy::consensus::TxEnvelope;
        use alloy::eips::eip2718::Decodable2718;

        let signer = PrivateKeySigner::from_str(ANVIL_KEY_0).unwrap();
        let from = signer.address();
        let asserter = Asserter::new();
        asserter.push_success(&alloy_primitives::B256::repeat_byte(0x01));
        let (client, requests) = recording_client(&asserter);
        let providers = keyed_providers(client, signer, 4242);

        providers
            .submit
            .send(complete_transfer(from))
            .await
            .expect("a complete transaction is sent");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0]["method"], "eth_sendRawTransaction");
        let raw: alloy_primitives::Bytes =
            serde_json::from_value(requests[0]["params"][0].clone()).unwrap();
        let envelope = TxEnvelope::decode_2718(&mut raw.as_ref()).expect("a signed envelope");
        let TxEnvelope::Eip1559(signed) = envelope else {
            panic!("an EIP-1559 transaction, got {envelope:?}");
        };
        assert_eq!(
            signed.tx().chain_id,
            4242,
            "the pinned id, never the endpoint's"
        );
        assert_eq!(signed.tx().nonce, 7);
        let signer_address = signed
            .signature()
            .recover_address_from_prehash(&signed.signature_hash())
            .expect("a recoverable signature");
        assert_eq!(signer_address, from);
    }

    #[tokio::test]
    async fn the_submitter_refuses_an_incomplete_transaction_without_rpc() {
        let signer = PrivateKeySigner::from_str(ANVIL_KEY_0).unwrap();
        let from = signer.address();
        let (client, requests) = recording_client(&Asserter::new());
        let providers = keyed_providers(client, signer, 4242);

        let mut incomplete = complete_transfer(from);
        incomplete.nonce = None;
        let error = providers
            .submit
            .send(incomplete)
            .await
            .expect_err("no filler may pick a nonce");
        assert!(matches!(error, SendError::Unsignable(_)), "{error:?}");
        assert!(requests.lock().unwrap().is_empty());
    }
}
