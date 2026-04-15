// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Single-lane hot path: dequeues user ops, runs them through the application,
//! persists frame/batch ordering, and rotates frame/batch boundaries.
//!
//! [`InclusionLane::start`] spawns the lane on a blocking thread and returns the
//! input MPSC sender plus the join handle. The lane is the only writer of open
//! batch/frame state in `Storage` — this is the invariant that lets the storage
//! layer trust the in-memory `WriteHead` without per-write sanity checks.

mod catch_up;
mod config;
mod error;
mod lane;
mod types;

pub use config::InclusionLaneConfig;
pub use error::InclusionLaneError;
pub use lane::InclusionLane;
pub use types::{PendingUserOp, SequencerError};

#[cfg(test)]
mod tests;
