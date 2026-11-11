// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::Path;
use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, Signature};
use rusqlite::params;
use tokio::sync::{mpsc, oneshot};

use crate::runtime::shutdown::RuntimeScope;
use crate::storage::test_helpers::{
    SENDER_A, WriteHeadParts, record_canonical_divergence, set_one_op_batch_target,
};
use crate::storage::{
    DirectInputExecution, IngestedSafeInput, L2TxContext, SafeInputRange, Storage, StoredSafeInput,
    WriteHead,
};
use crate::test_support::{pin_test_deployment_identity, temp_db};
use alloy_primitives::B256;
use sequencer_core::application::{
    Admission, AppError, AppOutputs, Application, SchedulerLimits, SkipReason,
};
use sequencer_core::history::L2TxCount;
use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
use sequencer_core::user_op::{SignedUserOp, UserOp};

use crate::test_support::{ReplayEvent, StubApp, shutdown_lane, wait_until};

use super::catch_up::catch_up_application_paged;
use super::dequeue_and_execute_user_op_chunk;
use super::error::CatchUpError;
use super::{
    IncludedUserOp, InclusionLane, InclusionLaneConfig, InclusionLaneError, LaneState,
    PendingUserOp, SequencerError, TurnOutcome,
};

fn default_test_config() -> InclusionLaneConfig {
    InclusionLaneConfig {
        // A leaked tempdir per call: the lane unconditionally writes
        // dump artifacts there, and the test stubs' `create_dump`
        // creates the directory. Tempdir gets reaped by the OS.
        dumps_dir: tempfile::tempdir()
            .expect("create dumps_dir tempdir")
            .keep(),
        snapshot_leases: crate::storage::SnapshotLeases::default(),
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
fn register_genesis_snapshot<A: Application>(app: &mut A, storage: &mut Storage, dumps_dir: &Path) {
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
        },
    )
    .expect("create genesis dump");
    storage
        .insert_baseline_snapshot(&dump_dir, L2TxCount::ZERO)
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
    pin_test_deployment_identity(&mut storage, Address::repeat_byte(0xff));
    storage
        .append_safe_inputs(0, &[], Address::repeat_byte(0xff))
        .expect("seed observed safe head");
    let mut app = StubApp::default();
    register_genesis_snapshot(&mut app, &mut storage, &config.dumps_dir);
    // `app` instance is dropped here; the lane reloads it from the
    // genesis dump on its background thread.
    drop(app);
    // Establish the tip structurally (as the runtime does at startup), then
    // hand its head to the lane — which now only loads, never initializes.
    storage.ensure_open_tip().expect("establish genesis tip");
    let shutdown = RuntimeScope::default();
    let (tx, handle) = InclusionLane::<StubApp>::start(128, shutdown.clone(), storage, config);
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
        // lane's max_fee >= frame fee check.
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
        l2_tx_index: L2TxCount::new(offset),
    }
}

