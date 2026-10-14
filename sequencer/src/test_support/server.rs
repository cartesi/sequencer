// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! An in-process API server over a test database: [`TestServer`] and the
//! two-listener [`bind_api_listeners`] helper.

use std::net::SocketAddr;
use std::time::Duration;

use alloy_primitives::{Address, Signature};
use alloy_sol_types::Eip712Domain;
use app_core::application::{MAX_METHOD_PAYLOAD_BYTES, WalletApp, WalletConfig};
use sequencer_core::application::Application;
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::user_op::{SignedUserOp, UserOp};
use tokio::sync::mpsc;

use crate::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use crate::http::{self, ApiConfig};
use crate::ingress::inclusion_lane::{
    InclusionLane, InclusionLaneConfig, InclusionLaneError, PendingUserOp, dump_info,
};
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::Storage;

/// Two ephemeral loopback listeners; returns (listeners, public, internal).
pub(crate) async fn bind_api_listeners() -> (
    crate::http::ApiListeners,
    std::net::SocketAddr,
    std::net::SocketAddr,
) {
    let public = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind public listener");
    let internal = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind internal listener");
    let (public_addr, internal_addr) = (
        public.local_addr().expect("public addr"),
        internal.local_addr().expect("internal addr"),
    );
    (
        crate::http::ApiListeners { public, internal },
        public_addr,
        internal_addr,
    )
}

/// The public routes, per README#api; every other route is internal.
pub(crate) const PUBLIC_ROUTES: [&str; 4] = ["/tx", "/fee", "/nonce", "/domain"];

enum QueueMode {
    /// Nothing drains `/tx`; the queue only has to exist.
    Idle,
    /// A `WalletApp` inclusion lane drains the queue.
    Lane,
    /// A capacity-one queue that already holds a parked op and is never
    /// drained, so every `/tx` finds it full.
    Full,
}

pub(crate) struct TestServerBuilder {
    db_path: String,
    domain: Eip712Domain,
    queue: QueueMode,
    max_body_bytes: Option<usize>,
}

impl TestServerBuilder {
    pub(crate) fn domain(mut self, domain: Eip712Domain) -> Self {
        self.domain = domain;
        self
    }

    pub(crate) fn with_lane(mut self) -> Self {
        self.queue = QueueMode::Lane;
        self
    }

    pub(crate) fn full_queue(mut self) -> Self {
        self.queue = QueueMode::Full;
        self
    }

    pub(crate) fn max_body(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = Some(max_body_bytes);
        self
    }

    pub(crate) async fn start(self) -> TestServer {
        let (listeners, public_addr, internal_addr) = bind_api_listeners().await;
        let shutdown = RuntimeScope::default();
        let db_path = self.db_path.as_str();
        let (tx, lane_handle, queue_rx) = match self.queue {
            QueueMode::Idle => {
                let (tx, rx) = mpsc::channel::<PendingUserOp>(1);
                (tx, None, Some(rx))
            }
            QueueMode::Lane => {
                let (tx, handle) = start_wallet_lane(db_path, &shutdown);
                (tx, Some(handle), None)
            }
            QueueMode::Full => {
                let _storage = Storage::open(db_path).expect("open storage");
                let (tx, rx) = mpsc::channel::<PendingUserOp>(1);
                let (respond_to, _) = tokio::sync::oneshot::channel();
                tx.try_send(PendingUserOp {
                    signed: SignedUserOp {
                        sender: Address::ZERO,
                        signature: Signature::test_signature(),
                        user_op: UserOp {
                            nonce: 0,
                            max_fee: 0,
                            data: Vec::new().into(),
                        },
                    },
                    respond_to,
                    received_at: std::time::SystemTime::now(),
                })
                .expect("park one op in the lane queue");
                (tx, None, Some(rx))
            }
        };
        let tx_feed = L2TxFeed::new(
            db_path.to_string(),
            shutdown.clone(),
            L2TxFeedConfig {
                idle_poll_interval: Duration::from_millis(2),
                page_size: 64,
            },
        );
        let mut config = ApiConfig::new(self.domain, MAX_METHOD_PAYLOAD_BYTES);
        if let Some(max_body_bytes) = self.max_body_bytes {
            config.max_body_bytes = max_body_bytes;
        }
        let server_task = http::start_on_listeners(
            listeners,
            tx,
            shutdown.clone(),
            tx_feed,
            config,
            http::SnapshotState {
                db_path: db_path.to_string(),
                state_file_in_dump: |dump_dir| {
                    WalletApp::state_file_in_dump(&dump_info::app_prefix(dump_dir))
                },
            },
        );
        TestServer {
            public_addr,
            internal_addr,
            lane_handle,
            shutdown,
            server_task: Some(server_task),
            _queue_rx: queue_rx,
        }
    }
}

