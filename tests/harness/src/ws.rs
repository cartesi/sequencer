// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use alloy_primitives::Address;
use futures_util::StreamExt;
use sequencer_core::api::WsTxMessage;
use sequencer_rust_client::SequencerClient;
use tokio_tungstenite::tungstenite::Message;

use crate::HarnessResult;
use crate::util::io_other;

const DEFAULT_WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_WS_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

pub struct WsClient {
    stream: sequencer_rust_client::SubscribeStream,
}

impl WsClient {
    pub async fn connect(client: &SequencerClient, from_offset: u64) -> HarnessResult<Self> {
        let stream =
            tokio::time::timeout(DEFAULT_WS_CONNECT_TIMEOUT, client.subscribe(from_offset))
                .await
                .map_err(|_| io_other("timeout connecting websocket"))??;
        Ok(Self { stream })
    }

    pub async fn next_message(&mut self) -> HarnessResult<WsTxMessage> {
        let frame = tokio::time::timeout(DEFAULT_WS_FRAME_TIMEOUT, self.stream.next())
            .await
            .map_err(|_| io_other("wait for websocket frame"))?
            .ok_or_else(|| io_other("websocket stream ended"))?
            .map_err(|err| io_other(format!("receive websocket frame: {err}")))?;

        match frame {
            Message::Text(value) => serde_json::from_str(value.as_str())
                .map_err(|err| io_other(format!("parse ws payload: {err}")).into()),
            other => Err(io_other(format!("expected text ws frame, got {other:?}")).into()),
        }
    }

    pub async fn expect_direct_input_from(
        &mut self,
        expected_sender: Address,
    ) -> HarnessResult<WsTxMessage> {
        let message = self.next_message().await?;
        match &message {
            WsTxMessage::DirectInput { sender, .. } => {
                let actual_sender = parse_sender(sender)?;
                if actual_sender != expected_sender {
                    return Err(io_other(format!(
                        "expected direct input from {expected_sender}, got {actual_sender}"
                    ))
                    .into());
                }
            }
            other => {
                return Err(io_other(format!("expected direct input frame, got {other:?}")).into());
            }
        }
        Ok(message)
    }

    pub async fn expect_user_op_from(
        &mut self,
        expected_sender: Address,
    ) -> HarnessResult<WsTxMessage> {
        let message = self.next_message().await?;
        match &message {
            WsTxMessage::UserOp { sender, .. } => {
                let actual_sender = parse_sender(sender)?;
                if actual_sender != expected_sender {
                    return Err(io_other(format!(
                        "expected user op from {expected_sender}, got {actual_sender}"
                    ))
                    .into());
                }
            }
            other => {
                return Err(io_other(format!("expected user-op frame, got {other:?}")).into());
            }
        }
        Ok(message)
    }

    pub async fn expect_no_message_for(&mut self, duration: Duration) -> HarnessResult<()> {
        match tokio::time::timeout(duration, self.stream.next()).await {
            Err(_) => Ok(()),
            Ok(None) => Err(io_other("websocket stream ended while waiting for no message").into()),
            Ok(Some(Err(err))) => Err(io_other(format!(
                "receive websocket frame while expecting no message: {err}"
            ))
            .into()),
            Ok(Some(Ok(frame))) => Err(io_other(format!(
                "expected no websocket frame for {duration:?}, got {frame:?}"
            ))
            .into()),
        }
    }
}

fn parse_sender(sender: &str) -> HarnessResult<Address> {
    sender
        .parse()
        .map_err(|err| io_other(format!("parse websocket sender '{sender}': {err}")).into())
}
