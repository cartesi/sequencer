// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod profiling;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::warn;

use self::profiling::{BroadcasterMetrics, FanoutOutcome};
use crate::l2_tx::SequencedL2Tx;
use crate::storage::Storage;

#[derive(Debug, Clone, Copy)]
pub struct L2TxBroadcasterConfig {
    pub idle_poll_interval: Duration,
    pub page_size: usize,
    pub subscriber_buffer_capacity: usize,
    pub metrics_enabled: bool,
    pub metrics_log_interval: Duration,
}

#[derive(Clone)]
pub struct L2TxBroadcaster {
    inner: Arc<L2TxBroadcasterInner>,
}

pub struct LiveSubscription {
    pub receiver: mpsc::Receiver<BroadcastTxMessage>,
    pub live_start_offset: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BroadcastTxMessage {
    UserOp {
        offset: u64,
        sender: String,
        fee: u64,
        data: String,
    },
    DirectInput {
        offset: u64,
        payload: String,
    },
}

struct L2TxBroadcasterInner {
    db_path: String,
    page_size: usize,
    subscriber_buffer_capacity: usize,
    head_offset: AtomicU64,
    next_subscriber_id: AtomicU64,
    stop_requested: AtomicBool,
    subscribers: Mutex<HashMap<u64, mpsc::Sender<BroadcastTxMessage>>>,
}

impl L2TxBroadcaster {
    pub fn start(
        db_path: String,
        config: L2TxBroadcasterConfig,
    ) -> std::result::Result<Self, String> {
        let mut storage = Storage::open_read_only(&db_path)
            .map_err(|err| format!("open broadcaster storage failed: {err}"))?;
        let head_offset = storage
            .ordered_l2_tx_count()
            .map_err(|err| format!("load broadcaster head offset failed: {err}"))?;

        let inner = Arc::new(L2TxBroadcasterInner {
            db_path,
            page_size: config.page_size.max(1),
            subscriber_buffer_capacity: config.subscriber_buffer_capacity.max(1),
            head_offset: AtomicU64::new(head_offset),
            next_subscriber_id: AtomicU64::new(0),
            stop_requested: AtomicBool::new(false),
            subscribers: Mutex::new(HashMap::new()),
        });

        let worker_inner = Arc::clone(&inner);
        tokio::task::spawn_blocking(move || {
            run_poller(
                worker_inner,
                config.idle_poll_interval,
                config.metrics_enabled,
                config.metrics_log_interval,
            );
        });

        Ok(Self { inner })
    }

    pub fn request_shutdown(&self) {
        self.inner.stop_requested.store(true, Ordering::Relaxed);
    }

    pub fn subscribe(&self) -> LiveSubscription {
        let (tx, rx) = mpsc::channel(self.inner.subscriber_buffer_capacity);
        let subscriber_id = self
            .inner
            .next_subscriber_id
            .fetch_add(1, Ordering::Relaxed);

        let mut subscribers = self
            .inner
            .subscribers
            .lock()
            .expect("l2 tx broadcaster subscribers mutex poisoned");
        subscribers.insert(subscriber_id, tx);
        let live_start_offset = self.inner.head_offset.load(Ordering::Acquire);

        LiveSubscription {
            receiver: rx,
            live_start_offset,
        }
    }

    pub fn db_path(&self) -> String {
        self.inner.db_path.clone()
    }

    pub fn page_size(&self) -> usize {
        self.inner.page_size
    }
}

impl BroadcastTxMessage {
    pub fn offset(&self) -> u64 {
        match self {
            Self::UserOp { offset, .. } => *offset,
            Self::DirectInput { offset, .. } => *offset,
        }
    }

