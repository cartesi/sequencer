// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Single ordering lane with a latency-critical user-op regime and a slower L1
//! reconciliation regime. It runs three layers of amortization:
//!
//! - **Fast processing** (`run_fast_turn`): processes at most one bounded
//!   user-op chunk per turn. Rejected requests therefore cannot keep a
//!   continuously nonempty queue from starving reconciliation.
//! - **Per-chunk persistence** (`max_user_ops_per_chunk`): a nonempty accepted
//!   subset commits at most once, bounding ack latency for its first op.
//!   All-rejected chunks mutate nothing and do not open a transaction.
//! - **L1 reconciliation** (observed at `frontier_min_interval`): once five
//!   newly-safe blocks have accumulated, executes the complete direct-input range and advances one frame directly to the observed tip. The time
//!   gate bounds SQL load; block distance is the semantic clock criterion.
//!   That frontier read is also the lane's divergence refusal point (I15):
//!   a marker already present closes intake before direct execution,
//!   or the frame-clock decision.
//!
//! The lane is a single-thread `spawn_blocking` task. SQLite is the durable data
//! coordination boundary with the input reader and batch submitter. HTTP
//! ingress uses the deliberate bounded-channel request/response exception;
//! `RuntimeScope` is process control, not data coordination. Reconciliation
//! has no timeout/resume protocol: supported applications are assumed to
//! promptly digest the complete newly-safe range in the supported envelope.

mod catch_up;
mod config;
pub mod dump_info;
mod error;
mod snapshot;
mod types;

#[cfg(test)]
mod tests;

pub use config::InclusionLaneConfig;
pub use error::InclusionLaneError;
pub(crate) use types::IncludedUserOp;
pub use types::{PendingUserOp, SequencerError};

use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::runtime::shutdown::RuntimeScope;
use crate::storage::{SafeFrontierState, SafeInputRange, Storage, StoredDirectInput, WriteHead};
use sequencer_core::application::{
    Application, ExecutionOutcome, execute_direct_input, validate_and_execute_user_op,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::user_op::SignedUserOp;

use catch_up::{catch_up_application, catch_up_snapshot};

/// Owns the application instance, the `Storage` write handle, and the user-op
/// receiver for the lifetime of the sequencer process.
pub struct InclusionLane<A: Application + 'static> {
    rx: mpsc::Receiver<PendingUserOp>,
    shutdown: RuntimeScope,
    app: A,
    storage: Storage,
    config: InclusionLaneConfig,
}

