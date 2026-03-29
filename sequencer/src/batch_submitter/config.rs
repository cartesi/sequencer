// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

/// Batch-submitter-specific options. L1 RPC URL and InputBox address are shared with the
/// input reader and come from the same discovery at startup (see `L1Config` in `config`).
/// These fields are parsed as part of `RunConfig` and passed through at runtime.
#[derive(Debug, Clone)]
pub struct BatchSubmitterConfig {
    /// How often the submitter polls for new work when idle.
    pub idle_poll_interval_ms: u64,
    /// Maximum L1 blocks a batch can wait before being considered stale.
    pub max_wait_blocks: u64,
    /// Blocks before MAX_WAIT to trigger preemptive recovery.
    /// Danger threshold = max_wait_blocks - preemptive_margin_blocks.
    pub preemptive_margin_blocks: u64,
    /// Assumed L1 block time in seconds, used for wall-clock danger estimation
    /// when the provider is unreachable.
    pub seconds_per_block: u64,
}

impl BatchSubmitterConfig {
    pub fn idle_poll_interval(&self) -> Duration {
        Duration::from_millis(self.idle_poll_interval_ms)
    }

    /// The block-age threshold at which preemptive recovery triggers.
    pub fn danger_threshold(&self) -> u64 {
        self.max_wait_blocks
            .saturating_sub(self.preemptive_margin_blocks)
    }
}
