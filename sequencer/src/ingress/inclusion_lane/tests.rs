// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, Signature};
use app_core::application::MAX_METHOD_PAYLOAD_BYTES as WALLET_MAX_METHOD_PAYLOAD_BYTES;
use rusqlite::params;
use tokio::sync::{mpsc, oneshot};

use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::test_helpers::{SENDER_A, default_protocol_timing, temp_db};
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

    fn validate_user_op(
        &self,
        _sender: Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }

    fn execute_valid_user_op(
        &mut self,
        user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        let current = self.nonces.get(&user_op.sender).copied().unwrap_or(0);
        let next_nonce = current.wrapping_add(1);
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

    fn last_executed_safe_block(&self) -> u64 {
        0
    }

    // The lane loads its app via `from_dump` after the runtime
    // registers a genesis dump, so test stubs must reload to the
    // initial state (an empty `nonces` map). `create_dump` writes
    // an empty marker file — `from_dump` ignores the contents and
    // returns `Self::default()`.
    fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
        Ok(Self::default())
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        std::fs::write(Self::state_file_in_dump(prefix), b"")?;
        Ok(())
    }

    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        std::fs::remove_dir_all(prefix)?;
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
    }
}

struct InternalUserOpApp;

impl Application for InternalUserOpApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

    fn validate_user_op(
        &self,
        _sender: Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }

    fn execute_valid_user_op(
        &mut self,
        _user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        Err(AppError::Internal {
            reason: "app invariant failed".to_string(),
        })
    }

    fn execute_direct_input(&mut self, _input: &DirectInput) -> Result<AppOutputs, AppError> {
        unimplemented!("not used in these tests")
    }

    fn executed_input_count(&self) -> u64 {
        0
    }

    fn last_executed_safe_block(&self) -> u64 {
        0
    }

    fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
        Ok(InternalUserOpApp)
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        std::fs::write(Self::state_file_in_dump(prefix), b"")?;
        Ok(())
    }

    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        std::fs::remove_dir_all(prefix)?;
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
    }
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

/// App that counts direct-input executions and persists the count
/// through `create_dump` / `from_dump`. Tests verify the count by
/// reading the state file from a snapshot (rather than by sharing
/// an `Arc<AtomicU64>` across thread boundaries) — the lane loads
/// its own instance via `from_dump`, so external observation has to
/// go through the on-disk snapshot.
struct SharedCountingApp {
    executed_direct_inputs: u64,
}

impl SharedCountingApp {
    fn new() -> Self {
        Self {
            executed_direct_inputs: 0,
        }
    }
}

impl Application for SharedCountingApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

    fn validate_user_op(
        &self,
        _sender: Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }

    fn execute_valid_user_op(
        &mut self,
        _user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        Ok(Vec::new())
    }

    fn execute_direct_input(&mut self, _input: &DirectInput) -> Result<AppOutputs, AppError> {
        self.executed_direct_inputs = self.executed_direct_inputs.saturating_add(1);
        Ok(Vec::new())
    }

    fn executed_input_count(&self) -> u64 {
        self.executed_direct_inputs
    }

    fn last_executed_safe_block(&self) -> u64 {
        0
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let bytes = std::fs::read(Self::state_file_in_dump(prefix))?;
        let counter =
            u64::from_le_bytes(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| AppError::Internal {
                        reason: "SharedCountingApp dump must be exactly 8 bytes".to_string(),
                    })?,
            );
        Ok(Self {
            executed_direct_inputs: counter,
        })
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        std::fs::write(
            Self::state_file_in_dump(prefix),
            self.executed_direct_inputs.to_le_bytes(),
        )?;
        Ok(())
    }

    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        std::fs::remove_dir_all(prefix)?;
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
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

    fn validate_user_op(
        &self,
        _sender: Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }

    fn execute_valid_user_op(
        &mut self,
        user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
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

    fn last_executed_safe_block(&self) -> u64 {
        0
    }

    fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
        Ok(Self::default())
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        std::fs::write(Self::state_file_in_dump(prefix), b"")?;
        Ok(())
    }

    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        std::fs::remove_dir_all(prefix)?;
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
    }
}

