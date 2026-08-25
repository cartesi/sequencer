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

use alloy_primitives::Address;
use tokio::sync::mpsc;

use crate::runtime::process_lock::spawn_blocking_with_lock;
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::{OrderedL2TxRow, Storage};

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
    /// Address of the batch submitter wallet. Direct inputs from this sender
    /// are skipped before WS delivery (they're our own batch submissions).
    /// One of I11's three consumer-side sender checks — keep them in sync.
    pub batch_submitter_address: Address,
}

#[derive(Clone)]
pub struct L2TxFeed {
    db_path: String,
    page_size: usize,
    idle_poll_interval: Duration,
    batch_submitter_address: Address,
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

impl L2TxFeedConfig {
    /// The only constructor: the submitter address is mandatory, so a feed
    /// that fans out our own batch envelopes is unconstructible.
    pub fn new(batch_submitter_address: Address) -> Self {
        Self {
            idle_poll_interval: DEFAULT_IDLE_POLL_INTERVAL,
            page_size: DEFAULT_PAGE_SIZE,
            batch_submitter_address,
        }
    }
}

impl L2TxFeed {
    pub fn new(db_path: String, shutdown: RuntimeScope, config: L2TxFeedConfig) -> Self {
        Self {
            db_path,
            page_size: config.page_size.max(1),
            idle_poll_interval: config.idle_poll_interval,
            batch_submitter_address: config.batch_submitter_address,
            shutdown,
        }
    }

    pub async fn subscribe_from(
        &self,
        from_offset: u64,
        max_catchup_events: u64,
    ) -> Result<Subscription, SubscribeError> {
        // Blocking SQLite (an open plus a COUNT over up to
        // `max_catchup_events` rows) runs on the blocking pool, making this
        // signature's `async` honest; the join classifies a decoder panic, so
        // the prepare phase needs no inline `catch_unwind`. The
        // streaming task below keeps its `catch_unwind` deliberately: its
        // only join point is `Subscription::finish`, and containment must
        // fire at the fault, not when the socket unwinds. Cancelling the
        // awaiting WS task detaches started blocking work, so the prepare
        // closure independently retains the process lock until its SQLite
        // work ends.
        let prepare = {
            let db_path = self.db_path.clone();
            let batch_submitter_address = self.batch_submitter_address;
            spawn_blocking_with_lock(self.shutdown.process_lock(), move || {
                load_catchup_info(
                    db_path.as_str(),
                    from_offset,
                    max_catchup_events,
                    batch_submitter_address,
                )
            })
            .await
        };
        let (head_offset, catchup_events) = match prepare {
            Ok(Ok(info)) => info,
            Ok(Err(error)) if error.is_persistent_storage_invariant() => {
                tracing::error!(
                    error = %error,
                    "persistent storage invariant violation while preparing tx-feed subscription"
                );
                self.shutdown.contain_storage_invariant_failure(format!(
                    "preparing tx-feed subscription: {error}"
                ));
                return Err(SubscribeError::StorageInvariantViolation);
            }
            Ok(Err(error)) => return Err(error),
            Err(join) if join.is_panic() => {
                let payload = join.into_panic();
                let message = panic_message(&*payload);
                tracing::error!(
                    panic = message,
                    "storage invariant violation while preparing tx-feed subscription"
                );
                self.shutdown.contain_storage_invariant_failure(format!(
                    "panic preparing tx-feed subscription: {message}"
                ));
                return Err(SubscribeError::StorageInvariantViolation);
            }
            // Not a panic: the runtime is tearing down and cancelled the
            // blocking task before it started. Nothing to contain.
            Err(join) => {
                tracing::warn!(error = %join, "tx-feed prepare task did not run");
                return Err(SubscribeError::StorageInvariantViolation);
            }
        };
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
            match catch_unwind(AssertUnwindSafe(|| {
                run_subscription(
                    db_path.as_str(),
                    page_size,
                    idle_poll_interval,
                    batch_submitter_address,
                    from_offset,
                    shutdown.clone(),
                    events_tx,
                )
            })) {
                Ok(Err(error)) if error.is_persistent_storage_invariant() => {
                    tracing::error!(
                        error = %error,
                        "persistent storage invariant violation while reading tx-feed subscription"
                    );
                    shutdown.contain_storage_invariant_failure(format!(
                        "reading tx-feed subscription: {error}"
                    ));
                    Err(SubscriptionError::StorageInvariantViolation)
                }
                Ok(result) => result,
                Err(payload) => {
                    let message = panic_message(&*payload);
                    tracing::error!(
                        panic = message,
                        "storage invariant violation while reading tx-feed subscription"
                    );
                    shutdown.contain_storage_invariant_failure(format!(
                        "panic reading tx-feed subscription: {message}"
                    ));
                    Err(SubscriptionError::StorageInvariantViolation)
                }
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

/// Returns `(head_offset, broadcastable_event_count_after_from_offset)`.
///
/// Counts events the client will actually receive — excludes invalidated batches
/// and batch-submitter direct inputs (which are filtered before WS delivery).
fn load_catchup_info(
    db_path: &str,
    from_offset: u64,
    max_catchup_events: u64,
    batch_submitter_address: Address,
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
    batch_submitter_address: Address,
    from_offset: u64,
    shutdown: RuntimeScope,
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
            .ordered_l2_tx_rows_page_from(next_offset, page_size)
            .map_err(|source| SubscriptionError::LoadReplay {
                offset: next_offset,
                source,
            })?;

        if txs.is_empty() {
            std::thread::sleep(idle_poll_interval);
            continue;
        }

        for row in txs {
            if shutdown.is_shutdown_requested() || events_tx.is_closed() {
                return Ok(());
            }

            next_offset = row.offset();
            let event = match row {
                OrderedL2TxRow::UserOp {
                    offset,
                    tx,
                    nonce,
                    safe_block,
                    batch_nonce,
                    ..
                } => BroadcastTxMessage::from_user_op(offset, tx, nonce, safe_block, batch_nonce),
                OrderedL2TxRow::DirectInput {
                    offset,
                    tx,
                    input_index,
                    batch_nonce,
                    block_timestamp,
                    transaction_hash,
                    ..
                } => {
                    if tx.sender == batch_submitter_address {
                        continue;
                    }
                    BroadcastTxMessage::from_direct_input(
                        offset,
                        tx,
                        input_index,
                        batch_nonce,
                        block_timestamp,
                        transaction_hash,
                    )
                }
            };
            if events_tx.blocking_send(event).is_err() {
                return Ok(());
            }
        }
    }
}
