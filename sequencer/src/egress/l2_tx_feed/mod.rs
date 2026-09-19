// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! DB-backed ordered-L2-tx feed used by WS subscriptions and catch-up replay.

mod error;

#[cfg(test)]
mod tests;

pub use error::{SubscribeError, SubscriptionError};
pub use sequencer_core::broadcast::BroadcastTxMessage;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use sequencer_core::history::{HistoryBounds, HistoryClaim};
use tokio::sync::mpsc;

use crate::runtime::process_lock::spawn_blocking_with_lock;
use crate::runtime::shutdown::{RuntimeScope, abort_terminal};
use crate::storage::{L2TxContext, Storage};

/// Best-effort extraction of a panic payload's message for fault causes.
fn panic_message(payload: &dyn std::any::Any) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

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
    shutdown: RuntimeScope,
}

pub struct Subscription {
    receiver: mpsc::Receiver<BroadcastTxMessage>,
    task: Option<SubscriptionTask>,
    /// Pure notification half: the subscription only waits for stop. The
    /// streaming task holds the scope (and with it the lock) itself.
    shutdown: crate::runtime::shutdown::ShutdownSignal,
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
    pub fn new(db_path: String, shutdown: RuntimeScope, config: L2TxFeedConfig) -> Self {
        Self {
            db_path,
            page_size: config.page_size.max(1),
            idle_poll_interval: config.idle_poll_interval,
            shutdown,
        }
    }

    async fn prepare(&self, claim: HistoryClaim) -> Result<HistoryBounds, SubscribeError> {
        let db_path = self.db_path.clone();
        let prepared = spawn_blocking_with_lock(self.shutdown.process_lock(), move || {
            match catch_unwind(AssertUnwindSafe(|| {
                let mut storage = Storage::open_read_only(&db_path)
                    .map_err(|source| SubscribeError::OpenStorage { source })?;
                storage
                    .canonical_history_page(claim, 0)
                    .map(|page| page.bounds)
                    .map_err(SubscribeError::from)
            })) {
                Ok(Err(error)) if error.is_persistent_storage_invariant() => {
                    abort_terminal(format_args!("preparing tx-feed subscription: {error}"))
                }
                Ok(result) => result,
                Err(payload) => abort_terminal(format_args!(
                    "panic preparing tx-feed subscription: {}",
                    panic_message(&*payload)
                )),
            }
        })
        .await;
        match prepared {
            Ok(result) => result,
            Err(source) if source.is_panic() => {
                abort_terminal(format_args!("preparing tx-feed subscription: {source}"))
            }
            Err(source) => Err(SubscribeError::Join { source }),
        }
    }

    pub async fn subscribe_from(
        &self,
        claim: HistoryClaim,
    ) -> Result<Subscription, SubscribeError> {
        self.prepare(claim).await?;
        let (events_tx, events_rx) = mpsc::channel(SUBSCRIPTION_BUFFER_CAPACITY);
        let db_path = self.db_path.clone();
        let page_size = self.page_size;
        let idle_poll_interval = self.idle_poll_interval;
        let shutdown = self.shutdown.clone();
        let task = tokio::task::spawn_blocking(move || {
            match catch_unwind(AssertUnwindSafe(|| {
                run_subscription(
                    db_path.as_str(),
                    page_size,
                    idle_poll_interval,
                    claim,
                    shutdown.clone(),
                    events_tx,
                )
            })) {
                Ok(Err(error)) if error.is_persistent_storage_invariant() => {
                    abort_terminal(format_args!("reading tx-feed subscription: {error}"));
                }
                Ok(result) => result,
                Err(payload) => abort_terminal(format_args!(
                    "panic reading tx-feed subscription: {}",
                    panic_message(&*payload)
                )),
            }
        });

        Ok(Subscription {
            receiver: events_rx,
            task: Some(task),
            shutdown: self.shutdown.signal(),
        })
    }

    pub(crate) fn runtime_scope(&self) -> RuntimeScope {
        self.shutdown.clone()
    }
}

impl Subscription {
    pub async fn recv(&mut self) -> Option<BroadcastTxMessage> {
        tokio::select! {
            biased;
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

fn run_subscription(
    db_path: &str,
    page_size: usize,
    idle_poll_interval: Duration,
    mut claim: HistoryClaim,
    shutdown: RuntimeScope,
    events_tx: mpsc::Sender<BroadcastTxMessage>,
) -> Result<(), SubscriptionError> {
    let mut storage = Storage::open_read_only(db_path)
        .map_err(|source| SubscriptionError::OpenStorage { source })?;

    loop {
        if shutdown.is_shutdown_requested() || events_tx.is_closed() {
            return Ok(());
        }

        let page =
            storage
                .canonical_history_page(claim, page_size)
                .map_err(|error| match error {
                    crate::storage::HistoryReadError::Storage(source) => {
                        SubscriptionError::LoadReplay {
                            offset: claim.next_input.get(),
                            source,
                        }
                    }
                    crate::storage::HistoryReadError::Policy(source) => {
                        SubscriptionError::History(source)
                    }
                })?;
        claim = page.next_claim();
        if page.rows.is_empty() {
            std::thread::sleep(idle_poll_interval);
            continue;
        }

        for row in page.rows {
            if shutdown.is_shutdown_requested() || events_tx.is_closed() {
                return Ok(());
            }

            let offset = row.offset.get();
            let event = match row.context {
                L2TxContext::UserOp {
                    tx,
                    nonce,
                    safe_block,
                    batch_nonce,
                    ..
                } => BroadcastTxMessage::from_user_op(offset, tx, nonce, safe_block, batch_nonce),
                L2TxContext::DirectInput {
                    tx,
                    input_index,
                    batch_nonce,
                    block_timestamp,
                    transaction_hash,
                    ..
                } => BroadcastTxMessage::from_direct_input(
                    offset,
                    tx,
                    input_index,
                    batch_nonce,
                    block_timestamp,
                    transaction_hash,
                ),
            };
            if events_tx.blocking_send(event).is_err() {
                return Ok(());
            }
        }
    }
}