fn default_test_config() -> InclusionLaneConfig {
    InclusionLaneConfig {
        batch_submitter_address: Address::from_slice(&[0xff; 20]),
        // A leaked tempdir per call: the lane unconditionally writes
        // dump artifacts there, and the test stubs' `create_dump`
        // creates the directory. Tempdir gets reaped by the OS.
        dumps_dir: tempfile::tempdir()
            .expect("create dumps_dir tempdir")
            .keep(),
        max_user_ops_per_chunk: 16,
        safe_input_buffer_capacity: 16,
        max_batch_open: Duration::MAX,
        idle_poll_interval: Duration::from_millis(2),
        // Tests should observe frontier changes immediately rather than wait
        // for the production gate.
        frontier_min_interval: Duration::ZERO,
    }
}

/// Register a genesis snapshot for `app` so the lane's always-load
/// invariant holds. Tests use this to satisfy the catch-up
/// precondition; production startup goes through the runtime's
/// `bootstrap_application` which does the same thing.
fn register_genesis_snapshot<A: Application>(app: &A, storage: &mut Storage, dumps_dir: &Path) {
    // Unique per call: the dumps_dir is a tempdir but tests may
    // register multiple snapshots within one test (e.g., catch-up
    // tests that re-seed storage), so reuse-free naming matters.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dump_dir = dumps_dir.join(format!("genesis-{counter}"));
    super::dump_info::create_dump_dir_with_info(
        app,
        &dump_dir,
        &super::dump_info::DumpInfo {
            format_version: super::dump_info::FORMAT_VERSION,
            next_batch_nonce: 0,
            l2_tx_index: 0,
            promoted_inclusion_block: Some(0),
        },
    )
    .expect("create genesis dump");
    storage
        .insert_finalized_dump(&dump_dir, 0, 0)
        .expect("insert finalized snapshot");
}

async fn start_lane(
    db_path: &str,
    config: InclusionLaneConfig,
) -> (
    mpsc::Sender<PendingUserOp>,
    ShutdownSignal,
    tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
) {
    let mut storage = Storage::open(db_path).expect("open storage");
    storage
        .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
        .expect("seed observed safe head");
    let app = TestApp::default();
    register_genesis_snapshot(&app, &mut storage, &config.dumps_dir);
    // `app` instance is dropped here; the lane reloads it from the
    // genesis dump on its background thread.
    drop(app);
    // Establish the tip structurally (as the runtime does at startup), then
    // hand its head to the lane — which now only loads, never initializes.
    storage.ensure_open_tip().expect("establish genesis tip");
    let shutdown = ShutdownSignal::default();
    let (tx, handle) = InclusionLane::<TestApp>::start(128, shutdown.clone(), storage, config);
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
        // Must be >= the DB default recommended_fee (1356) to pass the
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
    let mut storage = Storage::open(db_path).expect("open storage");
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
            SENDER_A,
            &default_protocol_timing(),
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
            SENDER_A,
            &default_protocol_timing(),
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
            SENDER_A,
            &default_protocol_timing(),
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
    let conn = Storage::open_connection(db_path).expect("open sqlite reader");
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(sql.as_str(), [], |row| row.get(0))
        .expect("count rows")
}

