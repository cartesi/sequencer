// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `GET /ws/subscribe` — replay-then-live stream of ordered L2 txs.
//! Acquires a subscriber permit before upgrading; permit is held for the
//! lifetime of the session and released on disconnect via `Drop`.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::OwnedSemaphorePermit;
use tracing::warn;

use crate::egress::l2_tx_feed::Subscription;
use crate::egress::l2_tx_feed::{BroadcastTxMessage, L2TxFeed, SubscribeError};
use crate::http::ApiError;
use axum::{Json, http::StatusCode};
use sequencer_core::history::{
    EraId, ExecutedInputCount, HistoryClaim, HistoryVersion, RecoveryGeneration,
};

use super::SubscribeState;

const MAX_INBOUND_WS_MESSAGE_SIZE: usize = 8 * 1024;
const MAX_INBOUND_WS_FRAME_SIZE: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
pub(crate) struct SubscribeQuery {
    era_id: EraId,
    recovery_generation: RecoveryGeneration,
    next_input: ExecutedInputCount,
}

pub(crate) async fn subscribe_l2_txs(
    State(state): State<Arc<SubscribeState>>,
    Query(query): Query<SubscribeQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(err) = state.reject_if_shutting_down() {
        return err.into_response();
    }

    let claim = HistoryClaim {
        version: HistoryVersion {
            era_id: query.era_id,
            recovery_generation: query.recovery_generation,
        },
        next_input: query.next_input,
    };
    let permit = match state.try_acquire_ws_subscriber_permit() {
        Ok(permit) => permit,
        Err(err) => return err.into_response(),
    };
    let tx_feed = state.tx_feed.clone();
    let subscription = match tx_feed.subscribe_from(claim).await {
        Ok(subscription) => subscription,
        Err(SubscribeError::History(error)) => {
            // WebSocket clients may stop reading after the HTTP headers. Keep
            // the structured refusal available even when its body arrives later.
            let policy = serde_json::to_string(&error).expect("history policy serializes");
            return (
                StatusCode::CONFLICT,
                [("X-History-Error", policy)],
                Json(error),
            )
                .into_response();
        }
        Err(error) => {
            warn!(%error, "ws subscription unavailable");
            return ApiError::unavailable("subscription unavailable").into_response();
        }
    };

    ws.max_message_size(MAX_INBOUND_WS_MESSAGE_SIZE)
        .max_frame_size(MAX_INBOUND_WS_FRAME_SIZE)
        .on_upgrade(move |socket| run_ws_session(tx_feed, socket, subscription, permit))
        .into_response()
}

async fn run_ws_session(
    tx_feed: L2TxFeed,
    mut socket: WebSocket,
    mut subscription: Subscription,
    _subscriber_permit: OwnedSemaphorePermit,
) {
    let shutdown = tx_feed.runtime_scope();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for_shutdown() => break,
            maybe_event = subscription.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                if send_ws_event(&mut socket, &event).await.is_err() {
                    break;
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if send_ws_message(&mut socket, Message::Pong(payload))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    if let Err(err) = subscription.finish().await {
        warn!(error = %err, "tx feed subscription cleanup failed");
    }
}

async fn send_ws_event(socket: &mut WebSocket, event: &BroadcastTxMessage) -> Result<(), ()> {
    let payload = match serde_json::to_string(event) {
        Ok(value) => value,
        Err(err) => {
            warn!(error = %err, "tx feed failed to serialize tx event");
            return Err(());
        }
    };

    send_ws_message(socket, Message::Text(payload.into())).await
}

async fn send_ws_message(socket: &mut WebSocket, message: Message) -> Result<(), ()> {
    socket.send(message).await.map_err(|_| ())
}
