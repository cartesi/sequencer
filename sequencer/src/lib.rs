// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Sequencer prototype focused on deterministic inclusion and replay.
//!
//! Top-level layout follows the system's data flow:
//!
//! - `ingress` — submit API + inclusion lane (write path from external clients)
//! - `egress` — subscribe API + L2-tx feed (read path to internal indexers)
//! - `l1` — input reader, batch submitter, L1 helpers
//! - `storage` — SQLite-backed persistence (organized by writer role)
//! - `recovery` — cascade invalidation + recovery batch
//! - `commands` — the operator command brackets (run / setup / flush) plus
//!   their command-scoped config and error taxonomy
//! - `runtime` — the runtime authority capabilities (process lock, scope)
//! - `http` — shared HTTP error type + axum::serve orchestration
//!
//! The inclusion lane is the single writer of open-batch state; this is the
//! invariant the storage layer relies on.

pub(crate) mod clock;
pub mod commands;
pub mod egress;
pub mod harness;
pub mod http;
pub mod ingress;
pub mod l1;
pub mod recovery;
pub mod runtime;
pub mod storage;

#[cfg(test)]
extern crate self as sequencer;
#[cfg(test)]
mod integration_tests;

pub use commands::config::{FlushConfig, RunConfig, SetupConfig};
pub use commands::error::CommandError;
pub use commands::run::run;
pub use harness::{Cli, Command, dispatch, run_main};
pub use http::{ApiConfig, ApiError, WS_CATCHUP_WINDOW_EXCEEDED_REASON};