fn read_frame_direct_count(db_path: &str, batch_index: i64, frame_in_batch: i64) -> i64 {
    let conn = Storage::open_connection(db_path).expect("open sqlite reader");
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
    let mut feeder_storage = Storage::open(db.path.as_str()).expect("open feeder storage");

    feeder_storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }],
            SENDER_A,
            &default_protocol_timing(),
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
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
        .expect("seed observed safe head");
    let config = InclusionLaneConfig {
        batch_submitter_address,
        ..default_test_config()
    };
    {
        let app = SharedCountingApp::new();
        register_genesis_snapshot(&app, &mut storage, &config.dumps_dir);
    }
    storage.ensure_open_tip().expect("establish genesis tip");
    let shutdown = ShutdownSignal::default();
    let (_tx, lane_handle) =
        InclusionLane::<SharedCountingApp>::start(128, shutdown.clone(), storage, config);
    let initialized = wait_until(Duration::from_secs(2), || {
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage.open_state().expect("load open state").is_some()
    })
    .await;
    assert!(initialized, "lane should initialize open state");

    let mut feeder_storage = Storage::open(db.path.as_str()).expect("open feeder storage");
    feeder_storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: batch_submitter_address,
                payload: vec![0xaa],
                block_number: 10,
            }],
            SENDER_A,
            &default_protocol_timing(),
        )
        .expect("append safe batch-submitter input");

    let drained = wait_until(Duration::from_secs(2), || {
        read_frame_direct_count(db.path.as_str(), 0, 1) == 1
    })
    .await;
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(
        drained,
        "expected sequenced safe input to be drained into frame 1"
    );

    // The lane's own batch input was drained into a frame and
    // sequenced into `sequenced_l2_txs`, but the lane skipped
    // `app.execute_direct_input` for it. Catch-up replays the same
    // sequenced stream and also filters batch-submitter rows — so a
    // fresh `SharedCountingApp` driven through `catch_up_application`
    // ends with counter == 0, confirming the symmetric skip.
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut fresh_app = SharedCountingApp::new();
    catch_up_application_paged(&mut fresh_app, &mut storage, batch_submitter_address, 0, 16)
        .expect("catch up");
    assert_eq!(
        fresh_app.executed_direct_inputs, 0,
        "batch-submitter safe input should be skipped by the local app"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_inputs_are_paginated_by_buffer_capacity() {
    let db = temp_db("directs-pagination");
    let mut config = default_test_config();
    config.safe_input_buffer_capacity = 2;
    let (_tx, shutdown, lane_handle) = start_lane(db.path.as_str(), config).await;
    let mut feeder_storage = Storage::open(db.path.as_str()).expect("open feeder storage");

    let mut directs = Vec::new();
    for index in 0..5_u64 {
        directs.push(StoredSafeInput {
            sender: Address::ZERO,
            payload: vec![0x10 + index as u8],
            block_number: 10,
        });
    }
    feeder_storage
        .append_safe_inputs(10, directs.as_slice(), SENDER_A, &default_protocol_timing())
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
    let mut feeder_storage = Storage::open(db.path.as_str()).expect("open feeder storage");

    feeder_storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }],
            SENDER_A,
            &default_protocol_timing(),
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
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .ordered_l2_txs_page_from(0, 1_000_000)
            .expect("load ordered replay")
            .into_iter()
            .map(|(_offset, tx, _frame_safe_block)| tx)
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
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
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

    let err =
        dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 1, &head, &mut included).unwrap_err();
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
    dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 16, &head, &mut included)
        .expect("should flush processed user ops before disconnect");
    assert_eq!(included.len(), 1);
}

#[test]
fn dequeue_returns_lane_error_when_app_reports_internal() {
    let (tx, mut rx) = mpsc::channel::<PendingUserOp>(1);
    let (pending, recv) = make_pending_user_op(0x45);
    tx.blocking_send(pending).expect("enqueue pending user op");

    let mut app = InternalUserOpApp;
    let mut included = Vec::new();
    let head = unbounded_head();
    let err = dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 16, &head, &mut included)
        .expect_err("internal application error should stop the lane");

    assert!(matches!(err, InclusionLaneError::ExecuteUserOp { .. }));
    assert!(
        included.is_empty(),
        "internal errors must not leave an op ready to persist"
    );
    let response = recv
        .blocking_recv()
        .expect("lane should respond to triggering op")
        .expect_err("triggering op should receive internal error");
    assert!(matches!(
        response,
        super::SequencerError::Internal(message) if message == "app invariant failed"
    ));
}

#[test]
fn catch_up_replays_multiple_pages() {
    let db = temp_db("catch-up-multi-page");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut app = ReplayRecordingApp::default();

    catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 0, 2)
        .expect("catch up in pages");

    assert_eq!(app.replayed, expected);
    assert_eq!(app.executed_input_count(), expected.len() as u64);
}

#[test]
fn catch_up_replays_from_storage_even_when_app_reports_executed_inputs() {
    let db = temp_db("catch-up-offset");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut app = ReplayRecordingApp::with_executed_input_count(3);

    catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 0, 2)
        .expect("catch up from storage");

    assert_eq!(app.replayed, expected);
    assert_eq!(app.executed_input_count(), 3 + expected.len() as u64);
}

#[test]
fn catch_up_handles_mixed_user_ops_and_direct_inputs_across_page_boundary() {
    let db = temp_db("catch-up-mixed-page-boundary");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut app = ReplayRecordingApp::default();

    catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 0, 4)
        .expect("catch up across page boundary");

    assert_eq!(app.replayed, expected);
}

