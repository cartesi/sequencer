// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! SQLite-backed storage for the sequencer.
//!
//! [`Storage`] is the single entry point. Methods are clustered by caller role
//! across sibling files — mostly "one file per writer", plus one read-only
//! batch-aggregate file that two roles share:
//!
//! - `ingress` — inclusion lane: user-op append, frame/batch close
//! - `egress` — WS feed and catch-up replay (read-only)
//! - `l1_inputs` — input reader: safe-input ingestion, L1 head, deployment identity
//! - `l1_submission` — batch-aggregate reads (submitter frontier, pending
//!   batches, per-batch replay) shared between the submitter and egress
//! - `recovery` — cascade invalidation, recovery-batch open, danger checks
//! - `admin` — operator policy alpha tuning
//! - `fee_oracle` — L1 fee-oracle gas-price updates
//! - `snapshot_dumps` — pending/finalized snapshot lifecycle, lease counts
//! - `history` — write-once era/base metadata and recovery generation
//! - `lifecycle` — command-admission facts + the terminal-fault black box
//!
//! Cross-writer helpers are split by concern:
//!
//! - `convert` — int width + time conversions
//! - `queries` — shared read helpers (`query_*`, `load_current_write_head`)
//! - `mutations` — shared write helpers (`insert_new_batch`, `seal_batch`, …)
//!
//! The schema and `valid_*` views live in `migrations/0001_schema.sql`. See
//! `docs/recovery/README.md` for the recovery design and TLA+ specs.

mod admin;
mod convert;
mod egress;
mod fee_oracle;
mod history;
mod ingress;
mod l1_inputs;
mod l1_submission;
mod lifecycle;
mod mutations;
mod open;
mod queries;
mod recovery;
mod safe_accepted_batches;
mod snapshot_dumps;

#[cfg(test)]
pub(crate) mod test_helpers;

pub(crate) use convert::is_persistent_storage_error;

use std::time::SystemTime;
use thiserror::Error;

pub(crate) use egress::OrderedL2TxRow;
pub use history::{DirectInputExecution, HistoryState};
pub use lifecycle::{LifecycleCommand, LifecycleError, TerminalFault};
pub use open::Storage;
pub use recovery::DangerStatus;
pub(crate) use recovery::{RecoveryInspection, RecoveryMutationError};
pub use sequencer_core::history::{EraId, ExecutedInputCount, HistoryVersion, RecoveryGeneration};
pub use snapshot_dumps::{
    DumpRow, FinalizedDump, LeaseGuard, LeasedDump, PendingDump, PersistentReleaseFailureReporter,
    ReleaseScheduler,
};

/// One safe input as stored on the L1 InputBox: sender, opaque payload, and
/// the L1 block where it was included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSafeInput {
    pub sender: alloy_primitives::Address,
    pub payload: Vec<u8>,
    /// Chain block number where this input was included (e.g. InputAdded event block).
    pub block_number: u64,
}

/// One InputBox event with the L1 provenance persisted for feed consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IngestedSafeInput {
    pub sender: alloy_primitives::Address,
    pub payload: Vec<u8>,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub transaction_hash: alloy_primitives::B256,
}

/// Whether a sync also maintains the scheduler-accepted gold frontier
/// (`safe_accepted_batches`).
///
/// Every normal sync uses [`FrontierMode::Populate`]. `setup --recovery` uses
/// [`FrontierMode::DeferUntilAnchorSet`] for its interim syncs: the local batch
/// tree is rebuilt from the checkpoint *after* the sync, so populating the
/// frontier against the empty tree would mark every L1 batch `Foreign` and
/// falsely freeze it. `run`'s first sync populates it once the anchor is `N'`.
/// See `docs/recovery/cockroach.md` and the anchor-aware-frontier note on I15.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontierMode {
    /// Maintain `safe_accepted_batches` as part of the sync (the default).
    Populate,
    /// Skip frontier maintenance — the tree is rebuilt and the anchor set
    /// afterwards, and the next normal sync will populate it correctly.
    DeferUntilAnchorSet,
}

