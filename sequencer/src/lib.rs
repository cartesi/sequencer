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
//! - `runtime` — orchestration, config, shutdown
//! - `http` — shared HTTP error type + axum::serve orchestration
//!
//! The inclusion lane is the single writer of open-batch state; this is the
//! invariant the storage layer relies on.

pub mod egress;
pub mod harness;
pub mod http;
pub mod ingress;
pub mod l1;
pub mod recovery;
pub mod runtime;
pub mod storage;

pub use harness::{Cli, Command, dispatch, run_main};
pub use http::{ApiConfig, ApiError, WS_CATCHUP_WINDOW_EXCEEDED_REASON};
pub use runtime::config::{FlushConfig, RunConfig, SetupConfig};
pub use runtime::{RunError, run};
