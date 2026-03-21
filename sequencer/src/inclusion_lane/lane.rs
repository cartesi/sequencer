// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::thread;
use std::time::SystemTime;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::shutdown::ShutdownSignal;
use crate::storage::{SafeInputRange, Storage, StoredSafeInput, WriteHead};
use sequencer_core::application::{AppError, Application, ExecutionOutcome};
use sequencer_core::l2_tx::DirectInput;
use sequencer_core::user_op::SignedUserOp;

use super::catch_up::catch_up_application;
use super::config::InclusionLaneConfig;
use super::{InclusionLaneError, PendingUserOp, SequencerError};

pub struct InclusionLane<A: Application + 'static> {
    rx: mpsc::Receiver<PendingUserOp>,
    shutdown: ShutdownSignal,
    app: A,
    storage: Storage,
    config: InclusionLaneConfig,
}

impl<A: Application + 'static> InclusionLane<A> {
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

            let advanced_safe_frontier =
                self.maybe_advance_safe_frontier(&mut lane_state, &mut safe_inputs)?;

            let included_user_op_count =
                self.process_user_op_chunk(&mut lane_state.head, &mut included)?;

            if should_close_batch::<A>(&lane_state.head, &self.config) {
                let next_safe_block = lane_state.head.safe_block;
                self.close_frame_and_batch(&mut lane_state.head, next_safe_block)?;
            } else if !advanced_safe_frontier && included_user_op_count == 0 {
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
        let next_safe_input_index = self
            .storage
            .load_next_undrained_safe_input_index()
            .map_err(|source| InclusionLaneError::LoadNextUndrainedDirectInputIndex { source })?;

        let last_drained_direct_range = SafeInputRange::empty_at(next_safe_input_index);
        if let Some(head) = self
            .storage
            .load_open_state()
            .map_err(|source| InclusionLaneError::LoadOpenState { source })?
        {
            return Ok(LaneState {
                last_drained_direct_range,
                head,
            });
        }

        let frontier = self
            .storage
            .load_safe_frontier()
            .map_err(|source| InclusionLaneError::LoadSafeInputs { source })?;
        assert!(
            frontier.end_exclusive >= last_drained_direct_range.end_exclusive,
            "safe-input head regressed during lane initialization: safe_end={}, next={}",
            frontier.end_exclusive,
            last_drained_direct_range.end_exclusive
        );

        let leading_direct_range = last_drained_direct_range.advance_to(frontier.end_exclusive);
        self.execute_safe_inputs_range(leading_direct_range, safe_inputs)?;
        let head = self
            .storage
            .initialize_open_state(frontier.safe_block, leading_direct_range)
            .map_err(|source| InclusionLaneError::LoadOpenState { source })?;

        Ok(LaneState {
            last_drained_direct_range: leading_direct_range,
            head,
        })
    }

    fn process_user_op_chunk(
        &mut self,
        head: &mut WriteHead,
        included: &mut Vec<PendingUserOp>,
    ) -> Result<usize, InclusionLaneError> {
        included.clear();
        dequeue_and_execute_user_op_chunk(
            &mut self.rx,
            &mut self.app,
            head.frame_fee,
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

    fn maybe_advance_safe_frontier(
        &mut self,
        lane_state: &mut LaneState,
        safe_inputs: &mut Vec<StoredSafeInput>,
    ) -> Result<bool, InclusionLaneError> {
        let frontier = self
            .storage
            .load_safe_frontier()
            .map_err(|source| InclusionLaneError::LoadSafeInputs { source })?;
        assert!(
            frontier.end_exclusive >= lane_state.last_drained_direct_range.end_exclusive,
            "safe-input head regressed: safe_end={}, next={}",
            frontier.end_exclusive,
            lane_state.last_drained_direct_range.end_exclusive
        );
        if frontier.safe_block <= lane_state.head.safe_block {
            return Ok(false);
        }

        let leading_direct_range = lane_state
            .last_drained_direct_range
            .advance_to(frontier.end_exclusive);
        self.execute_safe_inputs_range(leading_direct_range, safe_inputs)?;
        self.close_frame_only(
            &mut lane_state.head,
            frontier.safe_block,
            leading_direct_range,
        )?;
        lane_state.last_drained_direct_range = leading_direct_range;
        Ok(true)
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

    fn execute_safe_inputs_range(
        &mut self,
        direct_range: SafeInputRange,
        chunk: &mut Vec<StoredSafeInput>,
    ) -> Result<SafeInputRange, InclusionLaneError> {
        let max_chunk_len = self.config.safe_input_buffer_capacity.max(1) as u64;
        let mut chunk_start = direct_range.start_inclusive;
        while chunk_start < direct_range.end_exclusive {
            let chunk_end_exclusive = direct_range
                .end_exclusive
                .min(chunk_start.saturating_add(max_chunk_len));
            self.load_safe_inputs_chunk(chunk_start, chunk_end_exclusive, chunk)?;
            self.execute_safe_inputs_chunk(chunk.as_slice())?;
            chunk_start = chunk_end_exclusive;
        }

        Ok(direct_range)
    }

    fn close_frame_and_batch(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
    ) -> Result<(), InclusionLaneError> {
        self.storage
            .close_frame_and_batch(head, next_safe_block)
            .map_err(|source| InclusionLaneError::CloseFrameRotate { source })
    }

    fn close_frame_only(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> Result<(), InclusionLaneError> {
        self.storage
            .close_frame_only(head, next_safe_block, leading_direct_range)
            .map_err(|source| InclusionLaneError::CloseFrameRotate { source })
    }

    fn load_safe_inputs_chunk(
        &mut self,
        start_inclusive: u64,
        end_exclusive: u64,
        chunk: &mut Vec<StoredSafeInput>,
    ) -> Result<(), InclusionLaneError> {
        chunk.clear();
        self.storage
            .fill_safe_inputs(start_inclusive, end_exclusive, chunk)
            .map_err(|source| InclusionLaneError::LoadSafeInputs { source })
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
        loop {
            match self.rx.try_recv() {
                Ok(item) => {
                    let _ = item
                        .respond_to
                        .send(Err(SequencerError::unavailable("sequencer shutting down")));
                }
                Err(mpsc::error::TryRecvError::Empty)
                | Err(mpsc::error::TryRecvError::Disconnected) => return,
            }
        }
    }
}

fn should_close_batch<A: Application>(head: &WriteHead, config: &InclusionLaneConfig) -> bool {
    should_close_batch_by_time(head, config) || should_close_batch_by_size::<A>(head, config)
}

fn should_close_batch_by_time(head: &WriteHead, config: &InclusionLaneConfig) -> bool {
    let age = SystemTime::now()
        .duration_since(head.batch_created_at)
        .unwrap_or_default();
    age >= config.max_batch_open
}

fn should_close_batch_by_size<A: Application>(
    head: &WriteHead,
    config: &InclusionLaneConfig,
) -> bool {
    user_op_count_to_bytes::<A>(head.batch_user_op_count) >= config.max_batch_user_op_bytes as u64
}

fn execute_user_op(
    app: &mut impl Application,
    item: PendingUserOp,
    current_frame_fee: u64,
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

pub(super) fn dequeue_and_execute_user_op_chunk(
    rx: &mut mpsc::Receiver<PendingUserOp>,
    app: &mut impl Application,
    current_frame_fee: u64,
    max_chunk: usize,
    included: &mut Vec<PendingUserOp>,
) -> Result<(), InclusionLaneError> {
    let mut executed_user_ops = 0_usize;

    while executed_user_ops < max_chunk {
        match rx.try_recv() {
            Ok(item) => {
                execute_user_op(app, item, current_frame_fee, included);
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

fn user_op_count_to_bytes<A: Application>(user_op_count: u64) -> u64 {
    let one_user_op_bytes = SignedUserOp::max_batch_metadata() + A::MAX_METHOD_PAYLOAD_BYTES;
    user_op_count.saturating_mul(one_user_op_bytes as u64)
}

struct LaneState {
    last_drained_direct_range: SafeInputRange,
    head: WriteHead,
}
