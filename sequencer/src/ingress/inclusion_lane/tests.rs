// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, Signature};
use app_core::application::MAX_METHOD_PAYLOAD_BYTES as WALLET_MAX_METHOD_PAYLOAD_BYTES;
use rusqlite::params;
use tokio::sync::{mpsc, oneshot};

use crate::runtime::shutdown::RuntimeScope;
use crate::storage::test_helpers::{
    SENDER_A, default_protocol_timing, pin_test_deployment_identity, record_canonical_divergence,
    temp_db,
};
use crate::storage::{DirectInputExecution, SafeInputRange, Storage, StoredSafeInput, WriteHead};
use sequencer_core::application::{
    AppError, AppOutputs, Application, ApplicationProgress, ApplyInputCapability, InvalidReason,
    ProgressCommitCapability,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};
use sequencer_core::user_op::{SignedUserOp, UserOp};

use super::catch_up::{catch_up_application_paged, catch_up_snapshot};
use super::dequeue_and_execute_user_op_chunk;
use super::error::CatchUpError;
use super::{
    FastTurnSummary, IncludedUserOp, InclusionLane, InclusionLaneConfig, InclusionLaneError,
    LaneState, PendingUserOp, SequencerError,
};

fn encode_progress(progress: ApplicationProgress) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&progress.executed_input_count().get().to_le_bytes());
    bytes[8..].copy_from_slice(&progress.last_executed_safe_block().to_le_bytes());
    bytes
}

fn decode_progress(bytes: &[u8], app_name: &str) -> Result<ApplicationProgress, AppError> {
    if bytes.len() != 16 {
        return Err(AppError::Internal {
            reason: format!("{app_name} dump must be exactly 16 bytes"),
        });
    }
    let count = u64::from_le_bytes(bytes[..8].try_into().expect("checked slice length"));
    let safe_block = u64::from_le_bytes(bytes[8..].try_into().expect("checked slice length"));
    Ok(
        ApplicationProgress::try_new(ExecutedInputCount::new(count), safe_block)
            .expect("coherent progress"),
    )
}

#[derive(Default)]
struct TestApp {
    nonces: HashMap<Address, u32>,
    progress: ApplicationProgress,
    /// Test-only scheduling seam used to keep a rejected queue saturated long
    /// enough to distinguish one bounded turn from an unbounded drain.
    reject_user_ops_after: Option<Duration>,
}

