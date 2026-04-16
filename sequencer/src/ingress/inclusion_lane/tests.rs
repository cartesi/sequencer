// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, Signature, U256};
use app_core::application::MAX_METHOD_PAYLOAD_BYTES as WALLET_MAX_METHOD_PAYLOAD_BYTES;
use rusqlite::params;
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};

use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{SafeInputRange, Storage, StoredSafeInput, WriteHead};
use sequencer_core::application::{AppError, AppOutputs, Application, InvalidReason};
use sequencer_core::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};
use sequencer_core::user_op::{SignedUserOp, UserOp};

use super::catch_up::catch_up_application_paged;
use super::dequeue_and_execute_user_op_chunk;
use super::error::CatchUpError;
use super::{InclusionLane, InclusionLaneConfig, InclusionLaneError, PendingUserOp};

#[derive(Default)]
struct TestApp {
    nonces: HashMap<Address, u32>,
    executed_input_count: u64,
}

impl Application for TestApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

    fn current_user_nonce(&self, sender: Address) -> u32 {
        self.nonces.get(&sender).copied().unwrap_or(0)
    }

    fn current_user_balance(&self, _sender: Address) -> U256 {
        U256::MAX
    }

    fn validate_user_op(
        &self,
        _sender: Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }

    fn execute_valid_user_op(&mut self, user_op: &ValidUserOp) -> Result<AppOutputs, AppError> {
        let next_nonce = self.current_user_nonce(user_op.sender).wrapping_add(1);
        self.nonces.insert(user_op.sender, next_nonce);
        self.executed_input_count = self.executed_input_count.saturating_add(1);
        Ok(Vec::new())
    }

    fn execute_direct_input(&mut self, _input: &DirectInput) -> Result<AppOutputs, AppError> {
        self.executed_input_count = self.executed_input_count.saturating_add(1);
        Ok(Vec::new())
    }

    fn executed_input_count(&self) -> u64 {
        self.executed_input_count
    }
}

struct TestDb {
    _dir: TempDir,
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplayEvent {
    UserOp {
        sender: Address,
        data: Vec<u8>,
    },
    DirectInput {
        sender: Address,
        block_number: u64,
        payload: Vec<u8>,
    },
}

struct ReplayRecordingApp {
    executed_input_count: u64,
    replayed: Vec<ReplayEvent>,
}

struct SharedCountingApp {
    executed_direct_inputs: Arc<AtomicU64>,
}

impl Application for SharedCountingApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

    fn current_user_nonce(&self, _sender: Address) -> u32 {
        0
    }

    fn current_user_balance(&self, _sender: Address) -> U256 {
        U256::MAX
    }

    fn validate_user_op(
        &self,
        _sender: Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }

    fn execute_valid_user_op(&mut self, _user_op: &ValidUserOp) -> Result<AppOutputs, AppError> {
        Ok(Vec::new())
    }

    fn execute_direct_input(&mut self, _input: &DirectInput) -> Result<AppOutputs, AppError> {
        self.executed_direct_inputs.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

impl ReplayRecordingApp {
    fn with_executed_input_count(executed_input_count: u64) -> Self {
        Self {
            executed_input_count,
            replayed: Vec::new(),
        }
    }
}

impl Default for ReplayRecordingApp {
    fn default() -> Self {
        Self::with_executed_input_count(0)
    }
}

impl Application for ReplayRecordingApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

    fn current_user_nonce(&self, _sender: Address) -> u32 {
        0
    }

    fn current_user_balance(&self, _sender: Address) -> U256 {
        U256::MAX
    }

    fn validate_user_op(
        &self,
        _sender: Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }

    fn execute_valid_user_op(&mut self, user_op: &ValidUserOp) -> Result<AppOutputs, AppError> {
        self.replayed.push(ReplayEvent::UserOp {
            sender: user_op.sender,
            data: user_op.data.clone(),
        });
        self.executed_input_count = self.executed_input_count.saturating_add(1);
        Ok(Vec::new())
    }

    fn execute_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
        self.replayed.push(ReplayEvent::DirectInput {
            sender: input.sender,
            block_number: input.block_number,
            payload: input.payload.clone(),
        });
        self.executed_input_count = self.executed_input_count.saturating_add(1);
        Ok(Vec::new())
    }