#[test]
fn catch_up_load_error_reports_offset() {
    let db = temp_db("catch-up-load-error");
    let mut storage = Storage::open_writer(db.path.as_str()).expect("open raw storage");
    let mut app = ReplayRecordingApp::default();

    let err = catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 0, 2)
        .expect_err("catch up should fail without schema");

    assert!(matches!(err, CatchUpError::LoadReplay { offset: 0, .. }));
}

/// App that counts executed user ops and persists the count through
/// `create_dump` / `from_dump` (8 LE bytes at `prefix/state`). The
/// restart-resume regression test reads this count back from the
/// snapshot the lane writes on its own thread — the only way to observe
/// how much state the lane rebuilt across a restart.
struct UserOpCounterApp {
    executed_user_ops: u64,
}

impl UserOpCounterApp {
    fn new() -> Self {
        Self {
            executed_user_ops: 0,
        }
    }
}

impl Application for UserOpCounterApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

    fn validate_user_op(
        &self,
        _sender: Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }

    fn execute_valid_user_op(
        &mut self,
        _user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        self.executed_user_ops = self.executed_user_ops.saturating_add(1);
        Ok(Vec::new())
    }

    fn execute_direct_input(&mut self, _input: &DirectInput) -> Result<AppOutputs, AppError> {
        unimplemented!("not used in these tests")
    }

    fn executed_input_count(&self) -> u64 {
        self.executed_user_ops
    }

    fn last_executed_safe_block(&self) -> u64 {
        0
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let bytes = std::fs::read(Self::state_file_in_dump(prefix))?;
        let count =
            u64::from_le_bytes(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| AppError::Internal {
                        reason: "UserOpCounterApp dump must be exactly 8 bytes".to_string(),
                    })?,
            );
        Ok(Self {
            executed_user_ops: count,
        })
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        std::fs::write(
            Self::state_file_in_dump(prefix),
            self.executed_user_ops.to_le_bytes(),
        )?;
        Ok(())
    }

    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        std::fs::remove_dir_all(prefix)?;
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
    }
}

fn read_dump_counter(dump_dir: &Path) -> u64 {
    let state_file = UserOpCounterApp::state_file_in_dump(&super::dump_info::app_prefix(dump_dir));
    let bytes = std::fs::read(state_file).expect("read dump state file");
    u64::from_le_bytes(
        bytes
            .as_slice()
            .try_into()
            .expect("dump state must be 8 bytes"),
    )
}