/// Half-open range `[start, end)` over `safe_input_index` values. Used to
/// describe which safe inputs a frame drained.
///
/// Fields are private so the `new`-time invariant (`end >= start`) can't be
/// broken by direct mutation. Read via [`start`](Self::start) /
/// [`end`](Self::end); construct via [`new`](Self::new) / [`empty_at`](Self::empty_at).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeInputRange {
    start_inclusive: u64,
    end_exclusive: u64,
}

impl SafeInputRange {
    pub fn new(start_inclusive: u64, end_exclusive: u64) -> Self {
        assert!(
            end_exclusive >= start_inclusive,
            "safe-input range must be half-open and non-negative: start={start_inclusive}, end={end_exclusive}"
        );
        Self {
            start_inclusive,
            end_exclusive,
        }
    }

    pub fn empty_at(index: u64) -> Self {
        Self::new(index, index)
    }

    /// Extend the range forward, producing `[self.end, new_end)`. Panics if
    /// `new_end < self.end` — this is the "advance" direction only.
    pub fn advance_to(self, end_exclusive: u64) -> Self {
        Self::new(self.end_exclusive, end_exclusive)
    }

    pub fn start(self) -> u64 {
        self.start_inclusive
    }

    pub fn end(self) -> u64 {
        self.end_exclusive
    }

    pub fn is_empty(self) -> bool {
        self.start_inclusive == self.end_exclusive
    }

    /// Split the range into consecutive sub-ranges of at most `max_len`
    /// elements. The last chunk may be shorter. Yields nothing if empty.
    pub fn chunks(self, max_len: u64) -> SafeInputRangeChunks {
        assert!(max_len > 0, "chunk size must be positive");
        SafeInputRangeChunks {
            cursor: self.start_inclusive,
            end: self.end_exclusive,
            max_len,
        }
    }
}

/// Iterator returned by [`SafeInputRange::chunks`].
pub struct SafeInputRangeChunks {
    cursor: u64,
    end: u64,
    max_len: u64,
}

impl Iterator for SafeInputRangeChunks {
    type Item = SafeInputRange;

    fn next(&mut self) -> Option<SafeInputRange> {
        if self.cursor >= self.end {
            return None;
        }
        let chunk_end = self.end.min(self.cursor.saturating_add(self.max_len));
        let chunk = SafeInputRange::new(self.cursor, chunk_end);
        self.cursor = chunk_end;
        Some(chunk)
    }
}

/// Snapshot of the L1 view: current safe block plus the exclusive cursor into
/// `safe_inputs`. Read by the inclusion lane to decide when to advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeInputFrontier {
    pub safe_block: u64,
    pub end_exclusive: u64,
}

/// Whether the inclusion lane may reconcile the persisted L1 projection.
/// A poisoned projection never yields a usable frontier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeFrontierState {
    Open(SafeInputFrontier),
    CanonicalDivergence { nonce: u64, safe_input_index: u64 },
}

/// Snapshot of the scheduler-accepted frontier: current safe block plus the
/// next nonce the scheduler is expected to accept. Read by the batch submitter
/// each tick to derive the next unresolved nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitterFrontier {
    pub safe_block: u64,
    pub accepted_next_nonce: u64,
}

/// Per-frame metadata: position within batch, committed fee, and the
/// safe-block boundary the frame draws against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_in_batch: u32,
    /// Log-space fee exponent (base 129/128).
    pub fee: u16,
    pub safe_block: u64,
}

/// A batch ready for L1 submission: its local index, assigned nonce, and SSZ-encoded payload.
#[derive(Debug)]
pub struct PendingBatch {
    pub batch_index: u64,
    pub nonce: u64,
    pub encoded: Vec<u8>,
}

