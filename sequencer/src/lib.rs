// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Sequencer prototype focused on deterministic inclusion and replay.
//!
//! Flow: API -> inclusion lane -> SQLite -> catch-up replay.
//! The inclusion lane is the single writer that defines execution order.
pub mod api;
pub mod application;
pub mod inclusion_lane;
pub mod l2_tx;
pub mod l2_tx_broadcaster;
pub mod storage;
pub mod user_op;