impl Application for TestApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

    fn validate_user_op(
        &self,
        _sender: Address,
        user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        if let Some(delay) = self.reject_user_ops_after {
            std::thread::sleep(delay);
            return Err(InvalidReason::InvalidNonce {
                expected: user_op.nonce.wrapping_add(1),
                got: user_op.nonce,
            });
        }
        Ok(())
    }

    fn apply_valid_user_op(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        let current = self.nonces.get(&user_op.sender).copied().unwrap_or(0);
        let next_nonce = current.wrapping_add(1);
        self.nonces.insert(user_op.sender, next_nonce);
        Ok(Vec::new())
    }

    fn apply_direct_input(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _input: &DirectInput,
    ) -> Result<AppOutputs, AppError> {
        Ok(Vec::new())
    }

    fn execution_progress(&self) -> &ApplicationProgress {
        &self.progress
    }

    fn execution_progress_mut(
        &mut self,
        _capability: ProgressCommitCapability<'_>,
    ) -> &mut ApplicationProgress {
        &mut self.progress
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

#[derive(Default)]
struct InternalUserOpApp {
    progress: ApplicationProgress,
}

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

    fn apply_valid_user_op(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        Err(AppError::Internal {
            reason: "app invariant failed".to_string(),
        })
    }

    fn apply_direct_input(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _input: &DirectInput,
    ) -> Result<AppOutputs, AppError> {
        unimplemented!("not used in these tests")
    }

    fn execution_progress(&self) -> &ApplicationProgress {
        &self.progress
    }

    fn execution_progress_mut(
        &mut self,
        _capability: ProgressCommitCapability<'_>,
    ) -> &mut ApplicationProgress {
        &mut self.progress
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
    progress: ApplicationProgress,
    replayed: Vec<ReplayEvent>,
}

/// App that counts direct-input executions and persists the count
/// through `create_dump` / `from_dump`. Tests verify the count by
/// reading the state file from a snapshot (rather than by sharing
/// an `Arc<AtomicU64>` across thread boundaries) — the lane loads
/// its own instance via `from_dump`, so external observation has to
/// go through the on-disk snapshot.
struct SharedCountingApp {
    progress: ApplicationProgress,
}

impl SharedCountingApp {
    fn new() -> Self {
        Self {
            progress: ApplicationProgress::default(),
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

    fn apply_valid_user_op(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        Ok(Vec::new())
    }

    fn apply_direct_input(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _input: &DirectInput,
    ) -> Result<AppOutputs, AppError> {
        Ok(Vec::new())
    }

    fn execution_progress(&self) -> &ApplicationProgress {
        &self.progress
    }

    fn execution_progress_mut(
        &mut self,
        _capability: ProgressCommitCapability<'_>,
    ) -> &mut ApplicationProgress {
        &mut self.progress
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let bytes = std::fs::read(Self::state_file_in_dump(prefix))?;
        let progress = decode_progress(bytes.as_slice(), "SharedCountingApp")?;
        Ok(Self { progress })
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        std::fs::write(
            Self::state_file_in_dump(prefix),
            encode_progress(self.progress),
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
            progress: ApplicationProgress::try_new(
                ExecutedInputCount::new(executed_input_count),
                0,
            )
            .expect("coherent progress"),
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

    fn apply_valid_user_op(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        self.replayed.push(ReplayEvent::UserOp {
            sender: user_op.sender,
            data: user_op.data.clone(),
        });
        Ok(Vec::new())
    }

    fn apply_direct_input(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        input: &DirectInput,
    ) -> Result<AppOutputs, AppError> {
        self.replayed.push(ReplayEvent::DirectInput {
            sender: input.sender,
            block_number: input.block_number,
            payload: input.payload.clone(),
        });
        Ok(Vec::new())
    }

    fn execution_progress(&self) -> &ApplicationProgress {
        &self.progress
    }

    fn execution_progress_mut(
        &mut self,
        _capability: ProgressCommitCapability<'_>,
    ) -> &mut ApplicationProgress {
        &mut self.progress
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
    RuntimeScope,
    tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
) {
    let mut storage = Storage::open(db_path).expect("open storage");
    pin_test_deployment_identity(&mut storage, config.batch_submitter_address);
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
    let shutdown = RuntimeScope::default();
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

fn make_included_user_op(seed: u8, offset: u64) -> IncludedUserOp {
    let (pending, _response) = make_pending_user_op(seed);
    IncludedUserOp {
        pending,
        executed_input_offset: ExecutedInputCount::new(offset),
    }
}

#[tokio::test]
async fn terminal_storage_fault_rejects_current_and_queued_ops_before_persistence() {
    let db = temp_db("lane-terminal-storage-fault");
    let storage = Storage::open(db.path.as_str()).expect("open storage");
    let shutdown = RuntimeScope::default();
    let (tx, rx) = mpsc::channel(2);
    let (current, current_response) = make_pending_user_op(0x61);
    let (queued, queued_response) = make_pending_user_op(0x62);
    tx.try_send(queued).expect("queue pending op");

    let mut lane = InclusionLane {
        rx,
        shutdown: shutdown.clone(),
        app: TestApp::default(),
        storage,
        config: default_test_config(),
    };
    let mut included = vec![IncludedUserOp {
        pending: current,
        executed_input_offset: ExecutedInputCount::ZERO,
    }];
    shutdown.contain_storage_invariant_failure("test fault");

    assert!(
        lane.shutdown.authorize().is_none(),
        "containment must refuse the externalization token"
    );
    assert!(matches!(
        lane.refuse_externalization(&mut included),
        InclusionLaneError::TerminalStorageInvariant
    ));
    assert!(included.is_empty());
    assert!(matches!(
        current_response.await.expect("current response"),
        Err(SequencerError::Unavailable(_))
    ));
    assert!(matches!(
        queued_response.await.expect("queued response"),
        Err(SequencerError::Unavailable(_))
    ));
    assert!(
        lane.storage
            .ordered_l2_txs_page_from(0, 1)
            .expect("read ordered txs")
            .is_empty()
    );
}

#[test]
fn fast_turn_processes_at_most_one_rejected_chunk() {
    let db = temp_db("one-rejected-chunk-per-fast-turn");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
        .expect("seed observed safe head");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");

    let (tx, rx) = mpsc::channel(8);
    let mut responses = Vec::new();
    for seed in 1..=8 {
        let (mut pending, response) = make_pending_user_op(seed);
        pending.signed.user_op.max_fee = 0;
        tx.try_send(pending).expect("prefill rejected request");
        responses.push(response);
    }

    let mut config = default_test_config();
    config.max_user_ops_per_chunk = 4;
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: TestApp::default(),
        storage,
        config,
    };
    let mut included = Vec::new();

    let summary = lane
        .run_fast_turn(&mut head, &mut included)
        .expect("run one fast turn");

    assert_eq!(summary, FastTurnSummary::Processed);
    assert_eq!(lane.rx.len(), 4, "exactly one four-attempt chunk runs");
    assert_eq!(read_count(db.path.as_str(), "user_ops"), 0);
    for response in responses.iter_mut().take(4) {
        assert!(matches!(
            response.try_recv(),
            Ok(Err(SequencerError::Invalid(_)))
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sustained_rejected_queue_cannot_starve_poisoned_frontier() {
    let db = temp_db("reject-flood-does-not-starve-frontier");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
        .expect("seed observed safe head");
    storage.ensure_open_tip().expect("establish open tip");

    let (tx, rx) = mpsc::channel(128);
    let (first, first_response) = make_pending_user_op(0x80);
    tx.try_send(first).expect("seed first rejected request");
    for seed in 1..128_u8 {
        let (pending, _response) = make_pending_user_op(seed);
        tx.try_send(pending).expect("saturate reject queue");
    }

    let shutdown = RuntimeScope::default();
    let mut config = default_test_config();
    config.max_user_ops_per_chunk = 4;
    let mut lane = InclusionLane {
        rx,
        shutdown: shutdown.clone(),
        app: TestApp {
            reject_user_ops_after: Some(Duration::from_millis(2)),
            ..TestApp::default()
        },
        storage,
        config,
    };
    let mut lane_handle = tokio::task::spawn_blocking(move || lane.run_forever(0));

    let producer_tx = tx.clone();
    let producer = tokio::spawn(async move {
        let mut seed = 0_u8;
        loop {
            let (pending, _response) = make_pending_user_op(seed);
            if producer_tx.send(pending).await.is_err() {
                return;
            }
            seed = seed.wrapping_add(1);
        }
    });

    let first_result = tokio::time::timeout(Duration::from_secs(1), first_response)
        .await
        .expect("lane must begin processing the saturated queue")
        .expect("first response channel open");
    assert!(matches!(first_result, Err(SequencerError::Invalid(_))));

    let mut poisoner = Storage::open(db.path.as_str()).expect("open divergence writer");
    record_canonical_divergence(&mut poisoner, 7, 0);

    let lane_result = match tokio::time::timeout(Duration::from_secs(1), &mut lane_handle).await {
        Ok(joined) => joined.expect("join lane task"),
        Err(_) => {
            producer.abort();
            drop(tx);
            shutdown.request_shutdown();
            let late_result = lane_handle.await.expect("join timed-out lane task");
            panic!("reject flood starved the poisoned frontier: {late_result:?}");
        }
    };
    drop(tx);
    producer.await.expect("join reject producer");

    assert!(matches!(
        lane_result,
        Err(InclusionLaneError::CanonicalDivergence {
            nonce: 7,
            safe_input_index: 0,
        })
    ));
    assert_eq!(read_count(db.path.as_str(), "user_ops"), 0);
}

#[test]
fn reconciliation_digests_an_epoch_sized_outage_backlog_in_one_turn() {
    // The ADR's digestibility assumption — the complete accumulated
    // newly-safe range is consumed in ONE reconciliation turn, with no
    // timeout/resume protocol — exercised at L1-outage scale rather than
    // the unit-sized ranges the clock tests use. Functional assertions only
    // (everything drains, one frame at the observed tip); the printed wall
    // time is developer evidence. The full ACK-latency-during-catch-up
    // measurement stays with the benchmark harness.
    const BACKLOG_DIRECTS: u64 = 5_000;
    const JUMP_TARGET_BLOCK: u64 = 7_200; // ~a day of 12s safe blocks

    let db = temp_db("digestibility-epoch-backlog");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let config = default_test_config();
    pin_test_deployment_identity(&mut storage, config.batch_submitter_address);
    storage
        .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
        .expect("seed observed safe head");
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");

    let backlog: Vec<StoredSafeInput> = (0..BACKLOG_DIRECTS)
        .map(|i| StoredSafeInput {
            sender: Address::ZERO,
            payload: vec![0xd1],
            block_number: 1 + (i * (JUMP_TARGET_BLOCK - 1)) / BACKLOG_DIRECTS,
        })
        .collect();
    storage
        .append_safe_inputs(
            JUMP_TARGET_BLOCK,
            backlog.as_slice(),
            SENDER_A,
            &default_protocol_timing(),
        )
        .expect("seed the outage backlog at the jump target");

    let (_tx, rx) = mpsc::channel(1);
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: TestApp::default(),
        storage,
        config,
    };
    let mut state = LaneState::new(SafeInputRange::empty_at(0), head);
    let mut safe_inputs = Vec::new();

    let started = std::time::Instant::now();
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("one reconciliation turn digests the complete range");
    let elapsed = started.elapsed();

    assert_eq!(
        lane.storage.next_undrained_safe_input_index().unwrap(),
        BACKLOG_DIRECTS,
        "the complete accumulated range drains in one turn"
    );
    assert_eq!(
        read_frame_safe_blocks(db.path.as_str()),
        vec![0, JUMP_TARGET_BLOCK],
        "one frame at the observed tip; no synthetic intermediate ticks"
    );
    println!(
        "digestibility seed: {BACKLOG_DIRECTS} directs over a {JUMP_TARGET_BLOCK}-block jump \
         drained in one turn in {elapsed:?}"
    );
}

#[test]
fn frame_clock_waits_five_blocks_and_collapses_observation_jumps() {
    let db = temp_db("frame-clock-block-interval");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let config = default_test_config();
    pin_test_deployment_identity(&mut storage, config.batch_submitter_address);
    storage
        .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
        .expect("seed observed safe head");
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    let (tx, rx) = mpsc::channel(1);
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: TestApp::default(),
        storage,
        config,
    };
    let mut state = LaneState::new(SafeInputRange::empty_at(0), head);
    let mut safe_inputs = Vec::new();

    lane.storage
        .append_safe_inputs(
            4,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 4,
            }],
            SENDER_A,
            &default_protocol_timing(),
        )
        .expect("append below-threshold direct");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("observe block 4");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0]);
    assert_eq!(lane.storage.next_undrained_safe_input_index().unwrap(), 0);

    lane.storage
        .append_safe_inputs(5, &[], SENDER_A, &default_protocol_timing())
        .expect("advance to first clock tick");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("rotate at block 5");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 5]);
    assert_eq!(read_frame_direct_count(db.path.as_str(), 0, 1), 1);

    let (pending, mut response) = make_pending_user_op(0x72);
    tx.try_send(pending).expect("queue op after clock tick");
    let mut included = Vec::new();
    assert_eq!(
        lane.run_fast_turn(&mut state.head, &mut included)
            .expect("execute op at frame clock"),
        FastTurnSummary::Processed
    );
    assert!(matches!(response.try_recv(), Ok(Ok(()))));
    let sequenced = lane
        .storage
        .ordered_l2_txs_page_from(0, 10)
        .expect("read sequenced clock values");
    assert_eq!(sequenced.len(), 2);
    assert!(matches!(
        &sequenced[0].tx,
        SequencedL2Tx::Direct(DirectInput {
            block_number: 4,
            ..
        })
    ));
    assert_eq!(sequenced[0].frame_safe_block, 5);
    assert!(matches!(&sequenced[1].tx, SequencedL2Tx::UserOp(_)));
    assert_eq!(sequenced[1].frame_safe_block, 5);

    lane.storage
        .append_safe_inputs(32, &[], SENDER_A, &default_protocol_timing())
        .expect("jump safe head");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("collapse jump to one frame");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 5, 32]);

    lane.storage
        .append_safe_inputs(36, &[], SENDER_A, &default_protocol_timing())
        .expect("advance below reset threshold");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("observe block 36");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 5, 32]);

    lane.storage
        .append_safe_inputs(37, &[], SENDER_A, &default_protocol_timing())
        .expect("advance to reset threshold");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("rotate at block 37");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 5, 32, 37]);
    drop(tx);
}

