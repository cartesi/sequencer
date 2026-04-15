// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Runtime knobs for the inclusion lane. Defaults tuned for low-latency
//! Ethereum L1 deployment; tests override individual fields directly.

use std::time::Duration;

use alloy_primitives::Address;

const DEFAULT_MAX_USER_OPS_PER_CHUNK: usize = 64;
const DEFAULT_SAFE_INPUT_BUFFER_CAPACITY: usize = 2048;
const DEFAULT_MAX_BATCH_OPEN: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Minimum gap between L1 safe-frontier polls. Bounds the SQL load when the
/// lane is otherwise idle. L1 safe head advances at ~12s cadence, so 1s is
/// well inside the responsiveness budget.
const DEFAULT_FRONTIER_MIN_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
pub struct InclusionLaneConfig {
    /// Address of the batch submitter wallet. Direct inputs from this sender
    /// are skipped during application execution (they're our own batch
    /// submissions; the application doesn't apply them as user-level inputs).
    pub batch_submitter_address: Address,
    /// Cap on user ops dequeued per chunk. Bounds per-chunk SQL transaction
    /// size and (more importantly) ack latency for the first op in each chunk.
    pub max_user_ops_per_chunk: usize,
    /// Reusable buffer size for safe-input loading. Doesn't bound work; just
    /// the memory ceiling for the read-and-execute scratch buffer.
    pub safe_input_buffer_capacity: usize,
    /// Force a batch close after this much wall time, regardless of size.
    pub max_batch_open: Duration,
    /// Sleep duration when the lane has nothing to do (no queue, no advance).
    pub idle_poll_interval: Duration,
    /// Minimum gap between L1 safe-frontier polls. Bounds idle SQL load. See
    /// `DEFAULT_FRONTIER_MIN_INTERVAL` for the rationale on the default.
    pub frontier_min_interval: Duration,
}

impl InclusionLaneConfig {
    pub fn new(batch_submitter_address: Address) -> Self {
        Self {
            batch_submitter_address,
            max_user_ops_per_chunk: DEFAULT_MAX_USER_OPS_PER_CHUNK,
            safe_input_buffer_capacity: DEFAULT_SAFE_INPUT_BUFFER_CAPACITY,
            max_batch_open: DEFAULT_MAX_BATCH_OPEN,
            idle_poll_interval: DEFAULT_IDLE_POLL_INTERVAL,
            frontier_min_interval: DEFAULT_FRONTIER_MIN_INTERVAL,
        }
    }
}
