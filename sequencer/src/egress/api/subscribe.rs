// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `GET /ws/subscribe` — replay-then-live stream of ordered L2 txs.
//! Acquires a subscriber permit before upgrading; permit is held for the
//! lifetime of the session and released on disconnect via `Drop`.

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::OwnedSemaphorePermit;
use tracing::warn;

use crate::egress::l2_tx_feed::{BroadcastTxMessage, L2TxFeed, SubscribeError};
use crate::http::WS_CATCHUP_WINDOW_EXCEEDED_REASON;

use super::SubscribeState;

const MAX_INBOUND_WS_MESSAGE_SIZE: usize = 8 * 1024;
const MAX_INBOUND_WS_FRAME_SIZE: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
pub(crate) struct SubscribeQuery {
    from_offset: Option<u64>,
}

pub(crate) async fn subscribe_l2_txs(
    State(state): State<Arc<SubscribeState>>,
    Query(query): Query<SubscribeQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(err) = state.reject_if_shutting_down() {
        return err.into_response();
    }

    let from_offset = query.from_offset.unwrap_or(0);
    let permit = match state.try_acquire_ws_subscriber_permit() {
        Ok(permit) => permit,
        Err(err) => return err.into_response(),
    };
    let tx_feed = state.tx_feed.clone();
    let ws_max_catchup_events = state.ws_max_catchup_events;

    ws.max_message_size(MAX_INBOUND_WS_MESSAGE_SIZE)
        .max_frame_size(MAX_INBOUND_WS_FRAME_SIZE)
        .on_upgrade(move |socket| {
            run_ws_session(tx_feed, socket, from_offset, permit, ws_max_catchup_events)
        })
        .into_response()
}

async fn run_ws_session(
    tx_feed: L2TxFeed,
    mut socket: WebSocket,
    from_offset: u64,
    _subscriber_permit: OwnedSemaphorePermit,
    ws_max_catchup_events: u64,
) {
    let shutdown = tx_feed.runtime_scope();
    let mut subscription = match tx_feed
        .subscribe_from(from_offset, ws_max_catchup_events)
        .await
    {
        Ok(subscription) => subscription,
        Err(SubscribeError::CatchUpWindowExceeded {
            requested_offset,
            live_start_offset,
            max_catchup_events,
        }) => {
            warn!(
                requested_offset,
                live_start_offset,
                max_catchup_events,
                "ws catch-up window exceeded; closing subscriber"
            );
            let reason = format!(
                "{WS_CATCHUP_WINDOW_EXCEEDED_REASON}: live_start_offset={live_start_offset}"
            );
            close_with_frame(&mut socket, close_code::POLICY, reason.as_str(), &shutdown).await;
            return;
        }
        Err(SubscribeError::OpenStorage { source }) => {
            warn!(error = %source, "ws subscription failed to open replay storage");
            close_with_frame(
                &mut socket,
                close_code::ERROR,
                "subscription unavailable",
                &shutdown,
            )
            .await;
            return;
        }
        Err(SubscribeError::LoadHeadOffset { source }) => {
            warn!(error = %source, "ws subscription failed to read replay head");
            close_with_frame(
                &mut socket,
                close_code::ERROR,
                "subscription unavailable",
                &shutdown,
            )
            .await;
            return;
        }
        Err(SubscribeError::StorageInvariantViolation) => {
            warn!("ws subscription encountered a persistent storage invariant failure");
            close_with_frame(
                &mut socket,
                close_code::ERROR,
                "subscription unavailable",
                &shutdown,
            )
            .await;
            return;
        }
    };

    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for_shutdown() => break,
            maybe_event = subscription.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                if send_ws_event(&mut socket, &event, &shutdown).await.is_err() {
                    break;
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if send_ws_message(&mut socket, Message::Pong(payload), &shutdown)
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

async fn close_with_frame(
    socket: &mut WebSocket,
    code: u16,
    reason: &str,
    shutdown: &crate::runtime::shutdown::RuntimeScope,
) {
    let _ = send_ws_message(
        socket,
        Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })),
        shutdown,
    )
    .await;
}

async fn send_ws_event(
    socket: &mut WebSocket,
    event: &BroadcastTxMessage,
    shutdown: &crate::runtime::shutdown::RuntimeScope,
) -> Result<(), ()> {
    let payload = match serde_json::to_string(event) {
        Ok(value) => value,
        Err(err) => {
            warn!(error = %err, "tx feed failed to serialize tx event");
            return Err(());
        }
    };

    send_ws_message(socket, Message::Text(payload.into()), shutdown).await
}

/// The WS externalization primitive: emitting requires the token, so a new
/// frame-sending site cannot skip the containment consult (S-A).
async fn send_ws_message(
    socket: &mut WebSocket,
    message: Message,
    shutdown: &crate::runtime::shutdown::RuntimeScope,
) -> Result<(), ()> {
    let Some(auth) = shutdown.authorize() else {
        return Err(());
    };
    send_authorized(auth, socket, message).await
}

async fn send_authorized(
    _auth: crate::runtime::shutdown::Authorized<'_>,
    socket: &mut WebSocket,
    message: Message,
) -> Result<(), ()> {
    match socket.send(message).await {
        Ok(()) => Ok(()),
        Err(_) => Err(()),
    }
}