#[test]
fn structural_batch_frame_does_not_reset_frame_clock_anchor() {
    let db = temp_db("batch-frame-does-not-reset-clock");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
        .expect("seed observed safe head");
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    let (_tx, rx) = mpsc::channel(1);
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: TestApp::default(),
        storage,
        config: default_test_config(),
    };
    let mut state = LaneState::new(SafeInputRange::empty_at(0), head);
    let mut safe_inputs = Vec::new();

    lane.storage
        .append_safe_inputs(4, &[], SENDER_A, &default_protocol_timing())
        .expect("advance below threshold");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("observe block 4");
    let unchanged_clock = state.head.safe_block;
    lane.storage
        .close_frame_and_batch(&mut state.head, unchanged_clock)
        .expect("create successor batch frame");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 0]);

    lane.storage
        .append_safe_inputs(5, &[], SENDER_A, &default_protocol_timing())
        .expect("reach original five-block threshold");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("clock tick after structural frame");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 0, 5]);
}

#[test]
fn poisoned_frontier_outranks_frame_clock_and_closes_intake() {
    let db = temp_db("poisoned-frontier-outranks-clock");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
        .expect("seed observed safe head");
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    record_canonical_divergence(&mut storage, 7, 0);

    let (tx, rx) = mpsc::channel(2);
    let (pending, mut response) = make_pending_user_op(0x70);
    tx.try_send(pending)
        .expect("queue request before poison read");
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: TestApp::default(),
        storage,
        config: default_test_config(),
    };
    let mut state = LaneState::new(SafeInputRange::empty_at(0), head);
    let mut safe_inputs = Vec::new();

    let err = lane
        .maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect_err("poison must prevent reconciliation even with zero block delta");

    assert!(matches!(
        &err,
        InclusionLaneError::CanonicalDivergence {
            nonce: 7,
            safe_input_index: 0,
        }
    ));
    assert!(err.is_terminal_invariant());
    assert!(matches!(
        response.try_recv(),
        Ok(Err(SequencerError::Unavailable(_)))
    ));
    let (late, _late_response) = make_pending_user_op(0x71);
    assert!(matches!(
        tx.try_send(late),
        Err(mpsc::error::TrySendError::Closed(_))
    ));
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0]);
}

