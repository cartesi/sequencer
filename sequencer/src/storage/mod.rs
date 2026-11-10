// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! SQLite storage. [`Storage`] is the single entry point; each writer role's
//! methods live in its own sibling file (writer table: `docs/invariants.md`),
//! shared reads in `queries` and shared writes in `mutations`. The schema and
//! `valid_*` views live in `migrations/0001_schema.sql`.

mod acceptance;
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
mod rebuilt_history;
mod recovery;
mod snapshots;

#[cfg(test)]
pub(crate) mod test_helpers;

pub(crate) use convert::is_persistent_storage_error;

use std::time::SystemTime;
use thiserror::Error;

pub(crate) use egress::HistoricalReadError;
#[cfg(test)]
pub(crate) use history::ApplicationInputRow;
pub use history::{DirectInputExecution, NewEra};
pub(crate) use history::{HistoryReadError, L2TxContext};
pub(crate) use ingress::ExecutedUserOp;
pub use lifecycle::{LifecycleCommand, LifecycleError};
pub use open::Storage;
pub use recovery::DangerStatus;
pub(crate) use recovery::{RecoveryInspection, RecoveryMutationError};
pub use sequencer_core::history::L2TxCount;
pub use snapshots::{
    FinalizedLease, FinalizedSelectionError, LeaseGuard, LeasedDump, Snapshot, SnapshotLeases,
};

/// One safe input as stored on the L1 InputBox: sender, opaque payload, and
/// the L1 block where it was included. Test fixtures seed inputs with it.
#[cfg(test)]
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

    #[cfg(test)]
    pub(crate) fn empty_at(index: u64) -> Self {
        Self::new(index, index)
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
/// historical safe inputs are classified using the batch-submitter address
/// and the direct-payload cap,
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
    /// The application's `scheduler_limits().max_direct_payload_bytes`: with
    /// the batch submitter, it decides which safe inputs execute as directs.
    pub max_direct_payload_bytes: u64,
    pub fee_oracle: FeeOracleIdentity,
}

/// Fee-oracle configuration pinned at setup. Runtime reads this immutable
/// identity rather than accepting address-bearing oracle configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeOracleIdentity {
    Fixed {
        log_gas_price: i16,
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
    /// No database file at the given path. Databases are only created by the
    /// owning command, `setup`; a missing file at `open` time is a deployment
    /// mistake, never a cue to create one.
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
/// frame. Opaque outside storage: only the loader (`queries::load_current_write_head`)
/// builds one, and only the `Storage` methods that change the open state
/// (`append_executed_user_ops_chunk`, attributed `close_*`) advance it, after
/// their commit. Storage reloads the Tip and compares it at every frame
/// rotation and batch close. Failed writes and restarts discard it.
#[derive(Debug)]
pub struct WriteHead {
    batch_index: u64,
    /// The open batch's nonce; immutable once the batch is inserted.
    batch_nonce: u64,
    batch_created_at: SystemTime,
    /// Log-space fee exponent (base 129/128) committed for this open frame.
    frame_fee: u16,
    safe_block: u64,
    batch_user_op_count: u64,
    open_frame_user_op_count: u32,
    frame_in_batch: u32,
    // Soft batch size threshold read from batch_policy at each frame/batch transition.
    max_batch_user_op_bytes: u64,
}

impl WriteHead {
    pub(crate) fn batch_index(&self) -> u64 {
        self.batch_index
    }

    pub(crate) fn batch_nonce(&self) -> u64 {
        self.batch_nonce
    }

    pub(crate) fn batch_created_at(&self) -> SystemTime {
        self.batch_created_at
    }

    /// Log-space fee exponent (base 129/128) committed for the open frame.
    pub(crate) fn frame_fee(&self) -> u16 {
        self.frame_fee
    }

    pub(crate) fn safe_block(&self) -> u64 {
        self.safe_block
    }

    pub(crate) fn batch_user_op_count(&self) -> u64 {
        self.batch_user_op_count
    }

    pub(crate) fn max_batch_user_op_bytes(&self) -> u64 {
        self.max_batch_user_op_bytes
    }

    #[cfg(test)]
    pub(crate) fn frame_in_batch(&self) -> u32 {
        self.frame_in_batch
    }

    #[cfg(test)]
    pub(crate) fn open_frame_user_op_count(&self) -> u32 {
        self.open_frame_user_op_count
    }

    /// A private copy, so a counter advance is validated before the
    /// transaction that commits it.
    fn duplicate(&self) -> Self {
        Self { ..*self }
    }

    fn increment_batch_user_op_count(&mut self, count: usize) {
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

    fn advance_frame(&mut self, policy: BatchPolicy, safe_block: u64) {
        self.frame_in_batch = self
            .frame_in_batch
            .checked_add(1)
            .expect("frame index overflow: contract-impossible");
        self.frame_fee = policy.recommended_fee;
        self.safe_block = safe_block;
        self.open_frame_user_op_count = 0;
        self.max_batch_user_op_bytes = batch_size_target_bytes(policy);
    }

    fn move_to_next_batch(
        &mut self,
        batch_index: u64,
        batch_nonce: u64,
        batch_created_at: SystemTime,
        policy: BatchPolicy,
    ) {
        self.batch_index = batch_index;
        self.batch_nonce = batch_nonce;
        self.batch_created_at = batch_created_at;
        self.frame_fee = policy.recommended_fee;
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
    use super::test_helpers::WriteHeadParts;
    use super::*;
    use alloy_primitives::U256;

    #[test]
    #[should_panic(expected = "batch user-op count overflow: contract-impossible")]
    fn write_head_batch_count_fails_loud_on_overflow() {
        let mut head = WriteHeadParts {
            batch_user_op_count: u64::MAX,
            ..WriteHeadParts::ZERO
        }
        .build();
        head.increment_batch_user_op_count(1);
    }

    #[test]
    #[should_panic(expected = "frame user-op count overflow: contract-impossible")]
    fn write_head_frame_count_fails_loud_on_overflow() {
        let mut head = WriteHeadParts {
            open_frame_user_op_count: u32::MAX,
            ..WriteHeadParts::ZERO
        }
        .build();
        head.increment_batch_user_op_count(1);
    }

    #[test]
    #[should_panic(expected = "frame index overflow: contract-impossible")]
    fn write_head_frame_index_fails_loud_on_overflow() {
        let mut head = WriteHeadParts {
            frame_in_batch: u32::MAX,
            ..WriteHeadParts::ZERO
        }
        .build();
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