impl<A: Application + 'static> InclusionLane<A> {
    /// Spawn the lane on a blocking thread. Startup recovery establishes the
    /// open Tip, so the lane loads its resume state and never initializes a Tip.
    /// It fails loudly with
    /// [`InclusionLaneError::NoOpenTip`] if the invariant was somehow violated.
    ///
    /// Restore the newest surviving snapshot and replay from its exact
    /// application count. Startup preserves a rollback-safe checkpoint before
    /// admitting any runtime work.
    ///
    /// Returns the input MPSC sender (for the API to enqueue user
    /// ops) and the join handle (for the runtime to observe lane
    /// shutdown). The handle resolves to `Ok(())` on graceful
    /// shutdown, or an `InclusionLaneError` if the lane crashed.
    pub(crate) fn start(
        queue_capacity: usize,
        shutdown: RuntimeScope,
        storage: Storage,
        config: InclusionLaneConfig,
    ) -> (
        mpsc::Sender<PendingUserOp>,
        JoinHandle<Result<(), InclusionLaneError>>,
    ) {
        let (tx, rx) = mpsc::channel::<PendingUserOp>(queue_capacity.max(1));
        let handle = tokio::task::spawn_blocking(move || -> Result<(), InclusionLaneError> {
            let mut storage = storage;
            // Single checkpoint selection: the same snapshot supplies both
            // the dump dir we load the Application from and the offset we
            // replay from, so the loaded state and the catch-up cursor
            // can never drift apart.
            let checkpoint = catch_up_snapshot(&mut storage)
                .map_err(|source| InclusionLaneError::CatchUp { source })?;
            let app = A::from_dump(&dump_info::app_prefix(&checkpoint.dump_dir))
                .map_err(InclusionLaneError::LoadFromDump)?;
            if app.executed_input_count() != checkpoint.executed_input_count {
                return Err(InclusionLaneError::CatchUp {
                    source: error::CatchUpError::SnapshotExecutionCountMismatch {
                        application: app.executed_input_count().get(),
                        storage: checkpoint.executed_input_count.get(),
                    },
                });
            }
            tracing::debug!(
                executed_input_count = checkpoint.executed_input_count.get(),
                "inclusion lane resuming from snapshot"
            );
            let mut lane = Self {
                rx,
                shutdown,
                app,
                storage,
                config,
            };
            lane.run_forever(checkpoint.executed_input_count)
        });
        (tx, handle)
    }

    fn run_forever(&mut self, catch_up_from: ExecutedInputCount) -> Result<(), InclusionLaneError> {
        self.run_catch_up(catch_up_from)?;
        let mut included = Vec::with_capacity(self.config.max_user_ops_per_chunk.max(1));
        let mut safe_inputs = Vec::with_capacity(self.config.safe_input_buffer_capacity.max(1));
        // Startup recovery established the Tip before runtime admission.
        // Read the open frame (fail-loud if absent) and the drain
        // cursor together from storage, so both come from the same place. Any
        // leading range already sequenced into the Tip's frames (genesis or a
        // recovery batch) was replayed into the app by `run_catch_up` above,
        // so there is no cold-start drain here.
        let head = self
            .storage
            .open_state()?
            .ok_or(InclusionLaneError::NoOpenTip)?;
        let next_undrained = self.storage.next_undrained_safe_input_index()?;
        let mut lane_state = LaneState::new(next_undrained, head);

        loop {
            if self.shutdown.is_shutdown_requested() {
                self.reject_pending_user_ops_due_to_shutdown();
                return Ok(());
            }

            self.maybe_advance_safe_frontier(&mut lane_state, &mut safe_inputs)?;
            let turn = self.run_fast_turn(&mut lane_state.head, &mut included)?;

            if turn.hit_batch_target() || should_close_batch_by_time(&lane_state.head, &self.config)
            {
                let next_safe_block = lane_state.head.safe_block;
                // Atomic close: dump the app state, then seal the batch
                // and register its snapshot in one transaction.
                // A create_dump failure leaves the batch open for retry;
                // a committed close always has its immutable snapshot row.
                // Errors propagate per the lane's fail-loud policy.
                snapshot::close_batch_with_snapshot(
                    &mut self.app,
                    &mut self.storage,
                    &mut lane_state.head,
                    next_safe_block,
                    &self.config.dumps_dir,
                )
                .map_err(InclusionLaneError::Snapshot)?;
            } else if !turn.processed_any() {
                // Nothing to drain and no batch to close: back off. GC runs on the reconciliation cadence so load cannot starve it.
                thread::sleep(self.config.idle_poll_interval);
            }
        }
    }

    fn run_catch_up(&mut self, start_offset: ExecutedInputCount) -> Result<(), InclusionLaneError> {
        catch_up_application(&mut self.app, &mut self.storage, start_offset)
            .map_err(|source| InclusionLaneError::CatchUp { source })
    }

    /// Process at most one bounded dequeue chunk. Returning to the outer loop
    /// does not imply a frontier read: that check remains independently
    /// time-gated, so fast turns normally run back-to-back.
    fn run_fast_turn(
        &mut self,
        head: &mut WriteHead,
        included: &mut Vec<IncludedUserOp>,
    ) -> Result<TurnOutcome, InclusionLaneError> {
        included.clear();
        let outcome = match dequeue_and_execute_user_op_chunk::<A>(
            &mut self.rx,
            &mut self.app,
            self.config.max_user_ops_per_chunk.max(1),
            head,
            included,
        ) {
            Ok(outcome) => outcome,
            Err(err) => {
                Self::respond_internal_to_all(included, "application internal error".to_string());
                return Err(err);
            }
        };
        persist_included_user_ops(&mut self.storage, head, included)?;
        acknowledge_included(included);

        Ok(outcome)
    }

    /// Time-gated to bound idle SQL load. The preceding fast turn is one
    /// bounded dequeue chunk, so accepted and rejected traffic have the same
    /// finite attempt bound before this method gets another opportunity.
    fn maybe_advance_safe_frontier(
        &mut self,
        lane_state: &mut LaneState,
        safe_inputs: &mut Vec<StoredDirectInput>,
    ) -> Result<(), InclusionLaneError> {
        if !lane_state.frontier_check_due(self.config.frontier_min_interval) {
            return Ok(());
        }
        lane_state.mark_frontier_checked();

        let frontier = match self.storage.safe_frontier_state()? {
            SafeFrontierState::Open(frontier) => frontier,
            SafeFrontierState::CanonicalDivergence {
                nonce,
                safe_input_index,
            } => {
                self.reject_pending_user_ops_due_to_shutdown();
                return Err(InclusionLaneError::CanonicalDivergence {
                    nonce,
                    safe_input_index,
                });
            }
        };
        assert!(
            frontier.end_exclusive >= lane_state.next_safe_input_index,
            "safe-input head regressed: safe_end={}, next={}",
            frontier.end_exclusive,
            lane_state.next_safe_input_index
        );
        assert!(
            frontier.safe_block >= lane_state.head.safe_block,
            "safe-block frontier regressed: observed={}, frame={}",
            frontier.safe_block,
            lane_state.head.safe_block,
        );
        if frontier.safe_block - lane_state.head.safe_block
            < sequencer_core::protocol::ProtocolTiming::FRAME_CLOCK_INTERVAL_SAFE_BLOCKS
        {
            return Ok(());
        }

        let leading_direct_range =
            SafeInputRange::new(lane_state.next_safe_input_index, frontier.end_exclusive);
        let executions = self.execute_direct_range(leading_direct_range, safe_inputs)?;
        self.storage.close_frame_only_with_executions(
            &mut lane_state.head,
            frontier.safe_block,
            leading_direct_range,
            &executions,
        )?;
        lane_state.next_safe_input_index = frontier.end_exclusive;

        // Acceptance can advance independently of application inputs. A bounded
        // reconciliation cadence reclaims retired artifacts even under load.
        let removed = snapshot::run_gc(&mut self.storage)?;
        if removed > 0 {
            tracing::debug!(removed, "snapshot garbage collection removed artifacts");
        }
        Ok(())
    }

    fn execute_direct_range(
        &mut self,
        range: SafeInputRange,
        chunk: &mut Vec<StoredDirectInput>,
    ) -> Result<Vec<crate::storage::DirectInputExecution>, InclusionLaneError> {
        let mut executions = Vec::new();
        for chunk_range in range.chunks(self.config.safe_input_buffer_capacity.max(1) as u64) {
            self.storage.fill_direct_inputs(chunk_range, chunk)?;
            for input in chunk.iter() {
                let receipt = execute_direct_input(&mut self.app, &input.input)
                    .map_err(|source| InclusionLaneError::ExecuteDirectInput { source })?;
                executions.push(crate::storage::DirectInputExecution {
                    safe_input_index: input.safe_input_index,
                    executed_input_offset: receipt.offset,
                });
            }
        }
        Ok(executions)
    }

    fn respond_internal_to_all(pending: &mut Vec<IncludedUserOp>, message: String) {
        for item in pending.drain(..) {
            let _ = item
                .pending
                .respond_to
                .send(Err(SequencerError::internal(message.clone())));
        }
    }

    fn reject_pending_user_ops_due_to_shutdown(&mut self) {
        self.rx.close();
        while let Ok(item) = self.rx.try_recv() {
            let _ = item
                .respond_to
                .send(Err(SequencerError::unavailable("sequencer shutting down")));
        }
    }
}

