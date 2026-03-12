// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Sequencer prototype focused on deterministic inclusion and replay.
//!
//! Flow: API -> inclusion lane -> SQLite -> catch-up replay.
//! The inclusion lane is the single writer that defines execution order.
pub mod api;
pub mod batch_submitter;
pub mod config;
pub mod inclusion_lane;
pub mod input_reader;
pub mod l2_tx_feed;
pub mod onchain;
mod runtime;
pub mod shutdown;
pub mod storage;

pub use config::RunConfig;
pub use runtime::{RunError, run};
