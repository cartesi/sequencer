// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Hot-path loop. The lane runs three layers of amortization on each iteration:
//!
//! - **Frontier check** (time-gated by `frontier_min_interval`): polls L1's
//!   safe head; advances frame boundary if it moved.
//! - **Inner drain loop** (`run_inner_drain`): processes user-op chunks until
//!   the queue empties or the batch hits its size target.
//! - **Per-chunk persistence** (`max_user_ops_per_chunk`): each chunk commits
//!   in one SQL transaction, bounding ack latency for the first op in it.
//!
//! The lane is a single-thread `spawn_blocking` task. SQLite is the only
//! synchronization with other components (input reader, batch submitter).

mod catch_up;
mod config;
mod error;
mod types;

#[cfg(test)]
mod tests;

pub use config::InclusionLaneConfig;
pub use error::InclusionLaneError;
pub use types::{PendingUserOp, SequencerError};

use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{SafeInputRange, Storage, StoredSafeInput, WriteHead};
use sequencer_core::application::{AppError, Application, ExecutionOutcome};
use sequencer_core::l2_tx::DirectInput;
use sequencer_core::user_op::SignedUserOp;

use catch_up::catch_up_application;

/// Owns the application instance, the `Storage` write handle, and the user-op
/// receiver for the lifetime of the sequencer process.
pub struct InclusionLane<A: Application + 'static> {
    rx: mpsc::Receiver<PendingUserOp>,
    shutdown: ShutdownSignal,
    app: A,
    storage: Storage,
    config: InclusionLaneConfig,
}