/// Regression for the resume-checkpoint bug: the lane used to load its
/// Application from one snapshot (the finalized) but replay catch-up
/// from a *different* snapshot's offset (the latest pending). With a
/// pending snapshot present at restart — the common "closed batch not
/// yet observed on L1" case — the loaded state and the replay cursor
/// must come from the *same* checkpoint, or restart silently drops the
/// pending batch's txs.
///
/// Setup: 3 user ops each close their own batch (aggressive sizing) →
/// 3 pending snapshots, none promoted, so finalized stays at genesis
/// (counter 0) while the latest pending reflects counter 3. After
/// restart, one more op must land the snapshot at 4. A result of 1
/// means the lane resumed from genesis and skipped the pending state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_resumes_from_pending_checkpoint_without_skipping_txs() {
    let db = temp_db("restart-resume-pending-checkpoint");

    {
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        // Batch size target < one user op, so every op closes its batch.
        storage.set_alpha(17000, 1000).expect("set alpha");
        storage
            .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
            .expect("seed observed safe head");
    }

    // Generation 1: no idle closes (Duration::MAX), so the latest pending is
    // always the last user-op batch with a correct global offset.
    let mut config1 = default_test_config();
    config1.max_batch_open = Duration::MAX;

    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    // `app` only writes the genesis dump here; the lane reloads its own
    // instance via `from_dump` on its background thread.
    let app = UserOpCounterApp::new();
    register_genesis_snapshot(&app, &mut storage, &config1.dumps_dir);
    storage.ensure_open_tip().expect("establish genesis tip");
    let shutdown1 = ShutdownSignal::default();
    let (tx1, handle1) =
        InclusionLane::<UserOpCounterApp>::start(128, shutdown1.clone(), storage, config1.clone());
    assert!(
        wait_until(Duration::from_secs(2), || {
            Storage::open(db.path.as_str())
                .expect("open")
                .open_state()
                .expect("open state")
                .is_some()
        })
        .await,
        "gen-1 lane should initialize"
    );

    for seed in 0..3u8 {
        let (pending, recv) = make_pending_user_op(0x10 + seed);
        tx1.send(pending).await.expect("send gen-1 op");
        tokio::time::timeout(Duration::from_secs(2), recv)
            .await
            .expect("gen-1 ack timeout")
            .expect("gen-1 ack channel")
            .expect("gen-1 op included");
    }

    let snapshotted = wait_until(Duration::from_secs(2), || {
        let mut s = Storage::open(db.path.as_str()).expect("open");
        s.latest_pending_dump()
            .expect("read pending")
            .map(|p| read_dump_counter(&p.dump.prefix) == 3)
            .unwrap_or(false)
    })
    .await;
    assert!(
        snapshotted,
        "gen-1 should snapshot 3 executed ops into the latest pending dump"
    );
    {
        let mut s = Storage::open(db.path.as_str()).expect("open");
        assert_eq!(
            s.finalized_dump()
                .expect("finalized")
                .expect("genesis finalized exists")
                .l2_tx_index,
            0,
            "finalized must still be genesis (nothing was promoted)"
        );
    }
    shutdown_lane(&shutdown1, handle1).await;

    // Generation 2: restart on the same DB + dumps_dir. A moderate
    // max_batch_open lets the post-restart op's batch close so its snapshot
    // externalizes the resumed counter.
    let mut config2 = config1.clone();
    config2.max_batch_open = Duration::from_millis(50);

    let mut storage2 = Storage::open(db.path.as_str()).expect("reopen storage");
    // Restart: the Tip already exists, so this loads its head (warm path).
    storage2.ensure_open_tip().expect("load existing tip");
    let shutdown2 = ShutdownSignal::default();
    let (tx2, handle2) =
        InclusionLane::<UserOpCounterApp>::start(128, shutdown2.clone(), storage2, config2);

    let (pending, recv) = make_pending_user_op(0x20);
    tx2.send(pending).await.expect("send gen-2 op");
    tokio::time::timeout(Duration::from_secs(2), recv)
        .await
        .expect("gen-2 ack timeout")
        .expect("gen-2 ack channel")
        .expect("gen-2 op included");

    let reached_four = wait_until(Duration::from_secs(2), || {
        let mut s = Storage::open(db.path.as_str()).expect("open");
        s.latest_pending_dump()
            .expect("read pending")
            .map(|p| read_dump_counter(&p.dump.prefix) == 4)
            .unwrap_or(false)
    })
    .await;
    let observed = {
        let mut s = Storage::open(db.path.as_str()).expect("open");
        s.latest_pending_dump()
            .expect("read pending")
            .map(|p| read_dump_counter(&p.dump.prefix))
    };
    shutdown_lane(&shutdown2, handle2).await;

    assert!(
        reached_four,
        "after restart the snapshot counter should reach 4 (resumed 3 + 1 new op); observed {observed:?}. \
         A counter of 1 means the lane loaded genesis (0) and skipped the pending batch's txs."
    );
}

/// Regression for the empty-batch snapshot offset: a batch that closes
/// with no sequenced txs of its own still reflects state through the
/// prior global replay head. Recording `0` means a later promotion to
/// finalized would make catch-up replay the entire history again,
/// double-applying every prior tx.
#[test]
fn empty_batch_snapshot_records_global_replay_head_not_genesis() {
    let db = temp_db("empty-batch-snapshot-head");
    // Batch 0 gets 6 sequenced txs; close it, then close an empty batch 1.
    let _expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut head = storage
        .open_state()
        .expect("open state")
        .expect("batch 0 is the open tip");
    storage
        .close_frame_and_batch(&mut head, 30)
        .expect("close batch 0 (6 txs)");
    storage
        .close_frame_and_batch(&mut head, 30)
        .expect("close empty batch 1");

    let dumps_dir = tempfile::tempdir().expect("dumps dir");
    let app = TestApp::default();
    super::snapshot::take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 1)
        .expect("take dump for empty batch 1");

    let pending = storage
        .latest_pending_dump()
        .expect("read pending")
        .expect("pending row for batch 1");
    assert_eq!(
        pending.nonce, 1,
        "snapshot keyed by the empty batch's nonce"
    );

    let global_head: u64 = {
        let conn = Storage::open_connection(db.path.as_str()).expect("open reader");
        conn.query_row("SELECT MAX(offset) FROM sequenced_l2_txs", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("max offset") as u64
    };

    assert_eq!(
        pending.l2_tx_index, global_head,
        "empty-batch snapshot must record the global replay head ({global_head}), not genesis (0); \
         otherwise catch-up after this snapshot is finalized replays the whole stream and double-applies it"
    );
}