fn seed_replay_fixture(db_path: &str) -> Vec<ReplayEvent> {
    let mut storage = Storage::open(db_path).expect("open storage");
    let batch_submitter_address = Address::from([0xff; 20]);
    pin_test_deployment_identity(&mut storage, batch_submitter_address);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");

    let user_op_a = make_included_user_op(0x51, 0);
    let user_op_b = make_included_user_op(0x52, 1);
    storage
        .append_executed_user_ops_chunk(&mut head, &[user_op_a, user_op_b])
        .expect("append attributed first-frame user ops");
    storage
        .append_safe_inputs(
            10,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            }],
            batch_submitter_address,
            &default_protocol_timing(),
        )
        .expect("append first direct input");
    storage
        .close_frame_only_with_executions(
            &mut head,
            10,
            SafeInputRange::new(0, 1),
            &[DirectInputExecution {
                safe_input_index: 0,
                executed_input_offset: ExecutedInputCount::new(2),
            }],
        )
        .expect("close first frame with direct attribution");

    let user_op_c = make_included_user_op(0x53, 3);
    storage
        .append_executed_user_ops_chunk(&mut head, &[user_op_c])
        .expect("append attributed second-frame user op");
    storage
        .append_safe_inputs(
            20,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 20,
            }],
            batch_submitter_address,
            &default_protocol_timing(),
        )
        .expect("append second direct input");
    storage
        .close_frame_only_with_executions(
            &mut head,
            20,
            SafeInputRange::new(1, 2),
            &[DirectInputExecution {
                safe_input_index: 1,
                executed_input_offset: ExecutedInputCount::new(4),
            }],
        )
        .expect("close second frame with direct attribution");

    storage
        .append_safe_inputs(
            30,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xcc],
                block_number: 30,
            }],
            batch_submitter_address,
            &default_protocol_timing(),
        )
        .expect("append third direct input");
    storage
        .close_frame_only_with_executions(
            &mut head,
            30,
            SafeInputRange::new(2, 3),
            &[DirectInputExecution {
                safe_input_index: 2,
                executed_input_offset: ExecutedInputCount::new(5),
            }],
        )
        .expect("close third frame with direct attribution");

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

