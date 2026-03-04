// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

pub use sequencer_core::broadcast::BroadcastTxMessage;
use tokio::sync::mpsc;

use super::{SubscribeError, SubscriptionError};
use crate::shutdown::ShutdownSignal;
use crate::storage::Storage;

#[derive(Debug, Clone, Copy)]
pub struct L2TxFeedConfig {
    pub idle_poll_interval: Duration,
    pub page_size: usize,
}

#[derive(Clone)]
pub struct L2TxFeed {
    db_path: String,
    page_size: usize,
    idle_poll_interval: Duration,
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
        }
    }
}

impl L2TxFeed {
    pub fn new(db_path: String, shutdown: ShutdownSignal, config: L2TxFeedConfig) -> Self {
        Self {
            db_path,
            page_size: config.page_size.max(1),
            idle_poll_interval: config.idle_poll_interval,
            shutdown,
        }
    }

    pub fn subscribe_from(
        &self,
        from_offset: u64,
        max_catchup_events: u64,
    ) -> Result<Subscription, SubscribeError> {
        let head_offset = load_head_offset(self.db_path.as_str())?;
        let catchup_events = head_offset.saturating_sub(from_offset);
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
        let shutdown = self.shutdown.clone();
        let task = tokio::task::spawn_blocking(move || {
            run_subscription(
                db_path.as_str(),
                page_size,
                idle_poll_interval,
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

fn load_head_offset(db_path: &str) -> Result<u64, SubscribeError> {
    let mut storage = Storage::open_read_only(db_path)
        .map_err(|source| SubscribeError::OpenStorage { source })?;
    storage
        .ordered_l2_tx_count()
        .map_err(|source| SubscribeError::LoadHeadOffset { source })
}

fn run_subscription(
    db_path: &str,
    page_size: usize,
    idle_poll_interval: Duration,
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
            .load_ordered_l2_txs_page_from(next_offset, page_size)
            .map_err(|source| SubscriptionError::LoadReplay {
                offset: next_offset,
                source,
            })?;

        if txs.is_empty() {
            std::thread::sleep(idle_poll_interval);
            continue;
        }

        for tx in txs {
            if shutdown.is_shutdown_requested() || events_tx.is_closed() {
                return Ok(());
            }

            let event = BroadcastTxMessage::from_offset_and_tx(next_offset, tx);
            next_offset = next_offset.saturating_add(1);
            if events_tx.blocking_send(event).is_err() {
                return Ok(());
            }
        }
    }
}
