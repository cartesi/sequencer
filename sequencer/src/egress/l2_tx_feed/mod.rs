// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! DB-backed ordered-L2-tx feed used by WS subscriptions and catch-up replay.

mod error;

#[cfg(test)]
mod tests;

pub use error::{SubscribeError, SubscriptionError};
pub use sequencer_core::broadcast::BroadcastTxMessage;

use std::time::Duration;

use alloy_primitives::Address;
use sequencer_core::l2_tx::SequencedL2Tx;
use tokio::sync::mpsc;

use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::Storage;

#[derive(Debug, Clone, Copy)]
pub struct L2TxFeedConfig {
    pub idle_poll_interval: Duration,
    pub page_size: usize,
    pub batch_submitter_address: Option<Address>,
}

#[derive(Clone)]
pub struct L2TxFeed {
    db_path: String,
    page_size: usize,
    idle_poll_interval: Duration,
    batch_submitter_address: Option<Address>,
    shutdown: ShutdownSignal,
}

pub struct Subscription {
    receiver: mpsc::Receiver<BroadcastTxMessage>,
    task: Option<SubscriptionTask>,
    shutdown: ShutdownSignal,
}

type SubscriptionTask = tokio::task::JoinHandle<Result<(), SubscriptionError>>;

const DEFAULT_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const DEFAULT_PAGE_SIZE: usize = 256;
const SUBSCRIPTION_BUFFER_CAPACITY: usize = 1024;

impl Default for L2TxFeedConfig {
    fn default() -> Self {
        Self {
            idle_poll_interval: DEFAULT_IDLE_POLL_INTERVAL,
            page_size: DEFAULT_PAGE_SIZE,
            batch_submitter_address: None,
        }
    }
}

impl L2TxFeed {
    pub fn new(db_path: String, shutdown: ShutdownSignal, config: L2TxFeedConfig) -> Self {
        Self {
            db_path,
            page_size: config.page_size.max(1),
            idle_poll_interval: config.idle_poll_interval,
            batch_submitter_address: config.batch_submitter_address,
            shutdown,
        }
    }

    pub fn subscribe_from(
        &self,
        from_offset: u64,
        max_catchup_events: u64,
    ) -> Result<Subscription, SubscribeError> {
        let (head_offset, catchup_events) = load_catchup_info(
            self.db_path.as_str(),
            from_offset,
            max_catchup_events,
            self.batch_submitter_address,
        )?;
        if catchup_events > max_catchup_events {
            return Err(SubscribeError::CatchUpWindowExceeded {
                requested_offset: from_offset,
                live_start_offset: head_offset,
                max_catchup_events,
            });
        }

        let (events_tx, events_rx) = mpsc::channel(SUBSCRIPTION_BUFFER_CAPACITY);
        let db_path = self.db_path.clone();
        let page_size = self.page_size;
        let idle_poll_interval = self.idle_poll_interval;
        let batch_submitter_address = self.batch_submitter_address;
        let shutdown = self.shutdown.clone();
        let task = tokio::task::spawn_blocking(move || {
            run_subscription(
                db_path.as_str(),
                page_size,
                idle_poll_interval,
                batch_submitter_address,
                from_offset,
                shutdown,
                events_tx,
            )
        });

        Ok(Subscription {
            receiver: events_rx,
            task: Some(task),
            shutdown: self.shutdown.clone(),
        })
    }
}

impl Subscription {
    pub async fn recv(&mut self) -> Option<BroadcastTxMessage> {
        tokio::select! {
            _ = self.shutdown.wait_for_shutdown() => None,
            maybe_event = self.receiver.recv() => maybe_event,
        }
    }

    pub async fn finish(mut self) -> Result<(), SubscriptionError> {
        let task = self.task.take();
        self.receiver.close();
        drop(self.receiver);

        let Some(task) = task else {
            return Ok(());
        };

        match task.await {
            Ok(result) => result,
            Err(source) => Err(SubscriptionError::Join { source }),
        }
    }
}

/// Returns `(head_offset, broadcastable_event_count_after_from_offset)`.
///
/// Counts events the client will actually receive — excludes invalidated batches
/// and batch-submitter direct inputs (which are filtered before WS delivery).
fn load_catchup_info(
    db_path: &str,
    from_offset: u64,
    max_catchup_events: u64,
    batch_submitter_address: Option<Address>,
) -> Result<(u64, u64), SubscribeError> {
    let mut storage = Storage::open_read_only(db_path)
        .map_err(|source| SubscribeError::OpenStorage { source })?;
    let head_offset = storage
        .ordered_l2_tx_head_offset()
        .map_err(|source| SubscribeError::LoadHeadOffset { source })?;
    let catchup_count = storage
        .count_broadcastable_events_after(
            from_offset,
            max_catchup_events.saturating_add(1),
            batch_submitter_address,
        )
        .map_err(|source| SubscribeError::LoadHeadOffset { source })?;
    Ok((head_offset, catchup_count))
}

fn run_subscription(
    db_path: &str,
    page_size: usize,
    idle_poll_interval: Duration,
    batch_submitter_address: Option<Address>,
    from_offset: u64,
    shutdown: ShutdownSignal,
    events_tx: mpsc::Sender<BroadcastTxMessage>,
) -> Result<(), SubscriptionError> {
    let mut storage = Storage::open_read_only(db_path)
        .map_err(|source| SubscriptionError::OpenStorage { source })?;
    let mut next_offset = from_offset;

    loop {
        if shutdown.is_shutdown_requested() || events_tx.is_closed() {
            return Ok(());
        }

        let txs = storage
            .ordered_l2_txs_page_from(next_offset, page_size)
            .map_err(|source| SubscriptionError::LoadReplay {
                offset: next_offset,
                source,
            })?;

        if txs.is_empty() {
            std::thread::sleep(idle_poll_interval);
            continue;
        }

        // The frame safe_block (third element) is a replay-only concern;
        // the WS message shape doesn't carry it (review F7/WP5 owns any
        // feed-protocol extension).
        for (db_offset, tx, _frame_safe_block) in txs {
            if shutdown.is_shutdown_requested() || events_tx.is_closed() {
                return Ok(());
            }

            next_offset = db_offset;

            if should_filter_from_broadcast(&tx, batch_submitter_address) {
                continue;
            }

            let event = BroadcastTxMessage::from_offset_and_tx(db_offset, tx);
            if events_tx.blocking_send(event).is_err() {
                return Ok(());
            }
        }
    }
}

fn should_filter_from_broadcast(
    tx: &SequencedL2Tx,
    batch_submitter_address: Option<Address>,
) -> bool {
    matches!(
        (tx, batch_submitter_address),
        (SequencedL2Tx::Direct(direct), Some(batch_submitter_address))
            if direct.sender == batch_submitter_address
    )
}
