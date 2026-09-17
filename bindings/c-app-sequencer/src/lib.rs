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
             application-engine C API. The subcommands come from the sequencer library.\n\n\
             All options can also be set via environment variables (shown in brackets)."
)]
struct Cli {
    /// Genesis dump used only when plain setup needs its initial snapshot.
    /// Created by the application's genesis tool
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

    sequencer::run_command(command, move || {
        let state_file = state_file.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "plain setup requires --state-file or CARTESI_SEQUENCER_STATE_FILE",
            )
        })?;
        EngineApp::from_dump(&state_file)
    })
    .await
}
