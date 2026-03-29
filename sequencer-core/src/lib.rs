// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

pub mod api;
pub mod application;
pub mod batch;
pub mod broadcast;
pub mod fee;
pub mod l2_tx;
pub mod user_op;

/// Maximum number of L1 blocks a batch can wait before the scheduler considers it stale.
/// Shared between the scheduler (canonical-app) and the sequencer (batch submitter, startup detection).
pub const MAX_WAIT_BLOCKS: u64 = 1200;