fn read_frame_safe_blocks(db_path: &str) -> Vec<u64> {
    let conn = Storage::open_connection(db_path).expect("open sqlite reader");
    let mut statement = conn
        .prepare(
            "SELECT safe_block FROM frames \
             ORDER BY batch_index ASC, frame_in_batch ASC",
        )
        .expect("prepare frame-clock query");
    statement
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("query frame clocks")
        .map(|value| u64::try_from(value.expect("read frame clock")).expect("nonnegative clock"))
        .collect()
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
    shutdown: &RuntimeScope,
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
    pin_test_deployment_identity(&mut storage, batch_submitter_address);
    {
        let app = SharedCountingApp::new();
        register_genesis_snapshot(&app, &mut storage, &config.dumps_dir);
    }
    storage.ensure_open_tip().expect("establish genesis tip");
    let shutdown = RuntimeScope::default();
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
                // A well-formed but unexpected-nonce batch is a production-
                // faithful own-input row that the scheduler rejects. It must
                // still advance the physical drain cursor without executing
                // in the application.
                payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                    nonce: 1,
                    frames: Vec::new(),
                }),
                block_number: 10,
            }],
            batch_submitter_address,
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

    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let replay = storage
        .ordered_l2_txs_page_from(0, 16)
        .expect("load own-batch replay row");
    assert_eq!(replay.len(), 1);
    assert!(matches!(
        &replay[0].tx,
        SequencedL2Tx::Direct(DirectInput { sender, .. })
            if *sender == batch_submitter_address
    ));
    assert_eq!(replay[0].executed_input_offset, None);

    // The lane's own batch input was drained into a frame and
    // sequenced into `sequenced_l2_txs`, but the lane skipped
    // shared application-execution boundary for it. Catch-up replays the same
    // sequenced stream and also filters batch-submitter rows — so a
    // fresh `SharedCountingApp` driven through `catch_up_application`
    // ends with counter == 0, confirming the symmetric skip.
    let mut fresh_app = SharedCountingApp::new();
    catch_up_application_paged(&mut fresh_app, &mut storage, batch_submitter_address, 0, 16)
        .expect("catch up");
    assert_eq!(
        fresh_app.executed_input_count().get(),
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
            .map(|row| row.tx)
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

    let mut app = InternalUserOpApp::default();
    let mut included = Vec::new();
    let head = unbounded_head();
    let err = dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 16, &head, &mut included)
        .expect_err("internal application error should stop the lane");

    // The application's reason travels on the lane error (and the log) ...
    assert!(matches!(
        &err,
        InclusionLaneError::ExecuteUserOp { source }
            if source.to_string().contains("app invariant failed")
    ));
    assert!(
        included.is_empty(),
        "internal errors must not leave an op ready to persist"
    );
    // ... never into the client's 500 body, which is fixed text.
    let response = recv
        .blocking_recv()
        .expect("lane should respond to triggering op")
        .expect_err("triggering op should receive internal error");
    assert!(matches!(
        response,
        super::SequencerError::Internal(message) if message == "application internal error"
    ));
}

