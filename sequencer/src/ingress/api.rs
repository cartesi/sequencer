// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Public ingress HTTP:
//!
//! - `POST /tx` — validate a signed user op, enqueue it for the inclusion
//!   lane, and wait for the lane's commit ack. Synchronous from the client's
//!   perspective: 200 means included.
//! - `GET /fee` — quote the open-frame fee, recommended fee, and a suggested
//!   `max_fee` so a wallet can sign before submitting.
//! - `GET /nonce?sender=` — the nonce `sender` must sign next, derived from the
//!   user ops the lane has committed.
//! - `GET /domain` — the EIP-712 domain signatures are verified against.
//!
//! `/fee` and `/nonce` query SQLite on a read-only connection, never the lane:
//! WAL readers do not block its writes.
//!
//! Admission checks only what the lane cannot cheaply check itself: payload
//! size and the signature. Nonce and `max_fee` are left to the lane. A stale
//! op costs it an in-memory comparison and no transaction, whereas an ingress
//! pre-check would add a SQLite read to every honest submit, and a future
//! nonce or a fresh address bypasses it. Revisit if persistent `429`s trace
//! back to stale-nonce floods.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use alloy_sol_types::Eip712Domain;
use axum::Router;
use axum::extract::{Json, Query, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::oneshot;
use tower_http::cors::{Any, CorsLayer};
use tracing::debug;

use crate::http::{ApiError, storage_task};
use crate::ingress::inclusion_lane::PendingUserOp;
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::Storage;
use sequencer_core::api::{
    DomainResponse, FeeResponse, NonceResponse, TxRequest, TxResponse, parse_sender_address,
};
use sequencer_core::user_op::SignedUserOp;
use serde::Deserialize;

/// State for the submit endpoint. Kept narrow — only what `/tx` actually needs.
#[derive(Clone)]
pub(crate) struct SubmitState {
    pub tx_sender: mpsc::Sender<PendingUserOp>,
    pub domain: Eip712Domain,
    pub max_user_op_data_bytes: usize,
    pub shutdown: RuntimeScope,
}

impl SubmitState {
    pub(crate) fn new(
        tx_sender: mpsc::Sender<PendingUserOp>,
        domain: Eip712Domain,
        max_user_op_data_bytes: usize,
        shutdown: RuntimeScope,
    ) -> Self {
        Self {
            tx_sender,
            domain,
            max_user_op_data_bytes,
            shutdown,
        }
    }

    fn reject_if_shutting_down(&self) -> Result<(), ApiError> {
        if self.shutdown.is_shutdown_requested() {
            Err(ApiError::unavailable("sequencer shutting down"))
        } else {
            Ok(())
        }
    }
}

/// State for the public read routes. `/fee` reads the open frame and the fee
/// policy; `/nonce` reads lane-written user ops filtered by batch validity;
/// `/domain` is fixed for the process.
#[derive(Clone)]
pub(crate) struct ReadState {
    db_path: String,
    domain: DomainResponse,
    shutdown: RuntimeScope,
}

impl ReadState {
    pub(crate) fn new(db_path: String, domain: DomainResponse, shutdown: RuntimeScope) -> Self {
        Self {
            db_path,
            domain,
            shutdown,
        }
    }

    fn reject_if_shutting_down(&self) -> Result<(), ApiError> {
        if self.shutdown.is_shutdown_requested() {
            Err(ApiError::unavailable("sequencer shutting down"))
        } else {
            Ok(())
        }
    }
}

/// Build the ingress router. Caller wires it into an `axum::serve` listener.
pub(crate) fn router(submit: Arc<SubmitState>, read: Arc<ReadState>) -> Router {
    Router::new()
        .route("/tx", post(submit_tx))
        .with_state(submit)
        .merge(
            Router::new()
                .route("/fee", get(get_fee))
                .route("/nonce", get(get_nonce))
                .route("/domain", get(get_domain))
                .with_state(read),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers(Any)
                .max_age(Duration::from_secs(3600)),
        )
}

async fn submit_tx(
    State(state): State<Arc<SubmitState>>,
    req: Result<Json<TxRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(req) = req.map_err(map_json_rejection)?;

    let signed = req
        .into_signed_user_op(&state.domain, state.max_user_op_data_bytes)
        .map_err(ApiError::from)?;
    let nonce = signed.user_op.nonce;
    let sender = signed.sender;
    let ack = enqueue_verified_tx(state.as_ref(), signed)?;

    let commit_result = ack
        .await
        .map_err(|_| ApiError::internal_error("inclusion lane dropped response"))?;
    commit_result.map_err(ApiError::from)?;
    debug!(sender = %sender, nonce, "tx committed");

    Ok(Json(TxResponse {
        ok: true,
        sender: sender.to_string(),
        nonce,
    })
    .into_response())
}

async fn get_fee(State(state): State<Arc<ReadState>>) -> Result<Response, ApiError> {
    state.reject_if_shutting_down()?;
    let db_path = state.db_path.clone();
    let result = storage_task(state.shutdown.clone(), "read fee quote", move |_scope| {
        let mut storage = Storage::open_read_only(&db_path)?;
        Ok(storage.current_fee_quote()?)
    })
    .await;
    match result {
        Ok(Some((fee, recommended_fee))) => {
            Ok(no_store(Json(FeeResponse::quote(fee, recommended_fee))))
        }
        Ok(None) => Err(ApiError::unavailable("no open frame")),
        Err(err) => {
            tracing::warn!(error = %err, "GET /fee failed");
            Err(ApiError::internal_error("fee unavailable"))
        }
    }
}

#[derive(Deserialize)]
struct NonceQuery {
    sender: String,
}

const INVALID_NONCE_QUERY: &str = "expected ?sender=<0x-prefixed 20-byte hex address>";

async fn get_nonce(
    State(state): State<Arc<ReadState>>,
    query: Result<Query<NonceQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    state.reject_if_shutting_down()?;
    let sender = query
        .ok()
        .and_then(|Query(query)| parse_sender_address(&query.sender).ok())
        .ok_or_else(|| ApiError::bad_request(INVALID_NONCE_QUERY))?;
    let db_path = state.db_path.clone();
    let result = storage_task(
        state.shutdown.clone(),
        "read next user nonce",
        move |_scope| {
            let mut storage = Storage::open_read_only(&db_path)?;
            Ok(storage.next_user_nonce(sender)?)
        },
    )
    .await;
    match result {
        Ok(next_nonce) => Ok(no_store(Json(NonceResponse {
            sender: sender.to_string(),
            next_nonce,
        }))),
        Err(err) => {
            tracing::warn!(error = %err, "GET /nonce failed");
            Err(ApiError::internal_error("nonce unavailable"))
        }
    }
}

async fn get_domain(State(state): State<Arc<ReadState>>) -> Json<DomainResponse> {
    Json(state.domain.clone())
}

/// Quotes go stale within blocks and nonces can go down after recovery, so
/// intermediaries must not serve them from cache.
fn no_store(body: impl IntoResponse) -> Response {
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        body,
    )
        .into_response()
}