    pub fn from_offset_and_tx(offset: u64, tx: SequencedL2Tx) -> Self {
        match tx {
            SequencedL2Tx::UserOp(user_op) => Self::UserOp {
                offset,
                sender: user_op.sender.to_string(),
                fee: user_op.fee,
                data: alloy_primitives::hex::encode_prefixed(user_op.data.as_slice()),
            },
            SequencedL2Tx::Direct(direct) => Self::DirectInput {
                offset,
                payload: alloy_primitives::hex::encode_prefixed(direct.payload.as_slice()),
            },
        }
    }
}

fn run_poller(
    inner: Arc<L2TxBroadcasterInner>,
    idle_poll_interval: Duration,
    metrics_enabled: bool,
    metrics_log_interval: Duration,
) {
    let mut storage = match Storage::open_read_only(inner.db_path.as_str()) {
        Ok(storage) => storage,
        Err(err) => {
            warn!(error = %err, "l2 tx broadcaster failed to open read-only storage");
            return;
        }
    };
    let mut next_offset = inner.head_offset.load(Ordering::Acquire);
    let mut metrics = BroadcasterMetrics::new(
        metrics_enabled,
        metrics_log_interval,
        inner.page_size,
        inner.subscriber_buffer_capacity,
        idle_poll_interval,
    );

    while !inner.stop_requested.load(Ordering::Relaxed) {
        metrics.on_loop_start();
        let read_started = metrics.phase_started_at();
        let txs = match storage.load_ordered_l2_txs_page_from(next_offset, inner.page_size) {
            Ok(value) => value,
            Err(err) => {
                metrics.on_read_error(read_started);
                warn!(
                    error = %err,
                    offset = next_offset,
                    "l2 tx broadcaster failed to read ordered tx page"
                );
                let sleep_started = metrics.phase_started_at();
                std::thread::sleep(idle_poll_interval);
                metrics.on_idle_sleep_end(sleep_started);
                metrics.maybe_log_window();
                continue;
            }
        };
        metrics.on_read_end(read_started, txs.len() as u64);

        if txs.is_empty() {
            metrics.on_empty_poll();
            let sleep_started = metrics.phase_started_at();
            std::thread::sleep(idle_poll_interval);
            metrics.on_idle_sleep_end(sleep_started);
            metrics.maybe_log_window();
            continue;
        }

        for tx in txs {
            let event = BroadcastTxMessage::from_offset_and_tx(next_offset, tx);
            next_offset = next_offset.saturating_add(1);
            inner.head_offset.store(next_offset, Ordering::Release);
            let fanout_started = metrics.phase_started_at();
            let outcome = fanout_event(Arc::as_ref(&inner), event);
            metrics.on_fanout_end(fanout_started, outcome);
        }
        metrics.maybe_log_window();
    }
    metrics.log_final();
}

fn fanout_event(inner: &L2TxBroadcasterInner, event: BroadcastTxMessage) -> FanoutOutcome {
    let mut to_remove = Vec::new();
    let mut subscribers = inner
        .subscribers
        .lock()
        .expect("l2 tx broadcaster subscribers mutex poisoned");
    let subscriber_count_before = subscribers.len();
    let mut dropped_closed = 0_u64;
    let mut dropped_full = 0_u64;
    let mut delivered = 0_u64;

    for (subscriber_id, sender) in subscribers.iter() {
        match sender.try_send(event.clone()) {
            Ok(()) => delivered = delivered.saturating_add(1),
            Err(TrySendError::Closed(_)) => {
                to_remove.push(*subscriber_id);
                dropped_closed = dropped_closed.saturating_add(1);
                warn!(subscriber_id, "l2 tx broadcaster removed closed subscriber");
            }
            Err(TrySendError::Full(_)) => {
                to_remove.push(*subscriber_id);
                dropped_full = dropped_full.saturating_add(1);
                warn!(
                    subscriber_id,
                    "l2 tx broadcaster dropped slow subscriber due to full channel"
                );
            }
        }
    }

    for subscriber_id in to_remove {
        subscribers.remove(&subscriber_id);
    }

    FanoutOutcome {
        delivered,
        dropped_closed,
        dropped_full,
        subscriber_count_before: subscriber_count_before as u64,
        subscriber_count_after: subscribers.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::BroadcastTxMessage;
    use super::{L2TxBroadcaster, L2TxBroadcasterInner};
    use crate::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};
    use alloy_primitives::Address;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn broadcast_user_op_serializes_with_hex_data() {
        let msg = BroadcastTxMessage::from_offset_and_tx(
            7,
            SequencedL2Tx::UserOp(ValidUserOp {
                sender: Address::from_slice(&[0x11; 20]),
                fee: 3,
                data: vec![0xaa, 0xbb],
            }),
        );
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"kind\":\"user_op\""));
        assert!(json.contains("\"offset\":7"));
        assert!(json.contains("\"fee\":3"));
        assert!(json.contains("\"data\":\"0xaabb\""));
    }

    #[test]
    fn broadcast_direct_input_serializes_with_hex_payload() {
        let msg = BroadcastTxMessage::from_offset_and_tx(
            9,
            SequencedL2Tx::Direct(DirectInput {
                payload: vec![0xcc, 0xdd],
            }),
        );
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"kind\":\"direct_input\""));
        assert!(json.contains("\"offset\":9"));
        assert!(json.contains("\"payload\":\"0xccdd\""));
    }

    #[test]
    fn subscribe_observes_live_start_after_registering_subscriber() {
        let broadcaster = L2TxBroadcaster {
            inner: Arc::new(L2TxBroadcasterInner {
                db_path: ":memory:".to_string(),
                page_size: 1,
                subscriber_buffer_capacity: 1,
                head_offset: AtomicU64::new(0),
                next_subscriber_id: AtomicU64::new(0),
                stop_requested: AtomicBool::new(false),
                subscribers: Mutex::new(HashMap::new()),
            }),
        };

        for _ in 0..16 {
            broadcaster
                .inner
                .head_offset
                .store(0, std::sync::atomic::Ordering::Release);
            broadcaster
                .inner
                .subscribers
                .lock()
                .expect("subscribers mutex")
                .clear();

            let guard = broadcaster
                .inner
                .subscribers
                .lock()
                .expect("subscribers mutex");
            let (tx, rx) = std::sync::mpsc::channel();
            let cloned = broadcaster.clone();
            let join = std::thread::spawn(move || {
                let subscription = cloned.subscribe();
                tx.send(subscription.live_start_offset)
                    .expect("send live start offset");
            });

            std::thread::sleep(Duration::from_millis(2));
            broadcaster
                .inner
                .head_offset
                .store(1, std::sync::atomic::Ordering::Release);
            drop(guard);

            let observed = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("recv live start offset");
            join.join().expect("join subscribe thread");

            assert_eq!(
                observed, 1,
                "subscriber must observe current head after it is visible in subscriber set"
            );
        }
    }
}
