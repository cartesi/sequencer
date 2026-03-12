// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use clap::Parser;

/// Batch-submitter-specific options. L1 RPC URL and InputBox address are shared with the
/// input reader and come from the same discovery at startup (see `L1Config` in `config`).
#[derive(Debug, Clone, Parser)]
pub struct BatchSubmitterConfig {
    /// How often the submitter polls storage and L1 for new work.
    #[clap(
        long = "seq-batch-submitter-idle-poll-interval-ms",
        env = "SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS",
        default_value = "5000"
    )]
    pub idle_poll_interval_ms: u64,

    /// Maximum number of batches to submit in a single loop iteration.
    #[clap(
        long = "seq-batch-submitter-max-batches-per-loop",
        env = "SEQ_BATCH_SUBMITTER_MAX_BATCHES_PER_LOOP",
        default_value = "4"
    )]
    pub max_batches_per_loop: usize,
}

impl BatchSubmitterConfig {
    pub fn idle_poll_interval(&self) -> Duration {
        Duration::from_millis(self.idle_poll_interval_ms)
    }
}
