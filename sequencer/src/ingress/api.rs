// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `POST /tx` — validate a signed user op, enqueue it for the inclusion lane,
//! and wait for the lane's commit ack before responding. Synchronous from the
//! client's perspective: 200 means included.

use std::sync::Arc;
use std::time::SystemTime;

use alloy_sol_types::Eip712Domain;
use axum::Router;
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::routing::post;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::oneshot;
use tracing::debug;

use crate::http::ApiError;
use crate::ingress::inclusion_lane::PendingUserOp;
use crate::runtime::shutdown::ShutdownSignal;
use sequencer_core::api::{TxRequest, TxResponse};
use sequencer_core::user_op::SignedUserOp;

/// State for the submit endpoint. Kept narrow — only what `/tx` actually needs.
#[derive(Clone)]
pub(crate) struct SubmitState {
    pub tx_sender: mpsc::Sender<PendingUserOp>,
    pub domain: Eip712Domain,
    pub max_user_op_data_bytes: usize,
    pub shutdown: ShutdownSignal,
}

impl SubmitState {
    pub(crate) fn new(
        tx_sender: mpsc::Sender<PendingUserOp>,
        domain: Eip712Domain,
        max_user_op_data_bytes: usize,
        shutdown: ShutdownSignal,
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

/// Build the ingress router. Caller wires it into an `axum::serve` listener.
pub(crate) fn router(state: Arc<SubmitState>) -> Router {
    Router::new()
        .route("/tx", post(submit_tx))
        .with_state(state)
}

async fn submit_tx(
    State(state): State<Arc<SubmitState>>,
    req: Result<Json<TxRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<TxResponse>, ApiError> {
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
    }))
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
        Err(TrySendError::Closed(_)) => Err(ApiError::internal_error("inclusion lane unavailable")),
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

    #[tokio::test(flavor = "current_thread")]
    async fn submit_tx_rejects_when_shutdown_has_started() {
        let db = TempDir::new().expect("create temp dir");
        let db_path = db.path().join("sequencer.db");
        let _storage = Storage::open(&db_path.to_string_lossy()).expect("create db");
        let shutdown = ShutdownSignal::default();
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

    // ── §1.7 S-malleability — no alternate signature can recover a different
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
