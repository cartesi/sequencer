// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Host wiring for a C application: the sequencer library over `c-app-engine`'s shim.
//!
//! An application's binary crate is the few lines in `c-wallet-sequencer`, the same shape
//! `wallet-sequencer` has for a Rust application: link an engine, call [`run`].

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use c_app_engine::{Application, EngineApp};
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// The sequencer library's subcommands plus the one option the host owns, the engine state.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Rollup sequencer host for a C application.\n\n\
             Runs the application engine linked in at build time, the one implementing the \
             application-engine C API. The subcommands come from the sequencer library. \
             `--state-file` gives the genesis state, which only a setup without `--base` \
             reads.\n\n\
             All options can also be set via environment variables (shown in brackets)."
)]
struct Cli {
    /// The application's genesis state, written by its genesis tool. Read only
    /// by a setup without `--base`, which starts the history from it; a setup
    /// from a stored machine takes the state from the machine, and `run` never
    /// reads this.
    #[arg(long, env = "CARTESI_SEQUENCER_STATE_FILE", value_name = "PATH")]
    state_file: Option<PathBuf>,
    #[command(subcommand)]
    command: sequencer::Command,
}

/// Parse this host's arguments and run the sequencer over the linked engine.
pub async fn run() -> ExitCode {
    // Parse first so `--help`/`--version` work without the engine state
    let Cli {
        state_file,
        command,
    } = Cli::parse();

    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // The library calls this only for a setup from genesis.
    sequencer::run_command(command, move || {
        let state_file = state_file.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "setup without --base requires --state-file or CARTESI_SEQUENCER_STATE_FILE",
            )
        })?;
        EngineApp::from_dump(&state_file)
    })
    .await
}