#[test]
fn fast_turn_processes_at_most_one_rejected_chunk() {
    let db = temp_db("one-rejected-chunk-per-fast-turn");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .append_safe_inputs(0, &[], SENDER_A)
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
        app: StubApp::default(),
        storage,
        config,
    };
    let mut included = Vec::new();

    let summary = lane
        .run_fast_turn(&mut head, &mut included)
        .expect("run one fast turn");

    assert_eq!(summary, TurnOutcome::Processed);
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
        .append_safe_inputs(0, &[], SENDER_A)
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
        app: StubApp {
            reject_user_ops_after: Some(Duration::from_millis(2)),
            ..StubApp::default()
        },
        storage,
        config,
    };
    let mut lane_handle = tokio::task::spawn_blocking(move || lane.run_forever(L2TxCount::ZERO));

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
    pin_test_deployment_identity(&mut storage, Address::repeat_byte(0xff));
    storage
        .append_safe_inputs(0, &[], Address::repeat_byte(0xff))
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
            Address::repeat_byte(0xff),
        )
        .expect("seed the outage backlog at the jump target");

    let (_tx, rx) = mpsc::channel(1);
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: StubApp::default(),
        storage,
        config,
    };
    let mut state = LaneState::new(0, head);
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
    pin_test_deployment_identity(&mut storage, Address::repeat_byte(0xff));
    storage
        .append_safe_inputs(0, &[], Address::repeat_byte(0xff))
        .expect("seed observed safe head");
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    let (tx, rx) = mpsc::channel(1);
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: StubApp::default(),
        storage,
        config,
    };
    let mut state = LaneState::new(0, head);
    let mut safe_inputs = Vec::new();

    lane.storage
        .append_safe_inputs(
            4,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 4,
            }],
            Address::repeat_byte(0xff),
        )
        .expect("append below-threshold direct");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("observe block 4");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0]);
    assert_eq!(lane.storage.next_undrained_safe_input_index().unwrap(), 0);

    lane.storage
        .append_safe_inputs(5, &[], Address::repeat_byte(0xff))
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
        TurnOutcome::Processed
    );
    assert!(matches!(response.try_recv(), Ok(Ok(()))));
    let sequenced = history_rows(&mut lane.storage, 10);
    assert_eq!(sequenced.len(), 2);
    assert!(matches!(
        &sequenced[0].context,
        L2TxContext::Direct {
            tx: Direct { block: 4, .. },
            ..
        }
    ));
    assert!(matches!(
        &sequenced[1].context,
        L2TxContext::UserOp {
            tx: UserOpTx { block: 5, .. },
            ..
        }
    ));

    lane.storage
        .append_safe_inputs(32, &[], Address::repeat_byte(0xff))
        .expect("jump safe head");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("collapse jump to one frame");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 5, 32]);

    lane.storage
        .append_safe_inputs(36, &[], Address::repeat_byte(0xff))
        .expect("advance below reset threshold");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("observe block 36");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 5, 32]);

    lane.storage
        .append_safe_inputs(37, &[], Address::repeat_byte(0xff))
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
        .append_safe_inputs(0, &[], SENDER_A)
        .expect("seed observed safe head");
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    let (_tx, rx) = mpsc::channel(1);
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: StubApp::default(),
        storage,
        config: default_test_config(),
    };
    let mut state = LaneState::new(0, head);
    let mut safe_inputs = Vec::new();

    lane.storage
        .append_safe_inputs(4, &[], SENDER_A)
        .expect("advance below threshold");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("observe block 4");
    lane.storage
        .close_frame_and_batch(&mut state.head)
        .expect("create successor batch frame");
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 0]);

    lane.storage
        .append_safe_inputs(5, &[], SENDER_A)
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
        .append_safe_inputs(0, &[], SENDER_A)
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
        app: StubApp::default(),
        storage,
        config: default_test_config(),
    };
    let mut state = LaneState::new(0, head);
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
        .append_included_user_ops(&mut head, &[user_op_a, user_op_b])
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
        )
        .expect("append first direct input");
    storage
        .close_frame_only_with_executions(
            &mut head,
            10,
            SafeInputRange::new(0, 1),
            &[DirectInputExecution {
                safe_input_index: 0,
                l2_tx_index: L2TxCount::new(2),
            }],
        )
        .expect("close first frame with direct attribution");

    let user_op_c = make_included_user_op(0x53, 3);
    storage
        .append_included_user_ops(&mut head, &[user_op_c])
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
        )
        .expect("append second direct input");
    storage
        .close_frame_only_with_executions(
            &mut head,
            20,
            SafeInputRange::new(1, 2),
            &[DirectInputExecution {
                safe_input_index: 1,
                l2_tx_index: L2TxCount::new(4),
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
        )
        .expect("append third direct input");
    storage
        .close_frame_only_with_executions(
            &mut head,
            30,
            SafeInputRange::new(2, 3),
            &[DirectInputExecution {
                safe_input_index: 2,
                l2_tx_index: L2TxCount::new(5),
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
            input_index: 0,
            sender: Address::ZERO,
            block_number: 10,
            timestamp: 0,
            payload: vec![0xaa],
        },
        ReplayEvent::UserOp {
            sender: Address::from_slice(&[0x53; 20]),
            data: vec![0x53; 4],
        },
        ReplayEvent::DirectInput {
            input_index: 1,
            sender: Address::ZERO,
            block_number: 20,
            timestamp: 0,
            payload: vec![0xbb],
        },
        ReplayEvent::DirectInput {
            input_index: 2,
            sender: Address::ZERO,
            block_number: 30,
            timestamp: 0,
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

fn read_frame_direct_count(db_path: &str, batch_nonce: i64, frame_in_batch: i64) -> i64 {
    let conn = Storage::open_connection(db_path).expect("open sqlite reader");
    conn.query_row(
        "SELECT COUNT(*) FROM application_inputs
         WHERE batch_nonce = ?1
           AND frame_in_batch = ?2
           AND safe_input_index IS NOT NULL",
        params![batch_nonce, frame_in_batch],
        |row| row.get(0),
    )
    .expect("query frame direct count")
}

fn read_frame_safe_blocks(db_path: &str) -> Vec<u64> {
    let conn = Storage::open_connection(db_path).expect("open sqlite reader");
    let mut statement = conn
        .prepare(
            "SELECT safe_block FROM frames \
             ORDER BY batch_nonce ASC, frame_in_batch ASC",
        )
        .expect("prepare frame-clock query");
    statement
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("query frame clocks")
        .map(|value| u64::try_from(value.expect("read frame clock")).expect("nonnegative clock"))
        .collect()
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
            Address::repeat_byte(0xff),
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
    pin_test_deployment_identity(&mut storage, batch_submitter_address);
    storage
        .append_safe_inputs(0, &[], batch_submitter_address)
        .expect("seed observed safe head");
    let config = default_test_config();
    {
        let mut app = StubApp::default();
        register_genesis_snapshot(&mut app, &mut storage, &config.dumps_dir);
    }
    storage.ensure_open_tip().expect("establish genesis tip");
    let shutdown = RuntimeScope::default();
    let (_tx, lane_handle) =
        InclusionLane::<StubApp>::start(128, shutdown.clone(), storage, config);
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
        )
        .expect("append safe batch-submitter input");

    let drained = wait_until(Duration::from_secs(2), || {
        read_frame_safe_blocks(db.path.as_str()).last() == Some(&10)
    })
    .await;
    shutdown_lane(&shutdown, lane_handle).await;
    assert!(
        drained,
        "the frame must account for the observed L1 interval"
    );
    let mut storage = Storage::open(db.path.as_str()).unwrap();
    assert!(history_rows(&mut storage, 16).is_empty());
    assert_eq!(read_frame_direct_count(db.path.as_str(), 0, 1), 0);
    let mut fresh_app = StubApp::default();
    catch_up_application_paged(&mut fresh_app, &mut storage, L2TxCount::ZERO, 16)
        .expect("catch up");
    assert_eq!(
        fresh_app.l2_tx_count().get(),
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
        .append_safe_inputs(10, directs.as_slice(), Address::repeat_byte(0xff))
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
            Address::repeat_byte(0xff),
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

    let replay: Vec<L2Tx> = {
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        history_rows(&mut storage, 1_000_000)
            .into_iter()
            .map(|row| context_tx(row.context))
            .collect()
    };
    shutdown_lane(&shutdown, lane_handle).await;

    assert!(ack.is_ok(), "user op should be included");
    assert_eq!(replay.len(), 2);
    assert!(matches!(
        replay.first(),
        Some(L2Tx::Direct(direct)) if direct.payload.as_slice() == [0xaa]
    ));
    assert!(matches!(
        replay.get(1),
        Some(L2Tx::UserOp(user_op)) if user_op.data.as_slice() == [0x31, 0x31, 0x31, 0x31]
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
        set_one_op_batch_target(&mut storage);
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
    WriteHeadParts {
        batch_created_at: SystemTime::now(),
        max_batch_user_op_bytes: u64::MAX,
        ..WriteHeadParts::ZERO
    }
    .build()
}

#[test]
fn dequeue_returns_channel_closed_when_disconnected() {
    let (tx, mut rx) = mpsc::channel::<PendingUserOp>(1);
    drop(tx);
    let mut app = StubApp::default();
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

    let mut app = StubApp::default();
    let mut included = Vec::new();
    let head = unbounded_head();
    dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 16, &head, &mut included)
        .expect("should flush processed user ops before disconnect");
    assert_eq!(included.len(), 1);
}

#[test]
fn dequeue_returns_lane_error_when_app_reports_internal() {
    for fail_validation in [false, true] {
        let (tx, mut rx) = mpsc::channel::<PendingUserOp>(1);
        let (pending, recv) = make_pending_user_op(0x45);
        tx.blocking_send(pending).expect("enqueue pending user op");

        let mut app = StubApp {
            fail_validation,
            fail_apply: true,
            ..StubApp::default()
        };
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
}

#[test]
fn catch_up_replays_multiple_pages() {
    let db = temp_db("catch-up-multi-page");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mappings: Vec<_> = history_rows(&mut storage, 16)
        .into_iter()
        .map(|row| row.offset.get())
        .collect();
    assert_eq!(mappings, vec![0, 1, 2, 3, 4, 5]);
    let mut app = StubApp::default();

    catch_up_application_paged(&mut app, &mut storage, L2TxCount::ZERO, 2)
        .expect("catch up in pages");

    // The expected events carry each direct's index, block and timestamp, so
    // they also pin what replay hands the application.
    assert_eq!(app.replayed, expected);
    assert_eq!(app.l2_tx_count().get(), expected.len() as u64);
}

#[test]
fn catch_up_rejects_history_gap_before_execution() {
    let db = temp_db("catch-up-gap");
    seed_replay_fixture(&db.path);
    let mut storage = Storage::open(&db.path).unwrap();
    let conn = Storage::open_connection(&db.path).unwrap();
    conn.execute_batch("DROP TRIGGER trg_application_inputs_not_deletable; DELETE FROM application_inputs WHERE l2_tx_count=1;").unwrap();
    let mut app = StubApp::default();
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = catch_up_application_paged(&mut app, &mut storage, L2TxCount::ZERO, 2);
    }));
    assert!(failure.is_err());
    assert!(app.replayed.is_empty());
}

#[test]
fn catch_up_rejects_wrong_snapshot_count_before_execution() {
    let db = temp_db("catch-up-wrong-mapping");
    seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut app = StubApp::with_progress(3);

    let err = catch_up_application_paged(&mut app, &mut storage, L2TxCount::ZERO, 2)
        .expect_err("mapping from a different application boundary must stop catch-up");

    assert!(matches!(
        &err,
        CatchUpError::SnapshotExecutionCountMismatch {
            application: 3,
            storage: 0
        }
    ));
    assert!(
        app.replayed.is_empty(),
        "mapping is checked before execution"
    );
    assert_eq!(app.l2_tx_count(), L2TxCount::new(3));
    assert!(InclusionLaneError::CatchUp { source: err }.is_terminal_invariant());
}

#[test]
fn catch_up_handles_mixed_user_ops_and_direct_inputs_across_page_boundary() {
    let db = temp_db("catch-up-mixed-page-boundary");
    let expected = seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut app = StubApp::default();

    catch_up_application_paged(&mut app, &mut storage, L2TxCount::ZERO, 4)
        .expect("catch up across page boundary");

    assert_eq!(app.replayed, expected);
}

#[test]
fn catch_up_load_error_reports_offset() {
    let db = temp_db("catch-up-load-error");
    let mut storage = Storage::open_writer(db.path.as_str()).expect("open raw storage");
    let mut app = StubApp::default();

    let err = catch_up_application_paged(&mut app, &mut storage, L2TxCount::ZERO, 2)
        .expect_err("catch up should fail without schema");

    assert!(matches!(err, CatchUpError::LoadReplay { offset: 0, .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_refuses_snapshot_whose_application_count_disagrees_with_storage() {
    let db = temp_db("snapshot-execution-count-mismatch");
    let config = default_test_config();
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    pin_test_deployment_identity(&mut storage, Address::repeat_byte(0xff));
    storage
        .append_safe_inputs(0, &[], Address::repeat_byte(0xff))
        .expect("seed observed safe head");

    let mut app = StubApp::with_progress(1);
    register_genesis_snapshot(&mut app, &mut storage, &config.dumps_dir);
    storage.ensure_open_tip().expect("establish genesis tip");

    let shutdown = RuntimeScope::default();
    let (_tx, handle) = InclusionLane::<StubApp>::start(1, shutdown, storage, config);
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

fn read_dump_counter(dump_dir: &Path) -> u64 {
    StubApp::from_dump(&super::dump_info::app_prefix(dump_dir))
        .expect("decode dump progress")
        .l2_tx_count()
        .get()
}

/// Restart must load the application and the catch-up cursor from the same
/// checkpoint. With a pending snapshot present (a closed batch not yet observed
/// on L1), loading the finalized snapshot's state but replaying from the pending
/// snapshot's offset would silently drop the pending batch's txs.
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
        set_one_op_batch_target(&mut storage);
        storage
            .append_safe_inputs(0, &[], SENDER_A)
            .expect("seed observed safe head");
    }

    // Generation 1: no idle closes (Duration::MAX), so the latest pending is
    // always the last user-op batch with a correct global offset.
    let mut config1 = default_test_config();
    config1.max_batch_open = Duration::MAX;

    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    // `app` only writes the genesis dump here; the lane reloads its own
    // instance via `from_dump` on its background thread.
    let mut app = StubApp::default();
    register_genesis_snapshot(&mut app, &mut storage, &config1.dumps_dir);
    storage.ensure_open_tip().expect("establish genesis tip");
    let shutdown1 = RuntimeScope::default();
    let (tx1, handle1) =
        InclusionLane::<StubApp>::start(128, shutdown1.clone(), storage, config1.clone());
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
        s.latest_snapshot()
            .expect("read pending")
            .map(|p| read_dump_counter(&p.prefix) == 3)
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
                .snapshot
                .l2_tx_count
                .get(),
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
    let (tx2, handle2) = InclusionLane::<StubApp>::start(128, shutdown2.clone(), storage2, config2);

    let (pending, recv) = make_pending_user_op(0x20);
    tx2.send(pending).await.expect("send gen-2 op");
    tokio::time::timeout(Duration::from_secs(2), recv)
        .await
        .expect("gen-2 ack timeout")
        .expect("gen-2 ack channel")
        .expect("gen-2 op included");

    let reached_four = wait_until(Duration::from_secs(2), || {
        let mut s = Storage::open(db.path.as_str()).expect("open");
        s.latest_snapshot()
            .expect("read pending")
            .map(|p| read_dump_counter(&p.prefix) == 4)
            .unwrap_or(false)
    })
    .await;
    let observed = {
        let mut s = Storage::open(db.path.as_str()).expect("open");
        s.latest_snapshot()
            .expect("read pending")
            .map(|p| read_dump_counter(&p.prefix))
    };
    shutdown_lane(&shutdown2, handle2).await;

    assert!(
        reached_four,
        "after restart the snapshot counter should reach 4 (resumed 3 + 1 new op); observed {observed:?}. \
         A counter of 1 means the lane loaded genesis (0) and skipped the pending batch's txs."
    );
}

#[test]
fn application_dump_failure_leaves_tip_and_latest_snapshot_unchanged() {
    let db = temp_db("application-dump-failure");
    let mut storage = Storage::open(&db.path).unwrap();
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    let dumps = tempfile::tempdir().unwrap();
    let mut app = StubApp::default();
    register_genesis_snapshot(&mut app, &mut storage, dumps.path());
    super::snapshot::close_batch_with_snapshot(&mut app, &mut storage, &mut head, dumps.path())
        .unwrap();
    let snapshot = storage.latest_snapshot().unwrap().unwrap();
    let tip = head.batch_nonce();
    let dump_count = storage.list_snapshots().unwrap().len();

    app.fail_dump = true;
    let error =
        super::snapshot::close_batch_with_snapshot(&mut app, &mut storage, &mut head, dumps.path())
            .expect_err("an application dump failure must precede the batch seal");
    assert!(matches!(
        error,
        super::snapshot::TakeDumpError::CreateDump(super::dump_info::CreateDumpDirError::App(
            AppError::Io(ref source)
        )) if source.to_string() == "injected application dump failure"
    ));
    assert_eq!(head.batch_nonce(), tip);
    assert_eq!(storage.open_state().unwrap().unwrap().batch_nonce(), tip);
    assert_eq!(storage.latest_batch_nonce().unwrap(), Some(tip));
    assert_eq!(storage.latest_snapshot().unwrap().unwrap(), snapshot);
    assert_eq!(storage.list_snapshots().unwrap().len(), dump_count);
}

#[test]
fn empty_batch_snapshot_preserves_application_count() {
    let db = temp_db("empty-snapshot-count");
    let expected = seed_replay_fixture(&db.path);
    let mut storage = Storage::open(&db.path).unwrap();
    let mut app = StubApp::default();
    catch_up_application_paged(&mut app, &mut storage, L2TxCount::ZERO, 2).unwrap();
    assert_eq!(app.replayed, expected);
    let mut head = storage.open_state().unwrap().unwrap();
    let dumps = tempfile::tempdir().unwrap();
    super::snapshot::close_batch_with_snapshot(&mut app, &mut storage, &mut head, dumps.path())
        .unwrap();
    super::snapshot::close_batch_with_snapshot(&mut app, &mut storage, &mut head, dumps.path())
        .unwrap();
    let snapshot = storage.latest_snapshot().unwrap().unwrap();
    assert_eq!(snapshot.l2_tx_count.get(), 6);
    let restored = StubApp::from_dump(&super::dump_info::app_prefix(&snapshot.prefix)).unwrap();
    assert_eq!(restored.l2_tx_count().get(), 6);
}

fn history_rows(storage: &mut Storage, limit: usize) -> Vec<crate::storage::ApplicationInputRow> {
    let bounds = storage.history_bounds().unwrap();
    storage
        .canonical_history_page(
            sequencer_core::history::HistoryClaim {
                era_id: bounds.era_id,
                next_input: bounds.available_from,
            },
            limit,
        )
        .unwrap()
        .rows
}

fn context_tx(context: L2TxContext) -> L2Tx {
    match context {
        L2TxContext::UserOp { tx, .. } => L2Tx::UserOp(tx),
        L2TxContext::Direct { tx, .. } => L2Tx::Direct(tx),
    }
}

/// A lane with its queue already closed and one direct input due at the first
/// frame-clock tick.
fn lane_with_closed_queue_and_due_direct(
    name: &str,
    request_shutdown_on_direct: bool,
) -> (tempfile::TempDir, InclusionLane<StubApp>, RuntimeScope) {
    let db = temp_db(name);
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    pin_test_deployment_identity(&mut storage, Address::repeat_byte(0xff));
    storage
        .append_safe_inputs(0, &[], Address::repeat_byte(0xff))
        .expect("seed observed safe head");
    storage.ensure_open_tip().expect("establish open tip");
    storage
        .append_safe_inputs(
            5,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 5,
            }],
            Address::repeat_byte(0xff),
        )
        .expect("append a direct at the first clock tick");

    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    let shutdown = RuntimeScope::default();
    let lane = InclusionLane {
        rx,
        shutdown: shutdown.clone(),
        app: StubApp {
            shutdown_on_direct: request_shutdown_on_direct.then(|| shutdown.clone()),
            ..StubApp::default()
        },
        storage,
        config: default_test_config(),
    };
    (db._dir, lane, shutdown)
}

#[test]
fn lane_stops_cleanly_when_its_queue_closes_during_shutdown() {
    let (_dir, mut lane, shutdown) =
        lane_with_closed_queue_and_due_direct("closed-queue-during-shutdown", true);

    let result = lane.run_forever(L2TxCount::ZERO);

    assert!(shutdown.is_shutdown_requested());
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn lane_reports_a_closed_queue_without_shutdown() {
    let (_dir, mut lane, _shutdown) =
        lane_with_closed_queue_and_due_direct("closed-queue-without-shutdown", false);

    let result = lane.run_forever(L2TxCount::ZERO);

    assert!(matches!(result, Err(InclusionLaneError::ChannelClosed)));
}

/// Run one queued op through a dequeue chunk whose frame price is 10 and
/// return the 422 text it was refused with. Nothing may execute.
fn refusal_text(app: &mut StubApp, user_op: UserOp) -> String {
    let (tx, mut rx) = mpsc::channel::<PendingUserOp>(1);
    let (mut pending, mut response) = make_pending_user_op(0x61);
    pending.signed.user_op = user_op;
    tx.try_send(pending).expect("enqueue refused op");
    let head = WriteHeadParts {
        frame_fee: 10,
        ..WriteHeadParts::of(&unbounded_head())
    }
    .build();
    let count_before = app.l2_tx_count();
    let mut included = Vec::new();

    dequeue_and_execute_user_op_chunk(&mut rx, app, 1, &head, &mut included)
        .expect("a refusal never stops the lane");

    assert!(included.is_empty(), "a refused op is never persisted");
    assert_eq!(app.l2_tx_count(), count_before);
    assert!(
        app.replayed.is_empty(),
        "a refused op never reaches the app"
    );
    match response.try_recv() {
        Ok(Err(SequencerError::Invalid(message))) => message,
        other => panic!("expected a 422 refusal, got {other:?}"),
    }
}

#[test]
fn lane_refuses_with_each_refusal_text_in_check_order() {
    let sender = Address::from_slice(&[0x61; 20]);
    let op = |nonce: u32, max_fee: u16| UserOp {
        nonce,
        max_fee,
        data: vec![0x61; 4].into(),
    };
    let refusing = || StubApp {
        refuse_user_ops: Some("sender is frozen by policy".to_owned()),
        ..StubApp::default()
    };

    // Fee cap first: it outranks a wrong nonce and a policy refusal.
    assert_eq!(
        refusal_text(&mut refusing(), op(3, 9)),
        "max fee 9 below base fee 10"
    );
    // Then the executor's nonce rule, ahead of admission.
    assert_eq!(
        refusal_text(&mut refusing(), op(3, 10)),
        "bad nonce: expected 0, got 3"
    );
    let mut exhausted = refusing();
    exhausted.nonces.insert(sender, u32::MAX);
    assert_eq!(
        refusal_text(&mut exhausted, op(u32::MAX, 10)),
        "nonce 4294967295 has no successor"
    );
    // Admission last, its text verbatim.
    assert_eq!(
        refusal_text(&mut refusing(), op(0, 10)),
        "sender is frozen by policy"
    );
}

/// Reports every sender one nonce higher after its first read, so the
/// executor disagrees with the lane's pre-check about an op it admitted.
#[derive(Default)]
struct NonceDriftApp {
    inner: StubApp,
    reads: std::cell::Cell<u32>,
}

impl Application for NonceDriftApp {
    fn user_nonce(&self, sender: Address) -> Result<u32, AppError> {
        let drift = self.reads.get().min(1);
        self.reads.set(self.reads.get().saturating_add(1));
        Ok(self.inner.user_nonce(sender)? + drift)
    }

    fn apply_user_op(&mut self, op: &UserOpTx) -> Result<AppOutputs, AppError> {
        self.inner.apply_user_op(op)
    }

    fn apply_direct(&mut self, direct: &Direct) -> Result<AppOutputs, AppError> {
        self.inner.apply_direct(direct)
    }

    fn l2_tx_count(&self) -> L2TxCount {
        self.inner.l2_tx_count()
    }

    fn max_method_payload_bytes() -> usize {
        StubApp::max_method_payload_bytes()
    }

    fn scheduler_limits() -> SchedulerLimits {
        StubApp::scheduler_limits()
    }

    fn admit_user_op(&self, sender: Address, op: &UserOp, fee: u16) -> Result<Admission, AppError> {
        self.inner.admit_user_op(sender, op, fee)
    }

    fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
        unreachable!("the drift double is never restored")
    }

    fn create_dump(&mut self, _prefix: &Path) -> Result<(), AppError> {
        unreachable!("the drift double is never dumped")
    }

    fn state_file_in_dump(prefix: &Path) -> std::path::PathBuf {
        StubApp::state_file_in_dump(prefix)
    }

    fn from_canonical_state(_state_file: &Path, _l2_tx_count: L2TxCount) -> Result<Self, AppError> {
        unreachable!("the drift double is never restored")
    }
}

#[test]
fn admitted_op_that_the_executor_skips_is_a_terminal_lane_error() {
    let (tx, mut rx) = mpsc::channel::<PendingUserOp>(2);
    let (pending, response) = make_pending_user_op(0x62);
    tx.blocking_send(pending).expect("enqueue admitted op");
    let (later, mut later_response) = make_pending_user_op(0x63);
    tx.blocking_send(later).expect("enqueue later op");
    let mut app = NonceDriftApp::default();
    let mut included = Vec::new();

    let err =
        dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 16, &unbounded_head(), &mut included)
            .expect_err("a skip after admission must stop the lane");

    assert!(matches!(
        err,
        InclusionLaneError::AdmittedOpSkipped {
            reason: SkipReason::WrongNonce {
                expected: 1,
                got: 0
            }
        }
    ));
    assert!(err.is_terminal_invariant());
    assert!(included.is_empty());
    assert_eq!(app.l2_tx_count(), L2TxCount::ZERO);
    // The client gets the fixed 500 text, never the skip reason.
    let response = response
        .blocking_recv()
        .expect("lane answers the skipped op")
        .expect_err("skipped op gets an internal error");
    assert!(matches!(
        response,
        SequencerError::Internal(message) if message == "application internal error"
    ));
    assert!(
        matches!(
            later_response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "the chunk stops at the faulting op"
    );
}

#[test]
fn catch_up_stops_on_a_stored_op_the_application_would_skip() {
    let db = temp_db("catch-up-replay-skipped");
    seed_replay_fixture(db.path.as_str());
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    let mut app = StubApp::default();
    // The restored application already moved the first sender past its
    // stored op, so history and application disagree.
    app.nonces.insert(Address::from_slice(&[0x51; 20]), 1);

    let err = catch_up_application_paged(&mut app, &mut storage, L2TxCount::ZERO, 2)
        .expect_err("a replayed skip must stop catch-up");

    assert!(matches!(
        err,
        CatchUpError::ReplaySkipped {
            offset: 0,
            reason: SkipReason::WrongNonce {
                expected: 1,
                got: 0
            }
        }
    ));
    assert!(app.replayed.is_empty());
    assert_eq!(app.l2_tx_count(), L2TxCount::ZERO);
    assert!(InclusionLaneError::CatchUp { source: err }.is_terminal_invariant());
}

fn ingested_direct(block: u64, timestamp: u64, payload: Vec<u8>) -> IngestedSafeInput {
    IngestedSafeInput {
        sender: Address::repeat_byte(0x77),
        payload,
        block_number: block,
        block_timestamp: timestamp,
        transaction_hash: B256::ZERO,
    }
}

#[test]
fn direct_over_the_payload_cap_never_executes_and_later_directs_do() {
    let cap = usize::try_from(StubApp::scheduler_limits().max_direct_payload_bytes)
        .expect("payload cap fits usize");
    let db = temp_db("direct-payload-cap");
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
    pin_test_deployment_identity(&mut storage, Address::repeat_byte(0xff));
    storage
        .append_safe_inputs(0, &[], Address::repeat_byte(0xff))
        .expect("seed observed safe head");
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    let (_tx, rx) = mpsc::channel(1);
    let mut lane = InclusionLane {
        rx,
        shutdown: RuntimeScope::default(),
        app: StubApp::default(),
        storage,
        config: default_test_config(),
    };
    let mut state = LaneState::new(0, head);
    let mut safe_inputs = Vec::new();

    lane.storage
        .append_ingested_safe_inputs_with_timestamp(
            5,
            1_005,
            &[
                ingested_direct(3, 1_003, vec![0xa1; cap]),
                ingested_direct(4, 1_004, vec![0xee; cap + 1]),
                ingested_direct(5, 1_005, vec![0xa3]),
            ],
        )
        .expect("append directs around the payload cap");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("drain the range and close the frame");

    let at_cap = ReplayEvent::DirectInput {
        input_index: 0,
        sender: Address::repeat_byte(0x77),
        block_number: 3,
        timestamp: 1_003,
        payload: vec![0xa1; cap],
    };
    let after_dropped = ReplayEvent::DirectInput {
        input_index: 2,
        sender: Address::repeat_byte(0x77),
        block_number: 5,
        timestamp: 1_005,
        payload: vec![0xa3],
    };
    // Each direct reaches the application with its own L1 index and block
    // timestamp; the over-cap one never reaches it at all.
    assert_eq!(
        lane.app.replayed,
        vec![at_cap.clone(), after_dropped.clone()]
    );
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 5]);
    assert_eq!(read_frame_direct_count(db.path.as_str(), 0, 1), 2);
    assert_eq!(lane.storage.next_undrained_safe_input_index().unwrap(), 3);

    // A later frame keeps executing directs at the next contiguous offset.
    lane.storage
        .append_ingested_safe_inputs_with_timestamp(
            10,
            1_010,
            &[ingested_direct(10, 1_010, vec![0xb1])],
        )
        .expect("append a later direct");
    lane.maybe_advance_safe_frontier(&mut state, &mut safe_inputs)
        .expect("drain the later direct");
    let later = ReplayEvent::DirectInput {
        input_index: 3,
        sender: Address::repeat_byte(0x77),
        block_number: 10,
        timestamp: 1_010,
        payload: vec![0xb1],
    };
    assert_eq!(read_frame_safe_blocks(db.path.as_str()), vec![0, 5, 10]);
    let rows = history_rows(&mut lane.storage, 16);
    let stored: Vec<_> = rows
        .iter()
        .map(|row| match &row.context {
            L2TxContext::Direct { tx, .. } => (row.offset.get(), tx.input_index),
            L2TxContext::UserOp { .. } => panic!("no user op was submitted"),
        })
        .collect();
    assert_eq!(stored, vec![(0, 0), (1, 2), (2, 3)]);

    // Replay hands a restarted application the same directs, index and
    // timestamp included.
    let mut restarted = StubApp::default();
    catch_up_application_paged(&mut restarted, &mut lane.storage, L2TxCount::ZERO, 2)
        .expect("catch up over the dropped direct");
    assert_eq!(restarted.replayed, vec![at_cap, after_dropped, later]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_in_a_rebuilt_era_drains_from_the_eras_first_queued_input() {
    use crate::egress::test_era;

    let db = temp_db("lane-rebuilt-era");
    let config = default_test_config();
    let baseline = config.dumps_dir.join("era-baseline");
    super::dump_info::create_dump_dir_with_info(
        &mut StubApp::with_progress(test_era::REBUILT_END),
        &baseline,
        &super::dump_info::DumpInfo::at_baseline(test_era::NEXT_BATCH_NONCE),
    )
    .expect("create the era's baseline dump");
    let mut storage = test_era::publish_rebuilt_era(&db.path, &baseline);
    // Input 3 sits at the stop block but is still queued, so the drain starts
    // there, not after the stop block's last input.
    assert_eq!(
        storage.next_undrained_safe_input_index().unwrap(),
        test_era::FIRST_QUEUED_INPUT
    );
    // `run`'s startup: the root frame drains the queued direct.
    storage.ensure_open_tip().expect("open the root frame");
    assert_eq!(
        storage.next_undrained_safe_input_index().unwrap(),
        test_era::FIRST_QUEUED_INPUT + 1
    );

    let shutdown = RuntimeScope::default();
    let (_tx, handle) = InclusionLane::<StubApp>::start(16, shutdown.clone(), storage, config);
    let later = IngestedSafeInput {
        sender: test_era::DIRECT_SENDER,
        payload: vec![0xb4],
        block_number: test_era::STOP_BLOCK + 6,
        block_timestamp: 1_016,
        transaction_hash: B256::repeat_byte(0xb4),
    };
    Storage::open(&db.path)
        .expect("open feeder storage")
        .append_ingested_safe_inputs_with_timestamp(test_era::STOP_BLOCK + 6, 1_016, &[later])
        .expect("land a direct after the root frame");
    let head = |db_path: &str| {
        Storage::open_read_only(db_path)
            .unwrap()
            .history_bounds()
            .unwrap()
            .head
            .get()
    };
    let drained = wait_until(Duration::from_secs(2), || {
        head(&db.path) == test_era::REBUILT_END + 2
    })
    .await;
    // A clean stop means catch-up replayed the queued direct onto the
    // baseline, so the lane's own direct executed at the next count.
    shutdown_lane(&shutdown, handle).await;
    assert!(drained, "the lane must drain the later direct");

    let mut storage = Storage::open_read_only(&db.path).unwrap();
    let bounds = storage.history_bounds().unwrap();
    let live: Vec<_> = storage
        .canonical_history_page(
            sequencer_core::history::HistoryClaim {
                era_id: bounds.era_id,
                next_input: L2TxCount::new(test_era::REBUILT_END),
            },
            16,
        )
        .unwrap()
        .rows
        .into_iter()
        .map(|row| match row.context {
            L2TxContext::Direct { tx, .. } => (row.offset.get(), tx.input_index),
            L2TxContext::UserOp { .. } => panic!("no user op was submitted"),
        })
        .collect();
    assert_eq!(
        live,
        vec![
            (test_era::REBUILT_END, test_era::FIRST_QUEUED_INPUT),
            (test_era::REBUILT_END + 1, test_era::FIRST_QUEUED_INPUT + 1),
        ]
    );
}
