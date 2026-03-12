// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::OwnedSemaphorePermit;
use tracing::warn;

use crate::l2_tx_feed::{BroadcastTxMessage, L2TxFeed, SubscribeError};

use super::{ApiState, WS_CATCHUP_WINDOW_EXCEEDED_REASON};

const MAX_INBOUND_WS_MESSAGE_SIZE: usize = 8 * 1024;
const MAX_INBOUND_WS_FRAME_SIZE: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
pub(super) struct SubscribeQuery {
    from_offset: Option<u64>,
}

pub(super) async fn subscribe_l2_txs(
    State(state): State<Arc<ApiState>>,
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
    let mut subscription = match tx_feed.subscribe_from(from_offset, ws_max_catchup_events) {
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
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: close_code::POLICY,
                    reason: WS_CATCHUP_WINDOW_EXCEEDED_REASON.into(),
                })))
                .await;
            return;
        }
        Err(SubscribeError::OpenStorage { source }) => {
            warn!(error = %source, "ws subscription failed to open replay storage");
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: close_code::ERROR,
                    reason: "subscription unavailable".into(),
                })))
                .await;
            return;
        }
        Err(SubscribeError::LoadHeadOffset { source }) => {
            warn!(error = %source, "ws subscription failed to read replay head");
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: close_code::ERROR,
                    reason: "subscription unavailable".into(),
                })))
                .await;
            return;
        }
    };

    loop {
        tokio::select! {
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

    if socket.send(Message::Text(payload.into())).await.is_err() {
        return Err(());
    }
    Ok(())
}
