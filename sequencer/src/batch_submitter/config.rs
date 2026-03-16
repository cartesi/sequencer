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
}

impl BatchSubmitterConfig {
    pub fn idle_poll_interval(&self) -> Duration {
        Duration::from_millis(self.idle_poll_interval_ms)
    }
}
