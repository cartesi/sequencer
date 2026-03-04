// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod error;

use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{DefaultBodyLimit, Json, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::limit::ConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::{BoxError, ServiceBuilder};
use tower_http::trace::TraceLayer;
use tracing::{debug, warn};

use alloy_primitives::{Address, Signature};
use alloy_sol_types::{Eip712Domain, SolStruct};
use sequencer_core::api::{TxRequest, TxResponse};
use sequencer_core::user_op::SignedUserOp;

use crate::inclusion_lane::{InclusionLaneInput, PendingUserOp};
use crate::l2_tx_broadcaster::{BroadcastTxMessage, L2TxBroadcaster};
use crate::storage::Storage;

pub use error::ApiError;

#[derive(Clone)]
pub struct AppState {
    pub tx_sender: mpsc::Sender<InclusionLaneInput>,
    pub domain: Eip712Domain,
    pub overload_max_inflight_submissions: usize,
    pub ws_subscriber_limit: Arc<Semaphore>,
    pub ws_max_catchup_events: u64,
    pub broadcaster: L2TxBroadcaster,
}

#[derive(Debug, Deserialize)]
struct SubscribeQuery {
    from_offset: Option<u64>,
}

pub fn router(state: Arc<AppState>, max_body_bytes: usize) -> Router {
    let tx_concurrency_limit = state.overload_max_inflight_submissions;
    let tx_route = post(submit_tx).layer(
        ServiceBuilder::new()
            .layer(HandleErrorLayer::new(handle_tx_route_error))
            .layer(LoadShedLayer::new())
            .layer(ConcurrencyLimitLayer::new(tx_concurrency_limit)),
    );

    Router::new()
        .route("/tx", tx_route)
        .route("/ws/subscribe", get(subscribe_l2_txs))
        .with_state(state)
        // Enforces a raw request-body cap before JSON deserialization, including whitespace.
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(TraceLayer::new_for_http())
}

async fn submit_tx(
    State(state): State<Arc<AppState>>,
    req: Result<Json<TxRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<TxResponse>, ApiError> {
    let Json(req) = req.map_err(map_json_rejection)?;

    if req.signature.len() != TxRequest::SIGNATURE_HEX_LEN {
        return Err(ApiError::bad_request(format!(
            "signature must be {} hex chars (0x + 65 bytes)",
            TxRequest::SIGNATURE_HEX_LEN
        )));
    }
    if req.sender.len() != TxRequest::ADDRESS_HEX_LEN {
        return Err(ApiError::bad_request(format!(
            "sender must be {} hex chars (0x + 20 bytes)",
            TxRequest::ADDRESS_HEX_LEN
        )));
    }

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

    let signature_bytes = decode_hex_0x(&req.signature).map_err(ApiError::bad_request)?;
    if signature_bytes.len() != 65 {
        return Err(ApiError::bad_request("signature must be 65 bytes"));
    }
    let signature = parse_signature(&signature_bytes)?;
    let nonce = user_op.nonce;

    let signing_hash = user_op.eip712_signing_hash(&state.domain);
    let sender = signature
        .recover_address_from_prehash(&signing_hash)
        .map_err(|_| ApiError::invalid_signature("cannot recover sender"))?;

    let expected = parse_address(req.sender.as_str()).map_err(ApiError::bad_request)?;
    if expected != sender {
        return Err(ApiError::invalid_signature("sender mismatch"));
    }

    let signed = SignedUserOp {
        sender,
        user_op,
        signature,
    };

    let (respond_to, recv) = oneshot::channel();
    let enqueued = PendingUserOp {
        signed,
        respond_to,
        received_at: SystemTime::now(),
    };

    enqueue_tx(&state, enqueued)?;

    let commit_result = recv
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

fn enqueue_tx(state: &AppState, tx: PendingUserOp) -> Result<(), ApiError> {
    match state.tx_sender.try_send(InclusionLaneInput::UserOp(tx)) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(ApiError::overloaded("queue full")),
        Err(TrySendError::Closed(_)) => Err(ApiError::internal_error("inclusion lane unavailable")),
    }
}

async fn handle_tx_route_error(err: BoxError) -> impl IntoResponse {
    if err.is::<tower::load_shed::error::Overloaded>() {
        ApiError::overloaded("tx endpoint overloaded").into_response()
    } else {
        warn!(error = %err, "tx endpoint middleware error");
        ApiError::internal_error("tx endpoint unavailable").into_response()
    }
}

// Keep non-413 JSON extractor failures normalized to 400 for a stable API contract.
fn map_json_rejection(err: axum::extract::rejection::JsonRejection) -> ApiError {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::payload_too_large(format!("request body too large: {err}"))
    } else {
        ApiError::bad_request(format!("invalid JSON: {err}"))
    }
}

async fn subscribe_l2_txs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SubscribeQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let from_offset = query.from_offset.unwrap_or(0);
    let permit = match Arc::clone(&state.ws_subscriber_limit).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return ApiError::overloaded("ws subscriber limit reached").into_response(),
    };
    let broadcaster = state.broadcaster.clone();
    let ws_max_catchup_events = state.ws_max_catchup_events;
    ws.on_upgrade(move |socket| {
        run_broadcaster_session(
            broadcaster,
            socket,
            from_offset,
            permit,
            ws_max_catchup_events,
        )
    })
    .into_response()
}

