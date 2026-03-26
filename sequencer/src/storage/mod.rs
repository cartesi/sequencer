// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod db;
mod sql;

use std::time::SystemTime;
use thiserror::Error;

pub use db::Storage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSafeInput {
    pub sender: alloy_primitives::Address,
    pub payload: Vec<u8>,
    /// Chain block number where this input was included (e.g. InputAdded event block).
    pub block_number: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeInputRange {
    pub start_inclusive: u64,
    pub end_exclusive: u64,
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

    pub fn advance_to(self, end_exclusive: u64) -> Self {
        Self::new(self.end_exclusive, end_exclusive)
    }

    pub fn is_empty(self) -> bool {
        self.start_inclusive == self.end_exclusive
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeFrontier {
    pub safe_block: u64,
    pub end_exclusive: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_in_batch: u32,
    /// Log-space fee exponent (base 129/128).
    pub fee: u16,
    pub safe_block: u64,
}

#[derive(Debug, Error)]
pub enum StorageOpenError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Migration(#[from] rusqlite_migration::Error),
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
        self.batch_user_op_count = self.batch_user_op_count.saturating_add(count as u64);
        self.open_frame_user_op_count = self.open_frame_user_op_count.saturating_add(count as u32);
    }

    pub fn open_frame_has_user_ops(&self) -> bool {
        self.open_frame_user_op_count > 0
    }

    pub fn advance_frame(&mut self, policy: BatchPolicy, safe_block: u64) {
        self.frame_in_batch = self.frame_in_batch.saturating_add(1);
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
    // batch_size_target is always a reasonable byte count; clamp to u64.
    linear.try_into().unwrap_or(u64::MAX)
}
