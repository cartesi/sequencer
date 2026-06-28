// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! CLI harness: the subcommand parser, dispatch, and R4 exit-code projection,
//! exported once from the library so every app binary inherits them.
//!
//! An app's `main` is ~5 lines: init tracing, then [`run_main`] with a
//! genesis-app factory closure. The factory is only invoked by `setup` (to
//! write the genesis finalized snapshot); `run` and `flush-mempool` never
//! construct an app value.
//!
//! ```ignore
//! #[tokio::main]
//! async fn main() -> std::process::ExitCode {
//!     init_tracing();
//!     sequencer::harness::run_main(|| WalletApp::new(WalletConfig::default())).await
//! }
//! ```
//!
//! Genesis construction stays off the `Application` trait (it varies per impl)
//! and is supplied by this closure. When a future app needs setup-time CLI
//! args of its own (e.g. a machine-image path), the extension point is a
//! `Cli<AppArgs>` generic on the parser — deferred until an app needs it, so
//! the harness imposes no `clap` bound on the (possibly FFI) app type.

use clap::{Parser, Subcommand};
use sequencer_core::application::Application;

use crate::runtime::config::{FlushConfig, RunConfig, SetupConfig};

/// Top-level CLI. Apps parse this (via [`run_main`]) and dispatch.
#[derive(Debug, Parser)]
#[command(
    name = "sequencer",
    version,
    about = "App-specific rollup sequencer.\n\n\
             Subcommands: `setup` (pin identity + initial sync, run once), \
             `run` (boot the sequencer from a set-up DB), and `flush-mempool` \
             (settle the batch-submitter wallet nonce on demand).\n\n\
             All options can also be set via environment variables (shown in brackets)."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The three subcommands. Configs are boxed to keep the enum small (clippy
/// `large_enum_variant`); `RunConfig` is the largest.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Establish the deployment's timeless state: pin identity, do the initial
    /// L1 sync, register the genesis snapshot, and mark setup complete. Run
    /// once before `run`. L1-read-only (takes the submitter address, not the
    /// key).
    Setup(Box<SetupConfig>),
    /// Boot the sequencer from an already-set-up DB. Refuses unless `setup`
    /// completed. Reads identity from the DB; keeps the signing key.
    Run(Box<RunConfig>),
    /// Settle the batch-submitter wallet nonce on demand (operator tool).
    FlushMempool(Box<FlushConfig>),
}

/// Parse argv and dispatch. Returns the R4 process exit code.
pub async fn run_main<A, F>(genesis_app: F) -> std::process::ExitCode
where
    A: Application + Clone + Sync + 'static,
    F: FnOnce() -> A,
{
    let cli = Cli::parse();
    dispatch(cli.command, genesis_app).await
}

/// Dispatch a parsed [`Command`], projecting the result onto the R4 exit-code
/// contract (see [`crate::runtime::error`]). Clean completion is exit 0; every
/// `RunError` maps through `RunError::exit_code`.
///
/// `genesis_app` is called at most once — only by `setup`.
pub async fn dispatch<A, F>(command: Command, genesis_app: F) -> std::process::ExitCode
where
    A: Application + Clone + Sync + 'static,
    F: FnOnce() -> A,
{
    let result = match command {
        Command::Setup(config) => crate::runtime::setup::setup(*config, genesis_app()).await,
        Command::Run(config) => crate::runtime::run::<A>(*config).await,
        Command::FlushMempool(config) => crate::runtime::flush::flush_mempool(*config).await,
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            let code = err.exit_code();
            tracing::error!(error = %err, exit_code = code, "sequencer exiting");
            std::process::ExitCode::from(code)
        }
    }
}
