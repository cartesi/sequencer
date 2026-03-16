// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use alloy_primitives::Address;
use sequencer_core::application::Application;
use sequencer_core::user_op::SignedUserOp;

const DEFAULT_MAX_USER_OPS_PER_CHUNK: usize = 1024;
const DEFAULT_SAFE_INPUT_BUFFER_CAPACITY: usize = 2048;
const DEFAULT_MAX_BATCH_OPEN: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_MAX_BATCH_USER_OP_BYTES: usize = 1_048_576; // 1 MiB
const DEFAULT_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(2);

#[derive(Debug, Clone, Copy)]
pub struct InclusionLaneConfig {
    pub batch_submitter_address: Address,
    pub max_user_ops_per_chunk: usize,
    pub safe_input_buffer_capacity: usize,
    pub max_batch_open: Duration,

    // Soft threshold for batch rotation.
    //
    // We intentionally check this between chunks (not per user-op) to keep the hot path
    // simple and low-latency. This means batches can overshoot the threshold by at most
    // one processed chunk. API ingress bounds each user-op size, so this overshoot is
    // bounded by:
    //   max_user_ops_per_chunk * (SignedUserOp::max_batch_metadata() + A::MAX_METHOD_PAYLOAD_BYTES)
    pub max_batch_user_op_bytes: usize,

    pub idle_poll_interval: Duration,
}

impl InclusionLaneConfig {
    pub fn for_app<A: Application>(batch_submitter_address: Address) -> Self {
        Self {
            batch_submitter_address,
            max_user_ops_per_chunk: DEFAULT_MAX_USER_OPS_PER_CHUNK,
            safe_input_buffer_capacity: DEFAULT_SAFE_INPUT_BUFFER_CAPACITY,
            max_batch_open: DEFAULT_MAX_BATCH_OPEN,
            max_batch_user_op_bytes: DEFAULT_MAX_BATCH_USER_OP_BYTES
                .max(SignedUserOp::max_batch_metadata() + A::MAX_METHOD_PAYLOAD_BYTES),
            idle_poll_interval: DEFAULT_IDLE_POLL_INTERVAL,
        }
    }
}
