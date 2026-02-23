// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod error;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::{DefaultBodyLimit, Json, State};
use axum::routing::post;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::SendTimeoutError;
use tokio::sync::oneshot;
use tower_http::trace::TraceLayer;
use tracing::info;

use alloy_primitives::{Address, Signature};
use alloy_sol_types::{Eip712Domain, SolStruct};

use crate::inclusion_lane::{InclusionLaneInput, PendingUserOp};
use crate::user_op::{SignedUserOp, UserOp};

pub use error::ApiError;

#[derive(Clone)]
pub struct AppState {
    pub tx_sender: mpsc::Sender<InclusionLaneInput>,
    pub domain: Eip712Domain,
    pub queue_timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct TxRequest {
    message: UserOp,
    signature: String,
    sender: Option<String>,
}

#[derive(Debug, Serialize)]
struct TxResponse {
    ok: bool,
    tx_hash: String,
    sender: String,
    nonce: u32,
}

pub fn router(state: Arc<AppState>, max_body_bytes: usize) -> Router {
    Router::new()
        .route("/tx", post(submit_tx))
        .with_state(state)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(TraceLayer::new_for_http())
}

async fn submit_tx(
    State(state): State<Arc<AppState>>,
    req: Result<Json<TxRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<TxResponse>, ApiError> {
    let Json(req) = req.map_err(|err| ApiError::bad_request(format!("invalid JSON: {err}")))?;

    let signature_bytes = decode_hex_0x(&req.signature).map_err(ApiError::bad_request)?;
    if signature_bytes.len() != 65 {
        return Err(ApiError::bad_request("signature must be 65 bytes"));
    }

    let signature = parse_signature(&signature_bytes)?;
    let user_op = req.message;
    let user_op_data_len = user_op.data.len();
    let user_op_size_upper_bound =
        SignedUserOp::batch_bytes_upper_bound_for_data_len(user_op_data_len);
    // Keep over-sized payloads out of the hot path so chunk-level batch checks can stay simple.
    if user_op_size_upper_bound > SignedUserOp::max_batch_bytes_upper_bound() {
        return Err(ApiError::bad_request(format!(
            "user op payload too large: max {} bytes, got {} bytes",
            SignedUserOp::MAX_METHOD_PAYLOAD_BYTES,
            user_op_data_len
        )));
    }
    let nonce = user_op.nonce;

    let tx_hash = user_op.eip712_signing_hash(&state.domain);
    let sender = signature
        .recover_address_from_prehash(&tx_hash)
        .map_err(|_| ApiError::invalid_signature("cannot recover sender"))?;

    if let Some(sender_hex) = req.sender.as_deref() {
        let expected = parse_address(sender_hex).map_err(ApiError::bad_request)?;
        if expected != sender {
            return Err(ApiError::invalid_signature("sender mismatch"));
        }
    }

    let signed = SignedUserOp {
        sender,
        user_op,
        signature,
    };

    let (respond_to, recv) = oneshot::channel();
    let enqueued = PendingUserOp {
        signed,
        tx_hash,
        respond_to,
        received_at: SystemTime::now(),
    };

    enqueue_tx(&state, enqueued).await?;

    let commit_result = recv
        .await
        .map_err(|_| ApiError::internal_error("inclusion lane dropped response"))?;
    commit_result.map_err(ApiError::from)?;

    info!(tx_hash = %encode_hex(&tx_hash), sender = %sender, "tx committed");

    Ok(Json(TxResponse {
        ok: true,
        tx_hash: encode_hex(&tx_hash),
        sender: sender.to_string(),
        nonce,
    }))
}

fn decode_hex_0x(value: &str) -> Result<Vec<u8>, String> {
    if !value.starts_with("0x") {
        return Err("hex string must start with 0x".to_string());
    }
    alloy_primitives::hex::decode(value).map_err(|err| format!("invalid hex: {err}"))
}

fn parse_address(value: &str) -> Result<Address, String> {
    let bytes = decode_hex_0x(value)?;
    if bytes.len() != 20 {
        return Err("address must be 20 bytes".to_string());
    }
    Ok(Address::from_slice(&bytes))
}

fn parse_signature(bytes: &[u8]) -> Result<Signature, ApiError> {
    Signature::from_raw(bytes).map_err(|err| match err {
        alloy_primitives::SignatureError::FromBytes(_) => {
            ApiError::bad_request("signature must be 65 bytes")
        }
        alloy_primitives::SignatureError::FromHex(_) => {
            ApiError::bad_request("invalid signature hex")
        }
        alloy_primitives::SignatureError::InvalidParity(_) => {
            ApiError::invalid_signature("invalid signature parity")
        }
        alloy_primitives::SignatureError::K256(_) => {
            ApiError::invalid_signature("invalid signature")
        }
    })
}

fn encode_hex(value: &alloy_primitives::B256) -> String {
    alloy_primitives::hex::encode_prefixed(value.as_slice())
}

async fn enqueue_tx(state: &AppState, tx: PendingUserOp) -> Result<(), ApiError> {
    match state
        .tx_sender
        .send_timeout(InclusionLaneInput::UserOp(tx), state.queue_timeout)
        .await
    {
        Ok(()) => Ok(()),
        Err(SendTimeoutError::Timeout(_)) => Err(ApiError::overloaded("queue full")),
        Err(SendTimeoutError::Closed(_)) => {
            Err(ApiError::internal_error("inclusion lane unavailable"))
        }
    }
}
