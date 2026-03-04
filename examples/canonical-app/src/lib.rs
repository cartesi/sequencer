// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

pub mod scheduler;

pub use scheduler::{SEQUENCER_ADDRESS, SchedulerConfig, run_scheduler_forever};