/// Regression for the promote/drain wedge. The pre-fix per-block path promoted
/// a batch in one transaction and advanced the safe-input drain
/// (`close_frame_only`) in a *separate* one. A crash between them left a
/// promoted-but-undrained batch; on restart the lane re-processed the same safe
/// input, re-derived the accepted nonce, and called `promote_finalized` on a
/// now-deleted pending row → `QueryReturnedNoRows` → crash-loop (verified
/// against the SQL: `accepted_batch_nonce_at` has no pending-row gate, and
/// `next_undrained` advances only when inputs are sequenced by the drain).
///
/// `close_frame_only_promoting` folds the promotion into the drain's
/// transaction, so a committed promotion always comes with an advanced drain —
/// the wedge state is unrepresentable.
#[test]
fn promotion_advances_drain_atomically_so_restart_cannot_re_promote() {
    let db = temp_db("promote-drain-atomic");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("open batch 0");

    // Our batch 0 lands on L1 as safe input 0; the scheduler accepts it as
    // nonce 0 (this is what populates `safe_accepted_batches`).
    let batch0 = StoredSafeInput {
        sender: SENDER_A,
        payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: Vec::new(),
        }),
        block_number: 100,
    };
    storage
        .append_safe_inputs(
            100,
            std::slice::from_ref(&batch0),
            SENDER_A,
            &default_protocol_timing(),
        )
        .expect("append our batch as a safe input");

    // Close batch 0 off-chain and register its pending snapshot.
    storage
        .close_frame_and_batch(&mut head, 100)
        .expect("close batch 0");
    let dumps = tempfile::tempdir().expect("dumps dir");
    super::snapshot::take_dump_at_batch_close(&TestApp::default(), &mut storage, dumps.path(), 0)
        .expect("pending snapshot for batch 0");

    // The lane advances the safe frontier over batch 0's landing: it promotes
    // batch 0 AND sequences the drain in one transaction.
    storage
        .close_frame_only_promoting(&mut head, 100, SafeInputRange::new(0, 1), 0, 100)
        .expect("atomic close-frame + promote");

    // The promotion committed...
    assert_eq!(
        storage
            .finalized_dump()
            .unwrap()
            .expect("finalized")
            .inclusion_block,
        100,
    );
    // ...and the drain advanced past batch 0's safe input in the SAME commit, so
    // a restart resumes *after* it and never re-processes (hence never
    // re-promotes) the batch whose pending row promotion deleted.
    assert!(
        storage.next_undrained_safe_input_index().unwrap() > 0,
        "drain must advance past the promoted batch's safe input atomically with \
         the promotion — otherwise a restart re-processes safe input 0 and \
         re-promotes a now-deleted pending row (QueryReturnedNoRows wedge)",
    );
}

/// The atomicity complement: if the promotion fails *inside* the combined
/// transaction (here, a missing pending row), the drain advance rolls back with
/// it. There is never a half-applied "drained but not promoted" state — the
/// mirror of the wedge.
#[test]
fn close_frame_only_promoting_rolls_back_the_drain_when_promotion_fails() {
    let db = temp_db("close-promote-rollback");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("open batch 0");

    // A safe input exists to drain, but there is no pending snapshot for the
    // nonce we ask to promote, so `promote_finalized_in` errors mid-transaction.
    let batch0 = StoredSafeInput {
        sender: SENDER_A,
        payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: Vec::new(),
        }),
        block_number: 100,
    };
    storage
        .append_safe_inputs(
            100,
            std::slice::from_ref(&batch0),
            SENDER_A,
            &default_protocol_timing(),
        )
        .expect("append safe input");

    let result =
        storage.close_frame_only_promoting(&mut head, 100, SafeInputRange::new(0, 1), 7, 100);
    assert!(
        result.is_err(),
        "promoting a missing pending row must fail the whole call",
    );

    // Nothing committed: no finalized promotion, and the drain did not advance.
    assert!(
        storage.finalized_dump().unwrap().is_none(),
        "no finalized snapshot after the failed promotion",
    );
    assert_eq!(
        storage.next_undrained_safe_input_index().unwrap(),
        0,
        "the drain rolled back together with the failed promotion",
    );
}