#[test]
fn catch_up_replays_multiple_pages() {
    let db = temp_db("catch-up-multi-page");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mappings: Vec<_> = storage
        .ordered_l2_txs_page_from(0, 16)
        .expect("load attributed replay rows")
        .into_iter()
        .map(|row| row.executed_input_offset.map(ExecutedInputCount::get))
        .collect();
    assert_eq!(
        mappings,
        vec![Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)]
    );
    let mut app = ReplayRecordingApp::default();

    catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 0, 2)
        .expect("catch up in pages");

    assert_eq!(app.replayed, expected);
    assert_eq!(app.executed_input_count().get(), expected.len() as u64);
    assert_eq!(
        app.last_executed_safe_block(),
        30,
        "catch-up must advance scheduler-owned progress through the shared boundary"
    );
}

#[test]
fn catch_up_rejects_missing_mapping_before_execution() {
    let db = temp_db("catch-up-missing-mapping");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    pin_test_deployment_identity(&mut storage, Address::from([0xff; 20]));
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    let (unmapped, _response) = make_pending_user_op(0x51);
    storage
        .append_user_ops_chunk(&mut head, &[unmapped])
        .expect("seed intentionally unmapped physical user op");

    let mut app = ReplayRecordingApp::default();
    let err = catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 0, 2)
        .expect_err("missing canonical mapping must stop catch-up");

    assert!(matches!(
        &err,
        CatchUpError::ExecutionOffsetMismatch {
            db_offset: 1,
            kind: "user op",
            expected: Some(0),
            stored: None,
        }
    ));
    assert!(
        app.replayed.is_empty(),
        "mapping is checked before execution"
    );
    assert_eq!(app.executed_input_count(), ExecutedInputCount::ZERO);
    assert!(InclusionLaneError::CatchUp { source: err }.is_terminal_invariant());
}