impl<A: Application + 'static> InclusionLane<A> {
    /// Spawn the lane on a blocking thread. Returns the input MPSC sender (for
    /// the API to enqueue user ops) and the join handle (for the runtime to
    /// observe lane shutdown).
    ///
    /// The handle resolves to `Ok(())` on graceful shutdown, or an
    /// `InclusionLaneError` if the lane crashed.
    pub fn start(
        queue_capacity: usize,
        shutdown: ShutdownSignal,
        app: A,
        storage: Storage,
        config: InclusionLaneConfig,
    ) -> (
        mpsc::Sender<PendingUserOp>,
        JoinHandle<Result<(), InclusionLaneError>>,
    ) {
        let (tx, rx) = mpsc::channel::<PendingUserOp>(queue_capacity.max(1));
        let handle = tokio::task::spawn_blocking(move || {
            let mut lane = Self {
                rx,
                shutdown,
                app,
                storage,
                config,
            };
            lane.run_forever()
        });
        (tx, handle)
    }

    fn run_forever(&mut self) -> Result<(), InclusionLaneError> {
        self.run_catch_up()?;
        let mut included = Vec::with_capacity(self.config.max_user_ops_per_chunk.max(1));
        let mut safe_inputs = Vec::with_capacity(self.config.safe_input_buffer_capacity.max(1));
        let mut lane_state = self.load_or_initialize_lane_state(&mut safe_inputs)?;

        loop {
            if self.shutdown.is_shutdown_requested() {
                self.reject_pending_user_ops_due_to_shutdown();
                return Ok(());
            }

            self.maybe_advance_safe_frontier(&mut lane_state, &mut safe_inputs)?;
            let drain = self.run_inner_drain(&mut lane_state.head, &mut included)?;

            if drain.hit_batch_target()
                || should_close_batch_by_time(&lane_state.head, &self.config)
            {
                let next_safe_block = lane_state.head.safe_block;
                self.storage
                    .close_frame_and_batch(&mut lane_state.head, next_safe_block)?;
            } else if !drain.drained_any() {
                thread::sleep(self.config.idle_poll_interval);
            }
        }
    }

    fn run_catch_up(&mut self) -> Result<(), InclusionLaneError> {
        catch_up_application(
            &mut self.app,
            &mut self.storage,
            self.config.batch_submitter_address,
        )
        .map_err(|source| InclusionLaneError::CatchUp { source })
    }

    fn load_or_initialize_lane_state(
        &mut self,
        safe_inputs: &mut Vec<StoredSafeInput>,
    ) -> Result<LaneState, InclusionLaneError> {
        let next_safe_input_index = self.storage.load_next_undrained_safe_input_index()?;

        let last_drained_direct_range = SafeInputRange::empty_at(next_safe_input_index);
        if let Some(head) = self.storage.load_open_state()? {
            return Ok(LaneState::new(last_drained_direct_range, head));
        }

        let frontier = self.storage.load_safe_frontier()?;
        assert!(
            frontier.end_exclusive >= last_drained_direct_range.end(),
            "safe-input head regressed during lane initialization: safe_end={}, next={}",
            frontier.end_exclusive,
            last_drained_direct_range.end()
        );

        let leading_direct_range = last_drained_direct_range.advance_to(frontier.end_exclusive);
        self.execute_safe_inputs_range(leading_direct_range, safe_inputs)?;
        let head = self
            .storage
            .initialize_open_state(frontier.safe_block, leading_direct_range)?;

        Ok(LaneState::new(leading_direct_range, head))
    }

    /// Drain user ops in chunks until the queue empties or we cross the batch
    /// size target. Each chunk persists separately so ack latency stays bounded
    /// by `max_user_ops_per_chunk`.
    fn run_inner_drain(
        &mut self,
        head: &mut WriteHead,
        included: &mut Vec<PendingUserOp>,
    ) -> Result<DrainSummary, InclusionLaneError> {
        let mut drained_any = false;
        loop {
            let (count, outcome) = self.process_user_op_chunk(head, included)?;
            if count > 0 {
                drained_any = true;
            }
            match outcome {
                ChunkOutcome::QueueEmpty => {
                    return Ok(if drained_any {
                        DrainSummary::DrainedQueue
                    } else {
                        DrainSummary::Idle
                    });
                }
                ChunkOutcome::HitBatchTarget => return Ok(DrainSummary::HitBatchTarget),
                ChunkOutcome::MoreToProcess => continue,
            }
        }
    }

    fn process_user_op_chunk(
        &mut self,
        head: &mut WriteHead,
        included: &mut Vec<PendingUserOp>,
    ) -> Result<(usize, ChunkOutcome), InclusionLaneError> {
        included.clear();
        let outcome = dequeue_and_execute_user_op_chunk::<A>(
            &mut self.rx,
            &mut self.app,
            head.frame_fee,
            self.config.max_user_ops_per_chunk.max(1),
            head,
            included,
        )?;
        let included_count = included.len();

        self.persist_included_user_ops(head, included)?;

        for item in included.drain(..) {
            let _ = item.respond_to.send(Ok(()));
        }

        Ok((included_count, outcome))
    }

    /// Time-gated to bound idle SQL load. High-throughput batches can delay
    /// this past the gate, but a full batch is far less than 1s of work in
    /// practice.
    fn maybe_advance_safe_frontier(
        &mut self,
        lane_state: &mut LaneState,
        safe_inputs: &mut Vec<StoredSafeInput>,
    ) -> Result<(), InclusionLaneError> {
        if !lane_state.frontier_check_due(self.config.frontier_min_interval) {
            return Ok(());
        }
        lane_state.mark_frontier_checked();

        let frontier = self.storage.load_safe_frontier()?;
        assert!(
            frontier.end_exclusive >= lane_state.last_drained_direct_range.end(),
            "safe-input head regressed: safe_end={}, next={}",
            frontier.end_exclusive,
            lane_state.last_drained_direct_range.end()
        );
        if frontier.safe_block <= lane_state.head.safe_block {
            return Ok(());
        }

        let leading_direct_range = lane_state
            .last_drained_direct_range
            .advance_to(frontier.end_exclusive);
        self.execute_safe_inputs_range(leading_direct_range, safe_inputs)?;
        self.storage.close_frame_only(
            &mut lane_state.head,
            frontier.safe_block,
            leading_direct_range,
        )?;
        lane_state.last_drained_direct_range = leading_direct_range;
        Ok(())
    }

    fn persist_included_user_ops(
        &mut self,
        head: &mut WriteHead,
        included: &mut Vec<PendingUserOp>,
    ) -> Result<(), InclusionLaneError> {
        self.storage
            .append_user_ops_chunk(head, included.as_slice())
            .map_err(|err| {
                Self::respond_internal_to_all(included, "internal storage error".to_string());
                InclusionLaneError::Storage(err)
            })
    }

    fn execute_safe_inputs_range(
        &mut self,
        direct_range: SafeInputRange,
        chunk: &mut Vec<StoredSafeInput>,
    ) -> Result<(), InclusionLaneError> {
        let max_chunk_len = self.config.safe_input_buffer_capacity.max(1) as u64;
        for chunk_range in direct_range.chunks(max_chunk_len) {
            self.storage.fill_safe_inputs(chunk_range, chunk)?;
            self.execute_safe_inputs_chunk(chunk.as_slice())?;
        }
        Ok(())
    }

    fn execute_safe_inputs_chunk(
        &mut self,
        chunk: &[StoredSafeInput],
    ) -> Result<(), InclusionLaneError> {
        for input in chunk {
            if input.sender == self.config.batch_submitter_address {
                continue;
            }
            let direct_input = DirectInput {
                sender: input.sender,
                block_number: input.block_number,
                payload: input.payload.clone(),
            };

            self.app
                .execute_direct_input(&direct_input)
                .map_err(|source| InclusionLaneError::ExecuteDirectInput { source })?;
        }
        Ok(())
    }

    fn respond_internal_to_all(pending: &mut Vec<PendingUserOp>, message: String) {
        for item in pending.drain(..) {
            let _ = item
                .respond_to
                .send(Err(SequencerError::internal(message.clone())));
        }
    }

    fn reject_pending_user_ops_due_to_shutdown(&mut self) {
        while let Ok(item) = self.rx.try_recv() {
            let _ = item
                .respond_to
                .send(Err(SequencerError::unavailable("sequencer shutting down")));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DrainSummary {
    /// Queue was empty; nothing was drained this pass.
    Idle,
    /// Drained the queue, no batch close needed (size-wise).
    DrainedQueue,
    /// Drained at least one op AND crossed the batch size target.
    /// (`(false, true)` is unreachable: the size check fires only after a
    /// successful execution, so `HitBatchTarget` always implies `drained_any`.)
    HitBatchTarget,
}

impl DrainSummary {
    fn hit_batch_target(&self) -> bool {
        matches!(self, Self::HitBatchTarget)
    }

    fn drained_any(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ChunkOutcome {
    /// Queue drained or sender disconnected with at least one op processed.
    QueueEmpty,
    /// Including the latest op pushed the batch over `max_batch_user_op_bytes`.
    HitBatchTarget,
    /// Hit `max_user_ops_per_chunk` cap; queue may still have more.
    MoreToProcess,
}

fn should_close_batch_by_time(head: &WriteHead, config: &InclusionLaneConfig) -> bool {
    let age = SystemTime::now()
        .duration_since(head.batch_created_at)
        .unwrap_or_default();
    age >= config.max_batch_open
}

fn execute_user_op(
    app: &mut impl Application,
    item: PendingUserOp,
    current_frame_fee: u16,
    included: &mut Vec<PendingUserOp>,
) {
    match app.validate_and_execute_user_op(
        item.signed.sender,
        &item.signed.user_op,
        current_frame_fee,
    ) {
        Ok(ExecutionOutcome::Included { .. }) => included.push(item),
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

/// Dequeue and execute up to `max_chunk` user ops, stopping early if the batch
/// would cross its size target. Returns the outcome that drove the stop.
///
/// `head.batch_user_op_count` reflects already-persisted ops; `included.len()`
/// is the count we'd add by persisting now. When their sum's bytes equal or
/// exceed `head.max_batch_user_op_bytes`, we stop and the caller closes the
/// batch.
pub(super) fn dequeue_and_execute_user_op_chunk<A: Application>(
    rx: &mut mpsc::Receiver<PendingUserOp>,
    app: &mut A,
    current_frame_fee: u16,
    max_chunk: usize,
    head: &WriteHead,
    included: &mut Vec<PendingUserOp>,
) -> Result<ChunkOutcome, InclusionLaneError> {
    let mut executed = 0_usize;

    while executed < max_chunk {
        match rx.try_recv() {
            Ok(item) => {
                execute_user_op(app, item, current_frame_fee, included);
                executed = executed.saturating_add(1);

                let projected = head
                    .batch_user_op_count
                    .saturating_add(included.len() as u64);
                if user_op_count_to_bytes::<A>(projected) >= head.max_batch_user_op_bytes {
                    return Ok(ChunkOutcome::HitBatchTarget);
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(ChunkOutcome::QueueEmpty),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                if executed == 0 {
                    return Err(InclusionLaneError::ChannelClosed);
                }
                return Ok(ChunkOutcome::QueueEmpty);
            }
        }
    }

    Ok(ChunkOutcome::MoreToProcess)
}

fn user_op_count_to_bytes<A: Application>(user_op_count: u64) -> u64 {
    let one_user_op_bytes = SignedUserOp::max_batch_metadata() + A::MAX_METHOD_PAYLOAD_BYTES;
    user_op_count.saturating_mul(one_user_op_bytes as u64)
}

/// Lane-local state threaded through every loop iteration.
///
/// `head` and `last_drained_direct_range` stay in lockstep — every safe-frontier
/// advance updates both `head.safe_block` (persisted in the open frame) and
/// `last_drained_direct_range.end()` (in-memory drain cursor).
///
/// `last_frontier_check` is the time gate's bookkeeping; `None` initially so
/// the first iteration always polls.
struct LaneState {
    last_drained_direct_range: SafeInputRange,
    head: WriteHead,
    last_frontier_check: Option<Instant>,
}

impl LaneState {
    fn new(last_drained_direct_range: SafeInputRange, head: WriteHead) -> Self {
        Self {
            last_drained_direct_range,
            head,
            last_frontier_check: None,
        }
    }

    fn frontier_check_due(&self, min_interval: Duration) -> bool {
        self.last_frontier_check
            .map(|t| t.elapsed() >= min_interval)
            .unwrap_or(true)
    }

    fn mark_frontier_checked(&mut self) {
        self.last_frontier_check = Some(Instant::now());
    }
}