async fn run_broadcaster_session(
    broadcaster: L2TxBroadcaster,
    mut socket: WebSocket,
    from_offset: u64,
    _subscriber_permit: OwnedSemaphorePermit,
    ws_max_catchup_events: u64,
) {
    let mut subscription = broadcaster.subscribe();
    let mut next_offset = from_offset;

    if next_offset < subscription.live_start_offset {
        let catchup_events = subscription.live_start_offset - next_offset;
        if catchup_events > ws_max_catchup_events {
            warn!(
                requested_offset = next_offset,
                live_start_offset = subscription.live_start_offset,
                max_catchup_events = ws_max_catchup_events,
                "ws catch-up window exceeded; closing subscriber"
            );
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: close_code::POLICY,
                    reason: "catch-up window exceeded".into(),
                })))
                .await;
            return;
        }
        if send_catch_up(
            &broadcaster,
            &mut socket,
            next_offset,
            subscription.live_start_offset,
        )
        .await
        .is_err()
        {
            return;
        }
        next_offset = subscription.live_start_offset;
    }

    loop {
        tokio::select! {
            maybe_event = subscription.receiver.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                let offset = event.offset();
                if offset < next_offset {
                    continue;
                }
                if offset != next_offset {
                    warn!(
                        expected_offset = next_offset,
                        received_offset = offset,
                        "broadcaster detected gap in live stream"
                    );
                    break;
                }
                if send_ws_event(&mut socket, &event).await.is_err() {
                    break;
                }
                next_offset = next_offset.saturating_add(1);
            }
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

async fn send_catch_up(
    broadcaster: &L2TxBroadcaster,
    socket: &mut WebSocket,
    from_offset: u64,
    to_offset: u64,
) -> Result<(), ()> {
    if from_offset >= to_offset {
        return Ok(());
    }

    let (events_tx, mut events_rx) = mpsc::channel::<BroadcastTxMessage>(1024);
    let db_path = broadcaster.db_path();
    let page_size = broadcaster.page_size();
    let worker = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut storage = Storage::open_read_only(&db_path)
            .map_err(|err| format!("open catch-up storage failed: {err}"))?;
        let mut next_offset = from_offset;

        while next_offset < to_offset {
            let remaining = (to_offset - next_offset) as usize;
            let page_limit = remaining.min(page_size.max(1));
            let txs = storage
                .load_ordered_l2_txs_page_from(next_offset, page_limit)
                .map_err(|err| format!("read catch-up page from {next_offset} failed: {err}"))?;
            if txs.is_empty() {
                return Err(format!(
                    "catch-up reached sparse range [{next_offset}, {to_offset})"
                ));
            }

            for tx in txs {
                let event = BroadcastTxMessage::from_offset_and_tx(next_offset, tx);
                next_offset = next_offset.saturating_add(1);
                if events_tx.blocking_send(event).is_err() {
                    debug!("broadcaster catch-up worker stopping early: receiver channel closed");
                    return Ok(());
                }
            }
        }
        Ok(())
    });

    let mut ws_send_failed = false;
    while let Some(event) = events_rx.recv().await {
        if send_ws_event(socket, &event).await.is_err() {
            ws_send_failed = true;
            break;
        }
    }
    drop(events_rx);

    let worker_result = match worker.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(reason)) => {
            warn!(reason, "broadcaster catch-up worker exited with error");
            Err(())
        }
        Err(err) => {
            warn!(error = %err, "broadcaster catch-up worker join failed");
            Err(())
        }
    };

    if ws_send_failed {
        return Err(());
    }
    worker_result
}

async fn send_ws_event(socket: &mut WebSocket, event: &BroadcastTxMessage) -> Result<(), ()> {
    let payload = match serde_json::to_string(event) {
        Ok(value) => value,
        Err(err) => {
            warn!(error = %err, "broadcaster failed to serialize tx event");
            return Err(());
        }
    };

    if socket.send(Message::Text(payload.into())).await.is_err() {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn tx_route_internal_errors_are_sanitized() {
        let err: BoxError = std::io::Error::other("sensitive middleware detail").into();
        let response = handle_tx_route_error(err).await.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let body = String::from_utf8(body.to_vec()).expect("utf8 response body");

        assert!(
            body.contains("INTERNAL_ERROR"),
            "expected internal error code in body: {body}"
        );
        assert!(
            body.contains("tx endpoint unavailable"),
            "expected sanitized internal message in body: {body}"
        );
        assert!(
            !body.contains("sensitive middleware detail"),
            "middleware internals leaked in body: {body}"
        );
    }
}