    fn executed_input_count(&self) -> u64 {
        self.executed_input_count
    }
}

fn temp_db(name: &str) -> TestDb {
    let dir = tempfile::Builder::new()
        .prefix(format!("sequencer-inclusion-lane-{name}-").as_str())
        .tempdir()
        .expect("create temporary test directory");
    let path = dir.path().join("sequencer.sqlite");
    TestDb {
        _dir: dir,
        path: path.to_string_lossy().into_owned(),
    }
}

fn default_test_config() -> InclusionLaneConfig {
    InclusionLaneConfig {
        batch_submitter_address: Address::from_slice(&[0xff; 20]),
        max_user_ops_per_chunk: 16,
        safe_input_buffer_capacity: 16,
        max_batch_open: Duration::MAX,
        idle_poll_interval: Duration::from_millis(2),
        // Tests should observe frontier changes immediately rather than wait
        // for the production gate.
        frontier_min_interval: Duration::ZERO,
    }
}

async fn start_lane(
    db_path: &str,
    config: InclusionLaneConfig,
) -> (
    mpsc::Sender<PendingUserOp>,
    ShutdownSignal,
    tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
) {
    let storage = Storage::open(db_path, "NORMAL").expect("open storage");
    let shutdown = ShutdownSignal::default();
    let (tx, handle) =
        InclusionLane::start(128, shutdown.clone(), TestApp::default(), storage, config);
    let initialized = wait_until(Duration::from_secs(2), || {
        let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
        storage
            .load_open_state()
            .expect("load open state")
            .is_some()
    })
    .await;
    assert!(initialized, "lane should initialize its first open state");
    (tx, shutdown, handle)
}

fn make_pending_user_op(
    seed: u8,
) -> (
    PendingUserOp,
    oneshot::Receiver<Result<(), super::SequencerError>>,
) {
    let sender = Address::from_slice(&[seed; 20]);
    let (respond_to, recv) = oneshot::channel();
    let user_op = UserOp {
        nonce: 0,
        // Must be >= the DB default recommended_fee (1060) to pass the
        // protocol-level max_fee >= fee_price check in the trait default.
        max_fee: u16::MAX,
        data: vec![seed; 4].into(),
    };
    (
        PendingUserOp {
            signed: SignedUserOp {
                sender,
                signature: Signature::test_signature(),
                user_op,
            },
            respond_to,
            received_at: SystemTime::now(),
        },
        recv,
    )
}

fn seed_replay_fixture(db_path: &str) -> Vec<ReplayEvent> {
    let mut storage = Storage::open(db_path, "NORMAL").expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");

    let user_op_a = make_pending_user_op(0x51).0;
    let user_op_b = make_pending_user_op(0x52).0;
    storage
        .append_user_ops_chunk(&mut head, &[user_op_a, user_op_b])
        .expect("append first frame user ops");
    storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }],
        )
        .expect("append first direct input");
    storage
        .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
        .expect("close first frame");

    let user_op_c = make_pending_user_op(0x53).0;
    storage
        .append_user_ops_chunk(&mut head, &[user_op_c])
        .expect("append second frame user op");
    storage
        .append_safe_inputs(
            20,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 20,
            }],
        )
        .expect("append second direct input");
    storage
        .close_frame_only(&mut head, 20, SafeInputRange::new(1, 2))
        .expect("close second frame");

    storage
        .append_safe_inputs(
            30,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xcc],
                block_number: 30,
            }],
        )
        .expect("append third direct input");
    storage
        .close_frame_only(&mut head, 30, SafeInputRange::new(2, 3))
        .expect("close third frame");

    vec![
        ReplayEvent::UserOp {
            sender: Address::from_slice(&[0x51; 20]),
            data: vec![0x51; 4],
        },
        ReplayEvent::UserOp {
            sender: Address::from_slice(&[0x52; 20]),
            data: vec![0x52; 4],
        },
        ReplayEvent::DirectInput {
            sender: Address::ZERO,
            block_number: 10,
            payload: vec![0xaa],
        },
        ReplayEvent::UserOp {
            sender: Address::from_slice(&[0x53; 20]),
            data: vec![0x53; 4],
        },
        ReplayEvent::DirectInput {
            sender: Address::ZERO,
            block_number: 20,
            payload: vec![0xbb],
        },
        ReplayEvent::DirectInput {
            sender: Address::ZERO,
            block_number: 30,
            payload: vec![0xcc],
        },
    ]
}