/// Commit the accepted chunk (`synchronous=FULL`). Only this commit
/// authorizes acknowledgements; a failed commit answers internal-error.
fn persist_included_user_ops(
    storage: &mut Storage,
    head: &mut WriteHead,
    included: &mut Vec<IncludedUserOp>,
) -> Result<(), InclusionLaneError> {
    storage
        .append_executed_user_ops_chunk(head, included.as_slice())
        .map_err(|err| {
            for item in included.drain(..) {
                let _ = item.pending.respond_to.send(Err(SequencerError::internal(
                    "internal storage error".to_string(),
                )));
            }
            InclusionLaneError::Storage(err)
        })
}

/// Acknowledge the FULL-committed chunk.
fn acknowledge_included(included: &mut Vec<IncludedUserOp>) {
    for item in included.drain(..) {
        let _ = item.pending.respond_to.send(Ok(()));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TurnOutcome {
    /// The queue was observed empty and no accepted operation was persisted.
    Idle,
    /// Processed one chunk without crossing the batch target. This includes a
    /// full all-rejected chunk, which must still yield to reconciliation.
    Processed,
    /// An accepted operation crossed the batch size target.
    HitBatchTarget,
}

impl TurnOutcome {
    fn hit_batch_target(&self) -> bool {
        matches!(self, Self::HitBatchTarget)
    }

    fn processed_any(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

fn should_close_batch_by_time(head: &WriteHead, config: &InclusionLaneConfig) -> bool {
    // A backwards clock step makes `duration_since` err; `unwrap_or_default`
    // then reads as age 0, silently stalling the time-based close trigger
    // until the clock catches up. Acceptable: the size trigger
    // is unaffected, and a wedge here is liveness-only, never correctness.
    let age = SystemTime::now()
        .duration_since(head.batch_created_at)
        .unwrap_or_default();
    age >= config.max_batch_open
}

fn execute_user_op(
    app: &mut impl Application,
    item: PendingUserOp,
    current_frame_fee: u16,
    frame_safe_block: u64,
    included: &mut Vec<IncludedUserOp>,
) -> Result<(), InclusionLaneError> {
    match validate_and_execute_user_op(
        app,
        item.signed.sender,
        &item.signed.user_op,
        current_frame_fee,
        frame_safe_block,
    ) {
        Ok(ExecutionOutcome::Included(receipt)) => included.push(IncludedUserOp {
            pending: item,
            executed_input_offset: receipt.offset,
        }),
        Ok(ExecutionOutcome::Invalid(reason)) => {
            let _ = item
                .respond_to
                .send(Err(SequencerError::invalid(reason.to_string())));
        }
        // An application error has no canonical successor. Discard the
        // application-owned state; this op is never persisted or acknowledged.
        Err(err) => {
            // The client gets a fixed message: the application's reason and
            // any I/O detail travel on the lane error and the log, never
            // into the public 500 body.
            let _ = item.respond_to.send(Err(SequencerError::internal(
                "application internal error".to_string(),
            )));
            return Err(InclusionLaneError::ExecuteUserOp { source: err });
        }
    }
    Ok(())
}

/// Dequeue and execute up to `max_chunk` user ops, stopping early if the batch
/// would cross its size target. Returns the outcome that drove the stop.
///
/// `head.batch_user_op_count` reflects already-persisted ops; `included.len()`
/// is the count we'd add by persisting now. When their sum's bytes equal or
/// exceed `head.max_batch_user_op_bytes`, we stop and the caller closes the
/// batch.
fn dequeue_and_execute_user_op_chunk<A: Application>(
    rx: &mut mpsc::Receiver<PendingUserOp>,
    app: &mut A,
    max_chunk: usize,
    head: &WriteHead,
    included: &mut Vec<IncludedUserOp>,
) -> Result<TurnOutcome, InclusionLaneError> {
    let mut attempted = 0_usize;

    while attempted < max_chunk {
        match rx.try_recv() {
            Ok(item) => {
                execute_user_op(app, item, head.frame_fee, head.safe_block, included)?;
                attempted += 1;

                let included_count =
                    u64::try_from(included.len()).expect("in-memory chunk length must fit in u64");
                let projected = head
                    .batch_user_op_count
                    .checked_add(included_count)
                    .expect("batch user-op count overflow: contract-impossible");
                if user_op_count_to_bytes::<A>(projected) >= head.max_batch_user_op_bytes {
                    return Ok(TurnOutcome::HitBatchTarget);
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                if attempted == 0 {
                    return Err(InclusionLaneError::ChannelClosed);
                }
                break;
            }
        }
    }

    Ok(if attempted == max_chunk || !included.is_empty() {
        TurnOutcome::Processed
    } else {
        TurnOutcome::Idle
    })
}

fn user_op_count_to_bytes<A: Application>(user_op_count: u64) -> u64 {
    let one_user_op_bytes = SignedUserOp::max_batch_metadata()
        .checked_add(A::max_method_payload_bytes())
        .expect("one user-op wire bound overflow: contract-impossible");
    let one_user_op_bytes =
        u64::try_from(one_user_op_bytes).expect("one user-op wire bound must fit in u64");
    // This is a comparison bound, not a persisted domain value: once the
    // mathematical product exceeds u64::MAX it is certainly over every
    // representable batch target, so clamping preserves the predicate exactly.
    user_op_count.saturating_mul(one_user_op_bytes)
}

/// Lane-local state threaded through every loop iteration.
///
/// `head` and `next_safe_input_index` stay in lockstep — every safe-frontier
/// advance updates both `head.safe_block` (persisted in the open frame) and
/// `next_safe_input_index` (in-memory drain cursor).
///
/// `last_frontier_check` is the time gate's bookkeeping; `None` initially so
/// the first iteration always polls.
struct LaneState {
    next_safe_input_index: u64,
    head: WriteHead,
    last_frontier_check: Option<Instant>,
}

impl LaneState {
    fn new(next_safe_input_index: u64, head: WriteHead) -> Self {
        Self {
            next_safe_input_index,
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
