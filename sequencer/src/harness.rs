// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! CLI harness: the subcommand parser, dispatch, and exit-code projection,
//! exported once from the library so every app binary inherits them.
//!
//! An app's `main` is ~5 lines: init tracing, then [`run_main`] with a
//! genesis-app factory closure. The factory is invoked only when plain `setup`
//! reaches genesis-snapshot registration; completed setup no-ops, recovery,
//! `run`, and `flush-mempool` never construct an app value.
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

use crate::commands::config::{FlushConfig, RunConfig, SetupConfig};

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

/// Parse argv and dispatch. Returns the process exit code.
pub async fn run_main<A, F>(genesis_app: F) -> std::process::ExitCode
where
    A: Application + Clone + Sync + 'static,
    F: FnOnce() -> A + Send + 'static,
{
    let cli = Cli::parse();
    project_dispatch_join(tokio::spawn(dispatch(cli.command, genesis_app)).await)
}

fn project_dispatch_join(
    result: Result<std::process::ExitCode, tokio::task::JoinError>,
) -> std::process::ExitCode {
    match result {
        Ok(code) => code,
        Err(join) if join.is_panic() => {
            tracing::error!(
                error = %join,
                exit_code = crate::commands::error::EXIT_TERMINAL,
                "sequencer command panicked — trusted-code invariant failure"
            );
            std::process::ExitCode::from(crate::commands::error::EXIT_TERMINAL)
        }
        Err(join) => {
            tracing::error!(
                error = %join,
                exit_code = crate::commands::error::EXIT_UNCLASSIFIED,
                "sequencer command task failed"
            );
            std::process::ExitCode::from(crate::commands::error::EXIT_UNCLASSIFIED)
        }
    }
}

/// Dispatch a parsed [`Command`], projecting the result onto the exit-code
/// contract (see [`crate::commands::error`]). Clean completion is exit 0; every
/// `CommandError` maps through `CommandError::exit_code`.
///
/// `genesis_app` is called at most once — only when plain `setup` needs to
/// register the genesis snapshot.
pub async fn dispatch<A, F>(command: Command, genesis_app: F) -> std::process::ExitCode
where
    A: Application + Clone + Sync + 'static,
    F: FnOnce() -> A,
{
    let result = match command {
        Command::Setup(config) => crate::commands::setup::setup(*config, genesis_app).await,
        Command::Run(config) => crate::commands::run::run::<A>(*config).await,
        Command::FlushMempool(config) => crate::commands::flush::flush_mempool(*config).await,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_setup_does_not_construct_genesis_app() {
        use crate::commands::test_support::SweepTestApp;
        use crate::storage::{LifecycleCommand, Storage};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let data_dir = tempfile::tempdir().expect("create data dir");
        let data_dir = data_dir.path().to_string_lossy().into_owned();
        let cli = Cli::try_parse_from([
            "sequencer",
            "setup",
            "--data-dir",
            data_dir.as_str(),
            "--eth-rpc-url",
            "http://127.0.0.1:1",
            "--chain-id",
            "31337",
            "--app-address",
            "0x1111111111111111111111111111111111111111",
            "--batch-submitter-address",
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
        ])
        .expect("parse setup");
        let db_path = match &cli.command {
            Command::Setup(config) => config.db_path(),
            other => panic!("expected setup subcommand, got {other:?}"),
        };

        let mut storage = Storage::initialize_for_command(&db_path, LifecycleCommand::Setup)
            .expect("initialize setup");
        storage
            .insert_initial_finalized_dump(
                &std::path::Path::new(&data_dir).join("finalized"),
                0,
                0,
                0,
                0,
            )
            .expect("register finalized snapshot");
        storage.complete_setup().expect("complete setup");
        drop(storage);

        let constructions = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&constructions);
        let exit = dispatch::<SweepTestApp, _>(cli.command, move || {
            observed.fetch_add(1, Ordering::SeqCst);
            SweepTestApp
        })
        .await;

        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        assert_eq!(constructions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn top_level_command_panic_maps_to_terminal_exit() {
        let result = tokio::spawn(async {
            panic!("trusted-code invariant failure");
            #[allow(unreachable_code)]
            std::process::ExitCode::SUCCESS
        })
        .await;

        assert_eq!(
            project_dispatch_join(result),
            std::process::ExitCode::from(crate::commands::error::EXIT_TERMINAL)
        );
    }
}