/// Deployment identity the database is bound to.
///
/// The persisted sequencer state is not portable across these fields:
/// historical safe inputs are classified using the batch-submitter address,
/// user-op signatures use the app/domain identity, and recovery/frontier logic
/// depends on the app's InputBox stream, which is empty before
/// `app_deployment_block` (the v3 InputBox refuses inputs for apps that do
/// not exist yet — the reader anchors its scan there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploymentIdentity {
    pub chain_id: u64,
    pub app_address: alloy_primitives::Address,
    pub input_box_address: alloy_primitives::Address,
    pub app_deployment_block: u64,
    pub batch_submitter_address: alloy_primitives::Address,
    pub fee_oracle: FeeOracleIdentity,
}

/// Fee-oracle configuration pinned at setup. Runtime reads this immutable
/// identity rather than accepting address-bearing oracle configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeOracleIdentity {
    Fixed {
        log_gas_price: u16,
    },
    Uniswap {
        weth: alloy_primitives::Address,
        fee_token: alloy_primitives::Address,
        pool: alloy_primitives::Address,
        twap_window_secs: u32,
    },
}

/// Returned by [`Storage::open`] and friends; either the SQLite handle failed
/// to open or migrations refused to apply.
#[derive(Debug, Error)]
pub enum StorageOpenError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Migration(#[from] rusqlite_migration::Error),
    /// No database file at the given path. Databases are only created by an
    /// owning command (`setup` / `rebuild`); a missing file at `open` time is
    /// a deployment mistake, never a cue to create one (D6).
    #[error(
        "no database at {path} — this data directory was never initialized; run `setup` (or check --data-dir)"
    )]
    NeverInitialized { path: String },
}

pub(crate) fn is_persistent_storage_open_error(error: &StorageOpenError) -> bool {
    match error {
        StorageOpenError::Sqlite(source) => is_persistent_storage_error(source),
        StorageOpenError::Migration(rusqlite_migration::Error::RusqliteError { err, .. }) => {
            is_persistent_storage_error(err)
        }
        // Invalid schema versions/definitions and failed FK checks cannot be
        // repaired by retrying the same durable database.
        StorageOpenError::Migration(_) => true,
        // A missing database is a deterministic deployment mistake; retrying
        // the same configuration re-fails identically.
        StorageOpenError::NeverInitialized { .. } => true,
    }
}

/// Derived batch policy read from the `batch_policy_derived` view.
/// Both fields are log-space exponents (base 129/128).
#[derive(Debug, Clone, Copy)]
pub struct BatchPolicy {
    /// Log-space fee exponent (base 129/128).
    pub recommended_fee: u16,
    /// Log-space batch size target (base 129/128). Convert via `fee_to_linear()` for byte comparison.
    pub batch_size_target: u16,
}

/// Trusted, reconstructible in-memory cache of the latest durable open batch +
/// frame. Mutated by `Storage` methods that change the open state
/// (`append_executed_user_ops_chunk`, attributed `close_*`). The sole-writer
/// lane loads one from SQLite and threads it through every call; failed
/// writes/restarts discard it. Physical-only siblings are test fixtures.
#[derive(Debug, Clone, Copy)]
pub struct WriteHead {
    pub batch_index: u64,
    pub batch_created_at: SystemTime,
    /// Log-space fee exponent (base 129/128) committed for this open frame.
    pub frame_fee: u16,
    pub safe_block: u64,
    pub batch_user_op_count: u64,
    pub open_frame_user_op_count: u32,
    pub frame_in_batch: u32,
    // Soft batch size threshold read from batch_policy at each frame/batch transition.
    pub max_batch_user_op_bytes: u64,
}

