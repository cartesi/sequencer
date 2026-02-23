// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::application::{AppError, Application, ExecutionOutcome};
use crate::l2_tx::SequencedL2Tx;
use crate::storage::{IndexedDirectInput, Storage, WriteHead};
use crate::user_op::SignedUserOp;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::error::CatchUpError;
use super::{InclusionLaneError, InclusionLaneInput, PendingUserOp, SequencerError};

#[derive(Debug, Clone, Copy)]
pub struct InclusionLaneConfig {
    pub max_user_ops_per_chunk: usize,
    pub safe_direct_buffer_capacity: usize,
    pub max_batch_open: Duration,

    // Soft threshold for batch rotation.
    //
    // We intentionally check this between chunks (not per user-op) to keep the hot path
    // simple and low-latency. This means batches can overshoot the threshold by at most
    // one processed chunk. API ingress bounds each user-op size, so this overshoot is
    // bounded by:
    //   max_user_ops_per_chunk * SignedUserOp::max_batch_bytes_upper_bound()
    pub max_batch_user_op_bytes: usize,

    pub idle_poll_interval: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct InclusionLaneStop {
    shutdown: Arc<AtomicBool>,
}

impl InclusionLaneStop {
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}

pub struct InclusionLane<A: Application + 'static> {
    rx: mpsc::Receiver<InclusionLaneInput>,
    stop: InclusionLaneStop,
    app: A,
    storage: Storage,
    config: InclusionLaneConfig,
}