#[test]
fn catch_up_rejects_wrong_mapping_before_execution() {
    let db = temp_db("catch-up-wrong-mapping");
    seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut app = ReplayRecordingApp::with_executed_input_count(3);

    let err = catch_up_application_paged(&mut app, &mut storage, Address::from([0xff; 20]), 0, 2)
        .expect_err("mapping from a different application boundary must stop catch-up");

    assert!(matches!(
        &err,
        CatchUpError::ExecutionOffsetMismatch {
            db_offset: 1,
            kind: "user op",
            expected: Some(3),
            stored: Some(0),
        }
    ));
    assert!(
        app.replayed.is_empty(),
        "mapping is checked before execution"
    );
    assert_eq!(app.executed_input_count(), ExecutedInputCount::new(3));
    assert!(InclusionLaneError::CatchUp { source: err }.is_terminal_invariant());
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
fn standard_recovery_rebases_history_and_restart_on_surviving_checkpoint() {
    let db = temp_db("standard-recovery-history-boundary");
    let dumps_dir = tempfile::tempdir().expect("create dump directory");
    let batch_submitter = Address::from([0xff; 20]);
    let direct_sender = Address::repeat_byte(0x44);
    let protocol = default_protocol_timing();
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    pin_test_deployment_identity(&mut storage, batch_submitter);
    storage
        .append_safe_inputs(0, &[], batch_submitter, &protocol)
        .expect("seed genesis safe head");
    storage.ensure_open_tip().expect("open genesis Tip");
    let mut head = storage.open_state().unwrap().unwrap();
    let initial_history = storage.history_state().expect("initial history");
    let mut live_app = SharedCountingApp::new();

    // Batch 0 is the surviving prefix. Its pending snapshot reflects exactly
    // input 0 and becomes the recovery checkpoint once the batch lands.
    let (prefix_op, _response) = make_pending_user_op(0x51);
    let mut included = Vec::new();
    super::execute_user_op(
        &mut live_app,
        prefix_op,
        head.frame_fee,
        head.safe_block,
        &mut included,
    )
    .expect("execute prefix user op");
    assert_eq!(included[0].executed_input_offset, ExecutedInputCount::ZERO);
    storage
        .append_executed_user_ops_chunk(&mut head, &included)
        .expect("persist prefix user op");
    super::snapshot::close_batch_with_snapshot(
        &live_app,
        &mut storage,
        &mut head,
        0,
        dumps_dir.path(),
    )
    .expect("close prefix batch with snapshot");

    let prefix_landing = crate::storage::test_helpers::local_batch_payload(&mut storage, 0);
    let direct = DirectInput {
        sender: direct_sender,
        block_number: 10,
        payload: vec![0xd1],
    };
    storage
        .append_safe_inputs(
            10,
            &[
                StoredSafeInput {
                    sender: batch_submitter,
                    payload: prefix_landing,
                    block_number: 10,
                },
                StoredSafeInput {
                    sender: direct.sender,
                    payload: direct.payload.clone(),
                    block_number: direct.block_number,
                },
            ],
            batch_submitter,
            &protocol,
        )
        .expect("observe prefix landing and direct input");
    let direct_receipt = sequencer_core::application::execute_direct_input(&mut live_app, &direct)
        .expect("execute direct input in doomed suffix");
    assert_eq!(direct_receipt.offset, ExecutedInputCount::new(1));
    storage
        .close_frame_only_promoting_with_executions(
            &mut head,
            10,
            SafeInputRange::new(0, 2),
            &[DirectInputExecution {
                safe_input_index: 1,
                executed_input_offset: direct_receipt.offset,
            }],
            0,
            10,
        )
        .expect("drain direct and promote prefix snapshot");
    let finalized_before = storage
        .finalized_dump()
        .expect("read finalized prefix")
        .expect("prefix snapshot promoted");
    assert_eq!(finalized_before.l2_tx_index, 1);
    assert_eq!(
        finalized_before.executed_input_count,
        ExecutedInputCount::new(1)
    );

    // The direct and this user op are both beyond the finalized checkpoint.
    // Closing batch 1 makes it the first non-gold recovery pivot.
    let (doomed_op, _response) = make_pending_user_op(0x52);
    included.clear();
    super::execute_user_op(
        &mut live_app,
        doomed_op,
        head.frame_fee,
        head.safe_block,
        &mut included,
    )
    .expect("execute doomed user op");
    assert_eq!(
        included[0].executed_input_offset,
        ExecutedInputCount::new(2)
    );
    storage
        .append_executed_user_ops_chunk(&mut head, &included)
        .expect("persist doomed user op");
    super::snapshot::close_batch_with_snapshot(
        &live_app,
        &mut storage,
        &mut head,
        10,
        dumps_dir.path(),
    )
    .expect("close doomed batch with snapshot");
    assert_eq!(
        storage.next_executed_input_count().unwrap(),
        ExecutedInputCount::new(3)
    );

    let before_recovery = storage
        .ordered_l2_txs_page_from(0, 32)
        .expect("read pre-recovery history");
    let old_direct_physical = before_recovery
        .iter()
        .find_map(|row| match &row.tx {
            SequencedL2Tx::Direct(value) if value.sender == direct_sender => {
                assert_eq!(row.executed_input_offset, Some(ExecutedInputCount::new(1)));
                Some(row.db_offset)
            }
            _ => None,
        })
        .expect("doomed direct row exists before recovery");

    let invalidated = storage
        .recover_post_flush_for_recovery(10, &protocol, crate::clock::unix_now_ms())
        .expect("standard recovery cascade");
    assert_eq!(invalidated, vec![1, 2]);
    let recovered_history = storage.history_state().expect("recovered history");
    assert_eq!(
        recovered_history.version.era_id, initial_history.version.era_id,
        "standard recovery must stay in the same era"
    );
    assert_eq!(recovered_history.version.recovery_generation.get(), 1);
    assert_eq!(
        storage.next_executed_input_count().unwrap(),
        ExecutedInputCount::new(2),
        "H rolls back the doomed user op while the direct is re-drained"
    );
    assert_eq!(
        storage.finalized_dump().unwrap().unwrap(),
        finalized_before,
        "the accepted prefix checkpoint must survive the cascade"
    );
    assert!(
        storage.latest_pending_dump().unwrap().is_none(),
        "the doomed suffix checkpoint must not survive"
    );

    let after_recovery = storage
        .ordered_l2_txs_page_from(0, 32)
        .expect("read recovered history");
    let (new_direct_physical, new_direct_logical) = after_recovery
        .iter()
        .find_map(|row| match &row.tx {
            SequencedL2Tx::Direct(value) if value.sender == direct_sender => {
                Some((row.db_offset, row.executed_input_offset))
            }
            _ => None,
        })
        .expect("re-drained direct exists after recovery");
    assert!(
        new_direct_physical > old_direct_physical,
        "recovery must physically re-drain the invalidated direct"
    );
    assert_eq!(new_direct_logical, Some(ExecutedInputCount::new(1)));

    // A restart loads the surviving count-1 checkpoint and catches up through
    // the replacement recovery Tip. The invalidated rows are physical audit
    // history only and cannot perturb application progress.
    let checkpoint = catch_up_snapshot(&mut storage).expect("select surviving checkpoint");
    assert_eq!(checkpoint.l2_tx_index, finalized_before.l2_tx_index);
    assert_eq!(
        checkpoint.executed_input_count,
        finalized_before.executed_input_count
    );
    let mut restarted =
        SharedCountingApp::from_dump(&super::dump_info::app_prefix(&checkpoint.dump_dir))
            .expect("load surviving application checkpoint");
    assert_eq!(restarted.executed_input_count(), ExecutedInputCount::new(1));
    catch_up_application_paged(
        &mut restarted,
        &mut storage,
        batch_submitter,
        checkpoint.l2_tx_index,
        2,
    )
    .expect("catch up through recovery re-drain");
    assert_eq!(restarted.executed_input_count(), ExecutedInputCount::new(2));

    // The replacement suffix reuses offset 2, rolling H forward without
    // retaining the invalidated op that previously occupied that coordinate.
    let mut recovery_head = storage.open_state().unwrap().unwrap();
    let (replacement_op, _response) = make_pending_user_op(0x53);
    included.clear();
    super::execute_user_op(
        &mut restarted,
        replacement_op,
        recovery_head.frame_fee,
        recovery_head.safe_block,
        &mut included,
    )
    .expect("execute replacement user op");
    assert_eq!(
        included[0].executed_input_offset,
        ExecutedInputCount::new(2)
    );
    storage
        .append_executed_user_ops_chunk(&mut recovery_head, &included)
        .expect("persist replacement suffix");
    assert_eq!(
        storage.next_executed_input_count().unwrap(),
        ExecutedInputCount::new(3)
    );

    let valid = storage
        .ordered_l2_txs_page_from(0, 32)
        .expect("read replacement history");
    let mappings: Vec<_> = valid
        .iter()
        .map(|row| row.executed_input_offset.map(ExecutedInputCount::get))
        .collect();
    assert_eq!(mappings, vec![Some(0), None, Some(1), Some(2)]);
    let user_seeds: Vec<_> = valid
        .iter()
        .filter_map(|row| match &row.tx {
            SequencedL2Tx::UserOp(value) => Some(value.data[0]),
            SequencedL2Tx::Direct(_) => None,
        })
        .collect();
    assert_eq!(user_seeds, vec![0x51, 0x53]);

    let mut restarted_again =
        SharedCountingApp::from_dump(&super::dump_info::app_prefix(&checkpoint.dump_dir))
            .expect("reload surviving checkpoint");
    catch_up_application_paged(
        &mut restarted_again,
        &mut storage,
        batch_submitter,
        checkpoint.l2_tx_index,
        2,
    )
    .expect("catch up through replacement suffix");
    assert_eq!(
        restarted_again.executed_input_count(),
        ExecutedInputCount::new(3)
    );
    assert_eq!(restarted_again.last_executed_safe_block(), 10);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_refuses_snapshot_whose_application_count_disagrees_with_storage() {
    let db = temp_db("snapshot-execution-count-mismatch");
    let config = default_test_config();
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    pin_test_deployment_identity(&mut storage, config.batch_submitter_address);
    storage
        .append_safe_inputs(
            0,
            &[],
            config.batch_submitter_address,
            &default_protocol_timing(),
        )
        .expect("seed observed safe head");

    let app = SharedCountingApp {
        progress: ApplicationProgress::try_new(ExecutedInputCount::new(1), 0)
            .expect("coherent progress"),
    };
    register_genesis_snapshot(&app, &mut storage, &config.dumps_dir);
    storage.ensure_open_tip().expect("establish genesis tip");

    let shutdown = RuntimeScope::default();
    let (_tx, handle) = InclusionLane::<SharedCountingApp>::start(1, shutdown, storage, config);
    let err = handle
        .await
        .expect("join lane startup")
        .expect_err("snapshot count mismatch must refuse lane startup");

    assert!(matches!(
        &err,
        InclusionLaneError::CatchUp {
            source: CatchUpError::SnapshotExecutionCountMismatch {
                application: 1,
                storage: 0,
            }
        }
    ));
    assert!(err.is_terminal_invariant());
}

/// App that counts executed user ops and persists the count through
/// `create_dump` / `from_dump` (two LE `u64`s at `prefix/state`). The
/// restart-resume regression test reads this count back from the
/// snapshot the lane writes on its own thread — the only way to observe
/// how much state the lane rebuilt across a restart.
struct UserOpCounterApp {
    progress: ApplicationProgress,
}

impl UserOpCounterApp {
    fn new() -> Self {
        Self {
            progress: ApplicationProgress::default(),
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

    fn apply_valid_user_op(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        Ok(Vec::new())
    }

    fn apply_direct_input(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _input: &DirectInput,
    ) -> Result<AppOutputs, AppError> {
        unimplemented!("not used in these tests")
    }

    fn execution_progress(&self) -> &ApplicationProgress {
        &self.progress
    }

    fn execution_progress_mut(
        &mut self,
        _capability: ProgressCommitCapability<'_>,
    ) -> &mut ApplicationProgress {
        &mut self.progress
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let bytes = std::fs::read(Self::state_file_in_dump(prefix))?;
        let progress = decode_progress(bytes.as_slice(), "UserOpCounterApp")?;
        Ok(Self { progress })
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        std::fs::write(
            Self::state_file_in_dump(prefix),
            encode_progress(self.progress),
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
    decode_progress(bytes.as_slice(), "UserOpCounterApp")
        .expect("decode dump progress")
        .executed_input_count()
        .get()
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
    let shutdown1 = RuntimeScope::default();
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
    let shutdown2 = RuntimeScope::default();
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
    // DeferUntilAnchorSet skips acceptance simulation: this test's subject is
    // promote/drain atomicity, and a hand-built landing payload cannot
    // content-match an unsealed local batch — running the content-identity
    // check here would record a divergence marker and (correctly) freeze the
    // batch tree via the I15 triggers.
    storage
        .append_safe_inputs_with_timestamp(
            100,
            100,
            std::slice::from_ref(&batch0),
            SENDER_A,
            &default_protocol_timing(),
            crate::storage::FrontierMode::DeferUntilAnchorSet,
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