/// Start a `WalletApp` lane over `db_path`, registering a genesis dump first
/// when the database has no finalized snapshot.
fn start_wallet_lane(
    db_path: &str,
    shutdown: &RuntimeScope,
) -> (
    mpsc::Sender<PendingUserOp>,
    tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
) {
    let mut storage = Storage::open(db_path).expect("open storage");
    let dumps_dir = tempfile::tempdir().expect("test dumps dir").keep();
    // Always-load invariant: ensure a finalized snapshot exists
    // before the lane starts. On warm restart there's already one;
    // on cold start we register a genesis dump from a fresh wallet.
    // The lane reloads via `from_dump` either way.
    if storage.finalized_dump().expect("read finalized").is_none() {
        let mut app = WalletApp::new(WalletConfig::default());
        let genesis_dir = dumps_dir.join("genesis");
        dump_info::create_dump_dir_with_info(
            &mut app,
            &genesis_dir,
            &dump_info::DumpInfo {
                format_version: dump_info::FORMAT_VERSION,
                next_batch_nonce: 0,
            },
        )
        .expect("genesis dump");
        storage
            .insert_baseline_snapshot(&genesis_dir, ExecutedInputCount::ZERO)
            .expect("register genesis");
    }

    // Tip-existence invariant: open the genesis Tip only after the genesis
    // snapshot exists. Opening the Tip may attribute already-ingested direct
    // inputs, so doing this first would give the fresh application dump the
    // wrong executed-input count.
    storage.ensure_open_tip().expect("establish open tip");
    let head = storage
        .open_state()
        .expect("load open state")
        .expect("open state exists");
    // Default log_gas_price=0 -> 0+296+20+419+621 = 1356.
    assert_eq!(head.frame_fee, 1356);

    InclusionLane::<WalletApp>::start(
        128,
        shutdown.clone(),
        storage,
        InclusionLaneConfig {
            dumps_dir,
            max_user_ops_per_chunk: 32,
            safe_input_buffer_capacity: 32,
            max_batch_open: Duration::from_secs(60 * 60),
            idle_poll_interval: Duration::from_millis(2),
            frontier_min_interval: Duration::ZERO,
        },
    )
}

/// The API on two loopback listeners over a test database. Dropping it
/// requests shutdown and aborts its tasks; [`Self::shutdown`] instead waits
/// for both to stop cleanly.
pub(crate) struct TestServer {
    pub(crate) public_addr: SocketAddr,
    pub(crate) internal_addr: SocketAddr,
    pub(crate) lane_handle: Option<tokio::task::JoinHandle<Result<(), InclusionLaneError>>>,
    shutdown: RuntimeScope,
    server_task: Option<http::ApiServerTask>,
    _queue_rx: Option<mpsc::Receiver<PendingUserOp>>,
}

impl TestServer {
    pub(crate) fn builder(db_path: &str) -> TestServerBuilder {
        TestServerBuilder {
            db_path: db_path.to_string(),
            domain: sequencer_core::build_input_domain(1, Address::ZERO),
            queue: QueueMode::Idle,
            max_body_bytes: None,
        }
    }

    pub(crate) async fn start(db_path: &str) -> Self {
        Self::builder(db_path).start().await
    }

    /// A URL on the listener that serves `path`.
    pub(crate) fn url(&self, path: &str) -> String {
        let route = path.split('?').next().unwrap_or(path);
        let addr = if PUBLIC_ROUTES.contains(&route) {
            self.public_addr
        } else {
            self.internal_addr
        };
        format!("http://{addr}{path}")
    }

    pub(crate) async fn shutdown(mut self) {
        self.shutdown.request_shutdown();
        if let Some(task) = self.lane_handle.take() {
            let lane_result = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .expect("wait for inclusion lane")
                .expect("join inclusion lane task");
            // The router may close the lane's queue before the lane sees the
            // shutdown request; the lane still stops cleanly.
            assert!(
                lane_result.is_ok(),
                "expected clean lane shutdown, got {lane_result:?}"
            );
        }
        if let Some(task) = self.server_task.take() {
            let server_result = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .expect("wait for server task")
                .expect("join server task");
            assert!(
                server_result.is_ok(),
                "expected clean server shutdown, got {server_result:?}"
            );
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.request_shutdown();
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
        if let Some(task) = self.lane_handle.take() {
            task.abort();
        }
    }
}