impl<A: Application + 'static> InclusionLane<A> {
    pub fn new(
        rx: mpsc::Receiver<InclusionLaneInput>,
        app: A,
        storage: Storage,
        config: InclusionLaneConfig,
    ) -> Self {
        Self {
            rx,
            stop: InclusionLaneStop::default(),
            app,
            storage,
            config,
        }
    }

    pub fn spawn(self) -> (JoinHandle<InclusionLaneError>, InclusionLaneStop) {
        let stop = self.stop.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let mut lane = self;
            match lane.run_forever() {
                Err(err) => err,
                Ok(()) => unreachable!("inclusion lane run loop is expected to be non-terminating"),
            }
        });
        (handle, stop)
    }

    fn run_forever(&mut self) -> Result<(), InclusionLaneError> {
        self.run_catch_up()?;
        let (mut next_safe_input_index, mut head) = self.load_lane_state()?;

        let mut included = Vec::with_capacity(self.config.max_user_ops_per_chunk.max(1));
        let mut safe_directs = Vec::with_capacity(self.config.safe_direct_buffer_capacity.max(1));

        while !self.stop.is_shutdown_requested() {
            // Canonical per-iteration order: include user-ops first, then drain direct inputs.
            let included_user_op_count = self.process_user_op_chunk(&mut head, &mut included)?;
            let drained_safe_direct_count = self.drain_and_execute_safe_direct_inputs(
                &mut next_safe_input_index,
                &mut safe_directs,
            )?;

            if head.should_close_batch(&self.config) {
                self.close_frame_and_batch(&mut head, drained_safe_direct_count)?;
            } else if drained_safe_direct_count > 0 {
                self.close_frame_only(&mut head, drained_safe_direct_count)?;
            }

            if included_user_op_count == 0 && drained_safe_direct_count == 0 {
                thread::sleep(self.config.idle_poll_interval);
            }

            safe_directs.clear();
        }

        Err(InclusionLaneError::ShutdownRequested)
    }

    fn run_catch_up(&mut self) -> Result<(), InclusionLaneError> {
        catch_up_application(&mut self.app, &mut self.storage)
            .map_err(|source| InclusionLaneError::CatchUp { source })
    }

    fn load_lane_state(&mut self) -> Result<(u64, WriteHead), InclusionLaneError> {
        let next_safe_input_index = self
            .storage
            .load_next_undrained_direct_input_index()
            .map_err(|source| InclusionLaneError::LoadNextUndrainedDirectInputIndex { source })?;

        let head = self
            .storage
            .load_open_state()
            .map_err(|source| InclusionLaneError::LoadOpenState { source })?;

        Ok((next_safe_input_index, head))
    }

    fn process_user_op_chunk(
        &mut self,
        head: &mut WriteHead,
        included: &mut Vec<PendingUserOp>,
    ) -> Result<usize, InclusionLaneError> {
        dequeue_and_execute_user_op_chunk(
            &mut self.rx,
            &mut self.app,
            head.batch_fee,
            self.config.max_user_ops_per_chunk.max(1),
            included,
        )?;
        let included_count = included.len();

        self.persist_included_user_ops(head, included)?;
        for item in included.drain(..) {
            let _ = item.respond_to.send(Ok(()));
        }

        Ok(included_count)
    }

    fn persist_included_user_ops(
        &mut self,
        head: &mut WriteHead,
        included: &mut Vec<PendingUserOp>,
    ) -> Result<(), InclusionLaneError> {
        self.storage
            .append_user_ops_chunk(head, included.as_slice())
            .map_err(|source| {
                Self::respond_internal_to_all(included, format!("db error: {source}"));
                InclusionLaneError::AppendUserOps { source }
            })
    }

    fn drain_and_execute_safe_direct_inputs(
        &mut self,
        next_safe_input_index: &mut u64,
        chunk: &mut Vec<IndexedDirectInput>,
    ) -> Result<usize, InclusionLaneError> {
        let safe_end = self
            .storage
            .safe_input_end_exclusive()
            .map_err(|source| InclusionLaneError::LoadSafeDirectInputs { source })?;
        assert!(
            safe_end >= *next_safe_input_index,
            "safe direct-input head regressed: safe_end={safe_end}, next={next_safe_input_index}"
        );

        let mut drained_total = 0_usize;
        let max_chunk_len = self.config.safe_direct_buffer_capacity.max(1) as u64;
        while *next_safe_input_index < safe_end {
            let chunk_end = safe_end.min((*next_safe_input_index).saturating_add(max_chunk_len));
            chunk.clear();

            self.storage
                .fill_safe_inputs(*next_safe_input_index, chunk_end, chunk)
                .map_err(|source| InclusionLaneError::LoadSafeDirectInputs { source })?;

            for input in chunk.iter() {
                self.app
                    .execute_direct_input(input.payload.as_slice())
                    .map_err(|source| InclusionLaneError::ExecuteDirectInput { source })?;
            }

            drained_total = drained_total.saturating_add(chunk.len());
            *next_safe_input_index = chunk_end;
        }

        Ok(drained_total)
    }

    fn close_frame_and_batch(
        &mut self,
        head: &mut WriteHead,
        drained_direct_count: usize,
    ) -> Result<(), InclusionLaneError> {
        self.storage
            .close_frame_and_batch(head, drained_direct_count)
            .map_err(|source| InclusionLaneError::CloseFrameRotate { source })
    }

    fn close_frame_only(
        &mut self,
        head: &mut WriteHead,
        drained_direct_count: usize,
    ) -> Result<(), InclusionLaneError> {
        self.storage
            .close_frame_only(head, drained_direct_count)
            .map_err(|source| InclusionLaneError::CloseFrameRotate { source })
    }

    fn respond_internal_to_all(pending: &mut Vec<PendingUserOp>, message: String) {
        for item in pending.drain(..) {
            let _ = item
                .respond_to
                .send(Err(SequencerError::internal(message.clone())));
        }
    }
}

impl WriteHead {
    fn should_close_batch_by_time(&self, config: &InclusionLaneConfig) -> bool {
        let age = SystemTime::now()
            .duration_since(self.batch_created_at)
            .unwrap_or_default();
        age >= config.max_batch_open
    }

    fn should_close_batch_by_size(&self, config: &InclusionLaneConfig) -> bool {
        // This is computed from committed user-op count only. Because checks happen at chunk
        // boundaries, byte-based closure is a bounded soft policy, not a strict hard cutoff.
        user_op_count_to_bytes(self.batch_user_op_count) >= config.max_batch_user_op_bytes as u64
    }

    fn should_close_batch(&self, config: &InclusionLaneConfig) -> bool {
        let batch_has_activity = self.frame_in_batch > 0 || self.batch_user_op_count > 0;
        batch_has_activity
            && (self.should_close_batch_by_time(config) || self.should_close_batch_by_size(config))
    }
}