fn read_count(db_path: &str, table: &str) -> i64 {
    let conn = Storage::open_connection(db_path, "NORMAL").expect("open sqlite reader");
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(sql.as_str(), [], |row| row.get(0))
        .expect("count rows")
}

fn read_frame_direct_count(db_path: &str, batch_index: i64, frame_in_batch: i64) -> i64 {
    let conn = Storage::open_connection(db_path, "NORMAL").expect("open sqlite reader");
    conn.query_row(
        "SELECT COUNT(*) FROM sequenced_l2_txs
         WHERE batch_index = ?1
           AND frame_in_batch = ?2
           AND safe_input_index IS NOT NULL",
        params![batch_index, frame_in_batch],
        |row| row.get(0),
    )
    .expect("query frame direct count")
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let started = tokio::time::Instant::now();
    while started.elapsed() < timeout {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    predicate()
}

async fn shutdown_lane(
    shutdown: &ShutdownSignal,
    handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
) {
    shutdown.request_shutdown();
    let joined = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("wait for lane shutdown");
    let result = joined.expect("join lane task");
    assert!(result.is_ok(), "lane should shut down cleanly: {result:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ack_happens_after_chunk_commit_without_closing_frame() {
    let db = temp_db("ack-chunk-commit");
    let (tx, shutdown, lane_handle) = start_lane(db.path.as_str(), default_test_config()).await;
    let (pending, recv) = make_pending_user_op(0x11);

    tx.send(pending).await.expect("send user op");
    let ack = tokio::time::timeout(Duration::from_secs(2), recv)
        .await
        .expect("wait for ack")
        .expect("ack channel open");
    let user_ops_count = read_count(db.path.as_str(), "user_ops");
    let frame0_direct_count = read_frame_direct_count(db.path.as_str(), 0, 0);
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(ack.is_ok(), "user op should be included");
    assert_eq!(user_ops_count, 1);
    assert_eq!(
        frame0_direct_count, 0,
        "frame should stay open when no directs and no batch close"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_inputs_close_frame_and_persist_drain() {
    let db = temp_db("directs-close-frame");
    let (_tx, shutdown, lane_handle) = start_lane(db.path.as_str(), default_test_config()).await;
    let mut feeder_storage =
        Storage::open(db.path.as_str(), "NORMAL").expect("open feeder storage");

    feeder_storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }],
        )
        .expect("append safe direct input");

    let drained = wait_until(Duration::from_secs(2), || {
        read_frame_direct_count(db.path.as_str(), 0, 1) == 1
    })
    .await;
    let frames_count = read_count(db.path.as_str(), "frames");
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(drained, "expected one drained direct input in frame 1");
    assert_eq!(frames_count, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequenced_safe_inputs_are_drained_but_not_executed() {
    let db = temp_db("sequenced-safe-inputs-skip");
    let batch_submitter_address = Address::from([0xfe; 20]);
    let executed_direct_inputs = Arc::new(AtomicU64::new(0));
    let storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
    let shutdown = ShutdownSignal::default();
    let (tx, lane_handle) = InclusionLane::start(
        128,
        shutdown.clone(),
        SharedCountingApp {
            executed_direct_inputs: executed_direct_inputs.clone(),
        },
        storage,
        InclusionLaneConfig {
            batch_submitter_address,
            ..default_test_config()
        },
    );
    let initialized = wait_until(Duration::from_secs(2), || {
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        storage
            .load_open_state()
            .expect("load open state")
            .is_some()
    })
    .await;
    assert!(initialized, "lane should initialize open state");

    let mut feeder_storage =
        Storage::open(db.path.as_str(), "NORMAL").expect("open feeder storage");
    feeder_storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: batch_submitter_address,
                payload: vec![0xaa],
                block_number: 10,
            }],
        )
        .expect("append safe batch-submitter input");

    let drained = wait_until(Duration::from_secs(2), || {
        read_frame_direct_count(db.path.as_str(), 0, 1) == 1
    })
    .await;
    drop(tx);
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(
        drained,
        "expected sequenced safe input to be drained into frame 1"
    );
    assert_eq!(
        executed_direct_inputs.load(Ordering::SeqCst),
        0,
        "batch-submitter safe input should be skipped by the local app"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_inputs_are_paginated_by_buffer_capacity() {
    let db = temp_db("directs-pagination");
    let mut config = default_test_config();
    config.safe_input_buffer_capacity = 2;
    let (_tx, shutdown, lane_handle) = start_lane(db.path.as_str(), config).await;
    let mut feeder_storage =
        Storage::open(db.path.as_str(), "NORMAL").expect("open feeder storage");

    let mut directs = Vec::new();
    for index in 0..5_u64 {
        directs.push(StoredSafeInput {
            sender: Address::ZERO,
            payload: vec![0x10 + index as u8],
            block_number: 10,
        });
    }
    feeder_storage
        .append_safe_inputs(10, directs.as_slice())
        .expect("append safe direct inputs");

    let drained = wait_until(Duration::from_secs(2), || {
        read_frame_direct_count(db.path.as_str(), 0, 1) == 5
    })
    .await;
    let frames_count = read_count(db.path.as_str(), "frames");
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(drained, "expected five drained direct inputs in frame 1");
    assert_eq!(frames_count, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_inputs_already_available_are_sequenced_before_later_user_ops() {
    let db = temp_db("directs-before-later-userops");
    let (tx, shutdown, lane_handle) = start_lane(db.path.as_str(), default_test_config()).await;
    let mut feeder_storage =
        Storage::open(db.path.as_str(), "NORMAL").expect("open feeder storage");

    feeder_storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }],
        )
        .expect("append safe direct input");

    let drained = wait_until(Duration::from_secs(2), || {
        read_frame_direct_count(db.path.as_str(), 0, 1) == 1
    })
    .await;
    assert!(
        drained,
        "expected leading direct inputs to land before later user-op sequencing"
    );

    let (pending, recv) = make_pending_user_op(0x31);
    tx.send(pending).await.expect("send user op");
    let ack = tokio::time::timeout(Duration::from_secs(2), recv)
        .await
        .expect("wait for ack")
        .expect("ack channel open");

    let replay: Vec<SequencedL2Tx> = {
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        storage
            .load_ordered_l2_txs_page_from(0, 1_000_000)
            .expect("load ordered replay")
            .into_iter()
            .map(|(_offset, tx)| tx)
            .collect()
    };
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(ack.is_ok(), "user op should be included");
    assert_eq!(replay.len(), 2);
    assert!(matches!(
        replay.first(),
        Some(SequencedL2Tx::Direct(direct)) if direct.payload.as_slice() == [0xaa]
    ));
    assert!(matches!(
        replay.get(1),
        Some(SequencedL2Tx::UserOp(user_op)) if user_op.data.as_slice() == [0x31, 0x31, 0x31, 0x31]
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_closes_when_max_open_time_is_reached() {
    let db = temp_db("batch-close-time");
    let mut config = default_test_config();
    config.max_batch_open = Duration::from_millis(20);
    let (tx, shutdown, lane_handle) = start_lane(db.path.as_str(), config).await;
    let (pending, recv) = make_pending_user_op(0x22);

    tx.send(pending).await.expect("send user op");
    let ack = tokio::time::timeout(Duration::from_secs(2), recv)
        .await
        .expect("wait for ack")
        .expect("ack channel open");
    let rotated = wait_until(Duration::from_secs(2), || {
        read_count(db.path.as_str(), "batches") >= 2
    })
    .await;
    let drain = read_frame_direct_count(db.path.as_str(), 0, 0);
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(ack.is_ok(), "user op should be included");
    assert!(rotated, "expected batch rotation by time");
    assert_eq!(drain, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_batches_close_when_max_open_time_is_reached() {
    let db = temp_db("empty-batch-close-time");
    let mut config = default_test_config();
    config.max_batch_open = Duration::from_millis(20);
    let (_tx, shutdown, lane_handle) = start_lane(db.path.as_str(), config).await;

    let rotated = wait_until(Duration::from_secs(2), || {
        read_count(db.path.as_str(), "batches") >= 2
    })
    .await;
    let frames_count = read_count(db.path.as_str(), "frames");
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(rotated, "expected idle batch rotation by time");
    assert!(
        frames_count >= 2,
        "expected at least one new open frame after rotation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_closes_when_max_user_op_bytes_is_reached() {
    let db = temp_db("batch-close-size");
    // Set alpha high enough that batch_size_target ≤ one user op (126 bytes).
    // 55000*1000/(17000*26) = 124 bytes < 126.
    {
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        storage.set_alpha(17000, 1000).expect("set alpha");
    }
    let config = default_test_config();
    let (tx, shutdown, lane_handle) = start_lane(db.path.as_str(), config).await;
    let (pending, recv) = make_pending_user_op(0x33);

    tx.send(pending).await.expect("send user op");
    let ack = tokio::time::timeout(Duration::from_secs(2), recv)
        .await
        .expect("wait for ack")
        .expect("ack channel open");
    let rotated = wait_until(Duration::from_secs(2), || {
        read_count(db.path.as_str(), "batches") >= 2
    })
    .await;
    let drain = read_frame_direct_count(db.path.as_str(), 0, 0);
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(ack.is_ok(), "user op should be included");
    assert!(rotated, "expected batch rotation by size");
    assert_eq!(drain, 0);
}

/// Test fixture: a `WriteHead` whose size budget is unbounded, so the early-stop
/// in `dequeue_and_execute_user_op_chunk` never triggers from the size check
/// alone. Tests that want to exercise the size check construct their own.
fn unbounded_head() -> WriteHead {
    WriteHead {
        batch_index: 0,
        batch_created_at: SystemTime::now(),
        frame_fee: 0,
        safe_block: 0,
        batch_user_op_count: 0,
        open_frame_user_op_count: 0,
        frame_in_batch: 0,
        max_batch_user_op_bytes: u64::MAX,
    }
}

#[test]
fn dequeue_returns_channel_closed_when_disconnected() {
    let (tx, mut rx) = mpsc::channel::<PendingUserOp>(1);
    drop(tx);
    let mut app = TestApp::default();
    let mut included = Vec::new();
    let head = unbounded_head();

    let err = dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 1, 1, &head, &mut included)
        .unwrap_err();
    assert!(matches!(err, InclusionLaneError::ChannelClosed));
}

#[test]
fn dequeue_flushes_executed_ops_before_observing_disconnect() {
    let (tx, mut rx) = mpsc::channel::<PendingUserOp>(2);
    let (pending, _recv) = make_pending_user_op(0x44);
    tx.blocking_send(pending).expect("enqueue pending user op");
    drop(tx);

    let mut app = TestApp::default();
    let mut included = Vec::new();
    let head = unbounded_head();
    dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 1, 16, &head, &mut included)
        .expect("should flush processed user ops before disconnect");
    assert_eq!(included.len(), 1);
}

#[test]
fn catch_up_replays_multiple_pages() {
    let db = temp_db("catch-up-multi-page");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
    let mut app = ReplayRecordingApp::default();

    catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 2)
        .expect("catch up in pages");

    assert_eq!(app.replayed, expected);
    assert_eq!(app.executed_input_count(), expected.len() as u64);
}

#[test]
fn catch_up_replays_from_storage_even_when_app_reports_executed_inputs() {
    let db = temp_db("catch-up-offset");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
    let mut app = ReplayRecordingApp::with_executed_input_count(3);

    catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 2)
        .expect("catch up from storage");

    assert_eq!(app.replayed, expected);
    assert_eq!(app.executed_input_count(), 3 + expected.len() as u64);
}

#[test]
fn catch_up_handles_mixed_user_ops_and_direct_inputs_across_page_boundary() {
    let db = temp_db("catch-up-mixed-page-boundary");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
    let mut app = ReplayRecordingApp::default();

    catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 4)
        .expect("catch up across page boundary");

    assert_eq!(app.replayed, expected);
}

#[test]
fn catch_up_load_error_reports_offset() {
    let db = temp_db("catch-up-load-error");
    let mut storage =
        Storage::open_without_migrations(db.path.as_str(), "NORMAL").expect("open raw storage");
    let mut app = ReplayRecordingApp::default();

    let err = catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 2)
        .expect_err("catch up should fail without schema");

    assert!(matches!(err, CatchUpError::LoadReplay { offset: 0, .. }));
}
