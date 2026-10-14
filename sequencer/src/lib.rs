// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Sequencer library: deterministic inclusion, L1 reconciliation and replay for
//! one application. App binaries compose it through [`harness`]; the module map
//! is in AGENTS.md ("Sequencer module layout").

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
#[cfg(test)]
pub(crate) mod test_support;

pub use commands::config::{FlushConfig, RunConfig, SetupConfig};
pub use commands::error::CommandError;
pub use commands::run::run;
pub use harness::{Cli, Command, dispatch, run_command, run_main};
pub use http::{ApiConfig, ApiError};