impl WriteHead {
    pub fn increment_batch_user_op_count(&mut self, count: usize) {
        let count_u64 =
            u64::try_from(count).expect("user-op chunk length exceeds u64: contract-impossible");
        let count_u32 =
            u32::try_from(count).expect("user-op chunk length exceeds u32: contract-impossible");
        let next_batch_count = self
            .batch_user_op_count
            .checked_add(count_u64)
            .expect("batch user-op count overflow: contract-impossible");
        let next_frame_count = self
            .open_frame_user_op_count
            .checked_add(count_u32)
            .expect("frame user-op count overflow: contract-impossible");
        self.batch_user_op_count = next_batch_count;
        self.open_frame_user_op_count = next_frame_count;
    }

    pub fn open_frame_has_user_ops(&self) -> bool {
        self.open_frame_user_op_count > 0
    }

    pub fn advance_frame(&mut self, policy: BatchPolicy, safe_block: u64) {
        self.frame_in_batch = self
            .frame_in_batch
            .checked_add(1)
            .expect("frame index overflow: contract-impossible");
        self.frame_fee = policy.recommended_fee;
        self.safe_block = safe_block;
        self.open_frame_user_op_count = 0;
        self.max_batch_user_op_bytes = batch_size_target_bytes(policy);
    }

    pub fn move_to_next_batch(
        &mut self,
        batch_index: u64,
        batch_created_at: SystemTime,
        policy: BatchPolicy,
        safe_block: u64,
    ) {
        self.batch_index = batch_index;
        self.batch_created_at = batch_created_at;
        self.frame_fee = policy.recommended_fee;
        self.safe_block = safe_block;
        self.batch_user_op_count = 0;
        self.open_frame_user_op_count = 0;
        self.frame_in_batch = 0;
        self.max_batch_user_op_bytes = batch_size_target_bytes(policy);
    }
}

/// Convert the log-space `batch_size_target` to a linear byte count for the inclusion lane.
fn batch_size_target_bytes(policy: BatchPolicy) -> u64 {
    let linear = sequencer_core::fee::fee_to_linear(policy.batch_size_target);
    linear.try_into().unwrap_or_else(|_| {
        panic!(
            "batch size target exponent {} does not fit u64 bytes: contract-impossible",
            policy.batch_size_target
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use std::time::UNIX_EPOCH;

    fn write_head() -> WriteHead {
        WriteHead {
            batch_index: 0,
            batch_created_at: UNIX_EPOCH,
            frame_fee: 0,
            safe_block: 0,
            batch_user_op_count: 0,
            open_frame_user_op_count: 0,
            frame_in_batch: 0,
            max_batch_user_op_bytes: 1,
        }
    }

    #[test]
    #[should_panic(expected = "batch user-op count overflow: contract-impossible")]
    fn write_head_batch_count_fails_loud_on_overflow() {
        let mut head = write_head();
        head.batch_user_op_count = u64::MAX;
        head.increment_batch_user_op_count(1);
    }

    #[test]
    #[should_panic(expected = "frame user-op count overflow: contract-impossible")]
    fn write_head_frame_count_fails_loud_on_overflow() {
        let mut head = write_head();
        head.open_frame_user_op_count = u32::MAX;
        head.increment_batch_user_op_count(1);
    }

    #[test]
    #[should_panic(expected = "frame index overflow: contract-impossible")]
    fn write_head_frame_index_fails_loud_on_overflow() {
        let mut head = write_head();
        head.frame_in_batch = u32::MAX;
        head.advance_frame(
            BatchPolicy {
                recommended_fee: 0,
                batch_size_target: 0,
            },
            0,
        );
    }

    #[test]
    #[should_panic(expected = "does not fit u64 bytes: contract-impossible")]
    fn batch_size_target_fails_loud_when_linear_value_exceeds_u64() {
        let exponent = 6000;
        assert!(
            sequencer_core::fee::fee_to_linear(exponent) > U256::from(u64::MAX),
            "test exponent must exceed the byte-counter representation"
        );
        let _ = batch_size_target_bytes(BatchPolicy {
            recommended_fee: 0,
            batch_size_target: exponent,
        });
    }
}