fn execute_user_op(
    app: &mut impl Application,
    item: PendingUserOp,
    current_batch_fee: u64,
    included: &mut Vec<PendingUserOp>,
) {
    match app.validate_and_execute_user_op(
        item.signed.sender,
        &item.signed.user_op,
        current_batch_fee,
    ) {
        Ok(ExecutionOutcome::Included) => included.push(item),
        Ok(ExecutionOutcome::Invalid(reason)) => {
            let _ = item
                .respond_to
                .send(Err(SequencerError::invalid(reason.to_string())));
        }
        Err(AppError::Internal { reason }) => {
            let _ = item.respond_to.send(Err(SequencerError::internal(reason)));
        }
    }
}

fn dequeue_and_execute_user_op_chunk(
    rx: &mut mpsc::Receiver<InclusionLaneInput>,
    app: &mut impl Application,
    current_batch_fee: u64,
    max_chunk: usize,
    included: &mut Vec<PendingUserOp>,
) -> Result<(), InclusionLaneError> {
    let mut executed_user_ops = 0_usize;

    while executed_user_ops < max_chunk {
        match rx.try_recv() {
            Ok(InclusionLaneInput::UserOp(item)) => {
                execute_user_op(app, item, current_batch_fee, included);
                executed_user_ops = executed_user_ops.saturating_add(1);
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                if executed_user_ops == 0 {
                    return Err(InclusionLaneError::ChannelClosed);
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

fn catch_up_application(
    app: &mut impl Application,
    storage: &mut Storage,
) -> Result<(), CatchUpError> {
    let already_executed = app.executed_input_count();
    let replay = storage
        .load_ordered_l2_txs_from(already_executed)
        .map_err(|source| CatchUpError::LoadReplay { source })?;

    for item in replay {
        match item {
            SequencedL2Tx::UserOp(value) => {
                app.execute_valid_user_op(&value).map_err(|err| {
                    CatchUpError::ReplayUserOpInternal {
                        reason: err.to_string(),
                    }
                })?;
            }
            SequencedL2Tx::Direct(direct) => {
                app.execute_direct_input(direct.payload.as_slice())
                    .map_err(|err| CatchUpError::ReplayDirectInputInternal {
                        reason: err.to_string(),
                    })?;
            }
        }
    }

    Ok(())
}

fn user_op_count_to_bytes(user_op_count: u64) -> u64 {
    user_op_count.saturating_mul(SignedUserOp::max_batch_bytes_upper_bound() as u64)
}

#[cfg(test)]
mod tests {
    use super::{
        InclusionLane, InclusionLaneConfig, InclusionLaneError, InclusionLaneInput,
        InclusionLaneStop, PendingUserOp, dequeue_and_execute_user_op_chunk,
    };
    use crate::application::{AppError, Application, InvalidReason};
    use crate::l2_tx::ValidUserOp;
    use crate::storage::Storage;
    use crate::user_op::{SignedUserOp, UserOp};
    use alloy_primitives::{Address, B256, Signature, U256};
    use rusqlite::{OptionalExtension, params};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::{mpsc, oneshot};

    #[derive(Default)]
    struct TestApp {
        nonces: HashMap<Address, u32>,
        executed_input_count: u64,
    }

    impl Application for TestApp {
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
            _current_fee: u64,
        ) -> Result<(), InvalidReason> {
            Ok(())
        }

        fn execute_valid_user_op(&mut self, user_op: &ValidUserOp) -> Result<(), AppError> {
            let next_nonce = self.current_user_nonce(user_op.sender).wrapping_add(1);
            self.nonces.insert(user_op.sender, next_nonce);
            self.executed_input_count = self.executed_input_count.saturating_add(1);
            Ok(())
        }

        fn execute_direct_input(&mut self, _payload: &[u8]) -> Result<(), AppError> {
            self.executed_input_count = self.executed_input_count.saturating_add(1);
            Ok(())
        }

        fn executed_input_count(&self) -> u64 {
            self.executed_input_count
        }
    }

    fn temp_db_path(name: &str) -> String {
        let mut path = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        path.push(format!("sequencer-inclusion-lane-{name}-{unique}.sqlite"));
        path_to_string(path)
    }

    fn path_to_string(path: PathBuf) -> String {
        path.to_string_lossy().into_owned()
    }

    fn default_test_config() -> InclusionLaneConfig {
        InclusionLaneConfig {
            max_user_ops_per_chunk: 16,
            safe_direct_buffer_capacity: 16,
            max_batch_open: Duration::MAX,
            max_batch_user_op_bytes: 1_000_000_000,
            idle_poll_interval: Duration::from_millis(2),
        }
    }

    fn start_lane(
        db_path: &str,
        config: InclusionLaneConfig,
    ) -> (
        mpsc::Sender<InclusionLaneInput>,
        InclusionLaneStop,
        tokio::task::JoinHandle<InclusionLaneError>,
    ) {
        let storage = Storage::open(db_path, "NORMAL").expect("open storage");
        let (tx, rx) = mpsc::channel::<InclusionLaneInput>(128);
        let lane = InclusionLane::new(rx, TestApp::default(), storage, config);
        let (handle, stop) = lane.spawn();
        (tx, stop, handle)
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
            max_fee: 1,
            data: vec![seed; 4].into(),
        };
        (
            PendingUserOp {
                signed: SignedUserOp {
                    sender,
                    signature: Signature::test_signature(),
                    user_op,
                },
                tx_hash: B256::from([seed; 32]),
                respond_to,
                received_at: SystemTime::now(),
            },
            recv,
        )
    }

    fn read_count(db_path: &str, table: &str) -> i64 {
        let conn = Storage::open_connection(db_path, "NORMAL").expect("open sqlite reader");
        let sql = format!("SELECT COUNT(*) FROM {table}");
        conn.query_row(sql.as_str(), [], |row| row.get(0))
            .expect("count rows")
    }

    fn read_frame_drain(db_path: &str, batch_index: i64, frame_in_batch: i64) -> Option<i64> {
        let conn = Storage::open_connection(db_path, "NORMAL").expect("open sqlite reader");
        conn.query_row(
            "SELECT drain_n FROM frame_drains WHERE batch_index = ?1 AND frame_in_batch = ?2",
            params![batch_index, frame_in_batch],
            |row| row.get(0),
        )
        .optional()
        .expect("query frame drain")
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
        stop: &InclusionLaneStop,
        handle: tokio::task::JoinHandle<InclusionLaneError>,
    ) {
        stop.request_shutdown();
        let joined = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("wait for lane shutdown");
        let err = joined.expect("join lane task");
        assert!(matches!(err, InclusionLaneError::ShutdownRequested));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ack_happens_after_chunk_commit_without_closing_frame() {
        let db_path = temp_db_path("ack-chunk-commit");
        let (tx, lane_stop, lane_handle) = start_lane(&db_path, default_test_config());
        let (pending, recv) = make_pending_user_op(0x11);

        tx.send(InclusionLaneInput::UserOp(pending))
            .await
            .expect("send user op");
        let ack = tokio::time::timeout(Duration::from_secs(2), recv)
            .await
            .expect("wait for ack")
            .expect("ack channel open");
        let user_ops_count = read_count(&db_path, "user_ops");
        let frame_drains_count = read_count(&db_path, "frame_drains");
        shutdown_lane(&lane_stop, lane_handle).await;

        assert!(ack.is_ok(), "user op should be included");
        assert_eq!(user_ops_count, 1);
        assert_eq!(
            frame_drains_count, 0,
            "frame should stay open when no directs and no batch close"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_inputs_close_frame_and_persist_drain() {
        let db_path = temp_db_path("directs-close-frame");
        let (_tx, lane_stop, lane_handle) = start_lane(&db_path, default_test_config());
        let mut feeder_storage = Storage::open(&db_path, "NORMAL").expect("open feeder storage");

        feeder_storage
            .append_safe_direct_inputs(&[crate::storage::IndexedDirectInput {
                index: 0,
                payload: vec![0xaa],
            }])
            .expect("append safe direct input");

        let drained = wait_until(Duration::from_secs(2), || {
            read_frame_drain(&db_path, 0, 0) == Some(1)
        })
        .await;
        let frames_count = read_count(&db_path, "frames");
        shutdown_lane(&lane_stop, lane_handle).await;

        assert!(drained, "expected frame drain row with drain_n=1");
        assert_eq!(frames_count, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_inputs_are_paginated_by_buffer_capacity() {
        let db_path = temp_db_path("directs-pagination");
        let mut config = default_test_config();
        config.safe_direct_buffer_capacity = 2;
        let (_tx, lane_stop, lane_handle) = start_lane(&db_path, config);
        let mut feeder_storage = Storage::open(&db_path, "NORMAL").expect("open feeder storage");

        let mut directs = Vec::new();
        for index in 0..5_u64 {
            directs.push(crate::storage::IndexedDirectInput {
                index,
                payload: vec![index as u8],
            });
        }
        feeder_storage
            .append_safe_direct_inputs(directs.as_slice())
            .expect("append safe direct inputs");

        let drained = wait_until(Duration::from_secs(2), || {
            read_frame_drain(&db_path, 0, 0) == Some(5)
        })
        .await;
        let frames_count = read_count(&db_path, "frames");
        shutdown_lane(&lane_stop, lane_handle).await;

        assert!(drained, "expected frame drain row with drain_n=5");
        assert_eq!(frames_count, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_closes_when_max_open_time_is_reached() {
        let db_path = temp_db_path("batch-close-time");
        let mut config = default_test_config();
        config.max_batch_open = Duration::from_millis(20);
        let (tx, lane_stop, lane_handle) = start_lane(&db_path, config);
        let (pending, recv) = make_pending_user_op(0x22);

        tx.send(InclusionLaneInput::UserOp(pending))
            .await
            .expect("send user op");
        let ack = tokio::time::timeout(Duration::from_secs(2), recv)
            .await
            .expect("wait for ack")
            .expect("ack channel open");
        let rotated = wait_until(Duration::from_secs(2), || {
            read_count(&db_path, "batches") >= 2
        })
        .await;
        let drain = read_frame_drain(&db_path, 0, 0);
        shutdown_lane(&lane_stop, lane_handle).await;

        assert!(ack.is_ok(), "user op should be included");
        assert!(rotated, "expected batch rotation by time");
        assert_eq!(drain, Some(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_closes_when_max_user_op_bytes_is_reached() {
        let db_path = temp_db_path("batch-close-size");
        let mut config = default_test_config();
        config.max_batch_user_op_bytes = SignedUserOp::max_batch_bytes_upper_bound();
        let (tx, lane_stop, lane_handle) = start_lane(&db_path, config);
        let (pending, recv) = make_pending_user_op(0x33);

        tx.send(InclusionLaneInput::UserOp(pending))
            .await
            .expect("send user op");
        let ack = tokio::time::timeout(Duration::from_secs(2), recv)
            .await
            .expect("wait for ack")
            .expect("ack channel open");
        let rotated = wait_until(Duration::from_secs(2), || {
            read_count(&db_path, "batches") >= 2
        })
        .await;
        let drain = read_frame_drain(&db_path, 0, 0);
        shutdown_lane(&lane_stop, lane_handle).await;

        assert!(ack.is_ok(), "user op should be included");
        assert!(rotated, "expected batch rotation by size");
        assert_eq!(drain, Some(0));
    }

    #[test]
    fn dequeue_returns_channel_closed_when_disconnected() {
        let (tx, mut rx) = mpsc::channel::<InclusionLaneInput>(1);
        drop(tx);
        let mut app = TestApp::default();
        let mut included = Vec::new();

        let err =
            dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 1, 1, &mut included).unwrap_err();
        assert!(matches!(err, InclusionLaneError::ChannelClosed));
    }

    #[test]
    fn dequeue_flushes_executed_ops_before_observing_disconnect() {
        let (tx, mut rx) = mpsc::channel::<InclusionLaneInput>(2);
        let (pending, _recv) = make_pending_user_op(0x44);
        tx.blocking_send(InclusionLaneInput::UserOp(pending))
            .expect("enqueue pending user op");
        drop(tx);

        let mut app = TestApp::default();
        let mut included = Vec::new();
        dequeue_and_execute_user_op_chunk(&mut rx, &mut app, 1, 16, &mut included)
            .expect("should flush processed user ops before disconnect");
        assert_eq!(included.len(), 1);
    }
}