/// Normalize JSON-extractor failures into fixed client-facing messages.
/// Keeps the public API contract stable across axum upgrades and avoids
/// reflecting parser internals (serde line/column, token excerpts) to callers.
fn map_json_rejection(err: axum::extract::rejection::JsonRejection) -> ApiError {
    use axum::extract::rejection::JsonRejection;

    tracing::debug!(error = %err, "JSON extraction failed");

    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::payload_too_large("request body too large")
    } else {
        match err {
            JsonRejection::MissingJsonContentType(_) => {
                ApiError::bad_request("missing content type")
            }
            _ => ApiError::bad_request("invalid JSON"),
        }
    }
}

fn enqueue_verified_tx(
    state: &SubmitState,
    signed: SignedUserOp,
) -> Result<oneshot::Receiver<Result<(), crate::ingress::inclusion_lane::SequencerError>>, ApiError>
{
    state.reject_if_shutting_down()?;

    let (respond_to, recv) = oneshot::channel();
    let pending = PendingUserOp {
        signed,
        respond_to,
        received_at: SystemTime::now(),
    };

    match state.tx_sender.try_send(pending) {
        Ok(()) => Ok(recv),
        Err(TrySendError::Full(_)) => Err(ApiError::overloaded("queue full")),
        Err(TrySendError::Closed(_)) => Err(ApiError::unavailable("inclusion lane unavailable")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::{Address, Signature};
    use alloy_sol_types::Eip712Domain;
    use alloy_sol_types::SolStruct;
    use axum::http::StatusCode;
    use k256::ecdsa::SigningKey;
    use k256::ecdsa::signature::hazmat::PrehashSigner;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use crate::storage::Storage;
    use sequencer_core::user_op::UserOp;

    #[test]
    fn closed_lane_is_service_unavailable() {
        let (tx_sender, rx) = mpsc::channel::<PendingUserOp>(1);
        drop(rx);
        let state = SubmitState::new(
            tx_sender,
            Eip712Domain::default(),
            128,
            RuntimeScope::default(),
        );
        let signed = SignedUserOp {
            sender: Address::ZERO,
            signature: Signature::test_signature(),
            user_op: UserOp {
                nonce: 0,
                max_fee: 0,
                data: Vec::new().into(),
            },
        };

        let err = match enqueue_verified_tx(&state, signed) {
            Ok(_) => panic!("closed lane must reject admission"),
            Err(err) => err,
        };
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.code(), "UNAVAILABLE");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn submit_tx_rejects_when_shutdown_has_started() {
        let db = TempDir::new().expect("create temp dir");
        let db_path = db.path().join("sequencer.db");
        let _storage = Storage::open(&db_path.to_string_lossy()).expect("create db");
        let shutdown = RuntimeScope::default();
        shutdown.request_shutdown();

        let (tx_sender, _rx) = mpsc::channel::<PendingUserOp>(1);
        let state = Arc::new(SubmitState::new(
            tx_sender,
            Eip712Domain {
                name: None,
                version: None,
                chain_id: None,
                verifying_contract: None,
                salt: None,
            },
            128,
            shutdown,
        ));

        let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
        let sender = address_from_signing_key(&signing_key);
        let user_op = UserOp {
            nonce: 0,
            max_fee: 0,
            data: Vec::new().into(),
        };
        let request = TxRequest {
            message: user_op.clone(),
            signature: sign_user_op_hex(&state.domain, &user_op, &signing_key),
            sender: sender.to_string(),
        };

        let result = submit_tx(State(state), Ok(Json(request))).await;

        let err = result.expect_err("submit should be rejected during shutdown");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.code(), "UNAVAILABLE");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_fee_rejects_when_shutdown_has_started() {
        let shutdown = RuntimeScope::default();
        shutdown.request_shutdown();
        let state = read_state("unused.db".into(), shutdown);

        let err = get_fee(State(state))
            .await
            .expect_err("fee should be rejected during shutdown");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.code(), "UNAVAILABLE");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_fee_is_unavailable_when_no_open_frame() {
        let db = TempDir::new().expect("create temp dir");
        let db_path = db.path().join("sequencer.db");
        let _storage = Storage::open(&db_path.to_string_lossy()).expect("create db");
        let state = read_state(
            db_path.to_string_lossy().into_owned(),
            RuntimeScope::default(),
        );

        let err = get_fee(State(state))
            .await
            .expect_err("fee requires an open frame");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.code(), "UNAVAILABLE");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn corrupt_fee_policy_trips_terminal_storage_fault() {
        if !crate::runtime::shutdown::abort_test_child(
            "ingress::api::tests::corrupt_fee_policy_trips_terminal_storage_fault",
        ) {
            return;
        }
        let db = TempDir::new().expect("create temp dir");
        let db_path = db.path().join("sequencer.db");
        let mut storage = Storage::open(&db_path.to_string_lossy()).expect("create db");
        storage
            .initialize_open_state(0, crate::storage::SafeInputRange::empty_at(0))
            .expect("open tip");
        drop(storage);

        let conn = Storage::open_connection(&db_path.to_string_lossy()).expect("raw connection");
        conn.execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE batch_policy SET log_delta = -10000 WHERE singleton_id = 0;",
        )
        .expect("inject impossible policy row");
        drop(conn);

        let state = read_state(
            db_path.to_string_lossy().into_owned(),
            RuntimeScope::default(),
        );
        let result = get_fee(State(state)).await;
        panic!(
            "terminal fee fault returned instead of aborting: {:?}",
            result.as_ref().err().map(ApiError::status)
        );
    }

    fn read_state(db_path: String, shutdown: RuntimeScope) -> Arc<ReadState> {
        let domain = sequencer_core::build_input_domain(31337, Address::repeat_byte(0xab));
        Arc::new(ReadState::new(
            db_path,
            DomainResponse::from_domain(&domain).expect("complete domain"),
            shutdown,
        ))
    }

    fn nonce_query(
        uri: &str,
    ) -> Result<Query<NonceQuery>, axum::extract::rejection::QueryRejection> {
        Query::try_from_uri(&uri.parse().expect("test URI"))
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("JSON body")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_nonce_for_an_unseen_sender_is_zero_and_uncacheable() {
        let db = TempDir::new().expect("create temp dir");
        let db_path = db.path().join("sequencer.db");
        let _storage = Storage::open(&db_path.to_string_lossy()).expect("create db");
        let state = read_state(
            db_path.to_string_lossy().into_owned(),
            RuntimeScope::default(),
        );
        let sender = Address::repeat_byte(0xcd);
        let lowercase = format!("{sender:#x}");

        let response = get_nonce(
            State(state),
            nonce_query(&format!("/nonce?sender={lowercase}")),
        )
        .await
        .expect("an unseen sender is an ordinary answer, not a storage fault");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(
            json_body(response).await,
            serde_json::json!({ "sender": sender.to_checksum(None), "next_nonce": 0 })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_nonce_rejects_a_missing_or_malformed_sender() {
        let state = read_state("unused.db".into(), RuntimeScope::default());
        for uri in [
            "/nonce",
            "/nonce?address=0x0000000000000000000000000000000000000000",
            "/nonce?sender=0000000000000000000000000000000000000000",
            "/nonce?sender=0x00",
            "/nonce?sender=0xzz00000000000000000000000000000000000000",
        ] {
            let err = get_nonce(State(state.clone()), nonce_query(uri))
                .await
                .expect_err(uri);
            assert_eq!(err.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(err.to_string(), INVALID_NONCE_QUERY, "{uri}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_nonce_rejects_when_shutdown_has_started() {
        let shutdown = RuntimeScope::default();
        shutdown.request_shutdown();
        let state = read_state("unused.db".into(), shutdown);

        let err = get_nonce(
            State(state),
            nonce_query("/nonce?sender=0x0000000000000000000000000000000000000000"),
        )
        .await
        .expect_err("nonce should be rejected during shutdown");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    fn sign_user_op_hex(
        domain: &Eip712Domain,
        user_op: &UserOp,
        signing_key: &SigningKey,
    ) -> String {
        let hash = user_op.eip712_signing_hash(domain);
        let k256_sig = signing_key
            .sign_prehash(hash.as_slice())
            .expect("sign user op hash");

        let sender = address_from_signing_key(signing_key);
        let signature = [false, true]
            .into_iter()
            .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
            .find(|candidate| {
                candidate
                    .recover_address_from_prehash(&hash)
                    .ok()
                    .map(|value| value == sender)
                    .unwrap_or(false)
            })
            .expect("recoverable parity for signature");

        alloy_primitives::hex::encode_prefixed(signature.as_bytes())
    }

    fn address_from_signing_key(signing_key: &SigningKey) -> Address {
        let verifying = signing_key.verifying_key().to_encoded_point(false);
        Address::from_raw_public_key(&verifying.as_bytes()[1..])
    }

    // ── S-malleability — no alternate signature can recover a different
    // address at our boundary. Structurally guaranteed by alloy+k256; this is
    // a regression lock.

    #[test]
    fn s_malleable_signature_cannot_recover_a_different_address() {
        use alloy_primitives::{B256, U256};

        // secp256k1 curve order `n`. s' = n - s is the canonical malleable
        // transform that pairs with flipped parity to produce an alternate
        // signature recovering the same public key.
        const SECP256K1_N: U256 = U256::from_be_slice(&[
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C,
            0xD0, 0x36, 0x41, 0x41,
        ]);

        let signing_key = SigningKey::from_bytes((&[0x42_u8; 32]).into()).expect("key");
        let expected_sender = address_from_signing_key(&signing_key);

        let msg_hash = B256::from([0xfe_u8; 32]);
        let k256_sig = signing_key
            .sign_prehash(msg_hash.as_slice())
            .expect("sign prehash");

        // k256's `sign_prehash` returns a low-s signature by default. Find the
        // parity that pairs with it to recover the expected signer.
        let valid_sig = [false, true]
            .into_iter()
            .map(|p| Signature::from_signature_and_parity(k256_sig, p))
            .find(|s| {
                s.recover_address_from_prehash(&msg_hash)
                    .ok()
                    .is_some_and(|a| a == expected_sender)
            })
            .expect("low-s signature must recover the signer with one parity");

        // Construct the S-malleable variant: same r, s' = n - s, flipped parity.
        let malleable_sig =
            Signature::new(valid_sig.r(), SECP256K1_N - valid_sig.s(), !valid_sig.v());
        assert_ne!(
            malleable_sig.s(),
            valid_sig.s(),
            "malleable transform must actually change the signature",
        );

        match malleable_sig.recover_address_from_prehash(&msg_hash) {
            Err(_) => {
                // alloy rejected the high-s form (EIP-2 style). Impersonation
                // via malleability is structurally impossible at recovery.
            }
            Ok(addr) => {
                // alloy accepted high-s; it MUST return the same signer.
                // Any other outcome would let an attacker grind a distinct
                // signature that recovers a different address.
                assert_eq!(
                    addr, expected_sender,
                    "malleable signature recovered a DIFFERENT address — impersonation possible",
                );
            }
        }
    }
}
