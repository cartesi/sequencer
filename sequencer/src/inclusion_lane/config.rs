// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use alloy_primitives::Address;

const DEFAULT_MAX_USER_OPS_PER_CHUNK: usize = 64;
const DEFAULT_SAFE_INPUT_BUFFER_CAPACITY: usize = 2048;
const DEFAULT_MAX_BATCH_OPEN: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy)]
pub struct InclusionLaneConfig {
    pub batch_submitter_address: Address,
    pub max_user_ops_per_chunk: usize,
    pub safe_input_buffer_capacity: usize,
    pub max_batch_open: Duration,
    pub idle_poll_interval: Duration,
}

impl InclusionLaneConfig {
    pub fn new(batch_submitter_address: Address) -> Self {
        Self {
            batch_submitter_address,
            max_user_ops_per_chunk: DEFAULT_MAX_USER_OPS_PER_CHUNK,
            safe_input_buffer_capacity: DEFAULT_SAFE_INPUT_BUFFER_CAPACITY,
            max_batch_open: DEFAULT_MAX_BATCH_OPEN,
            idle_poll_interval: DEFAULT_IDLE_POLL_INTERVAL,
        }
    }
}
