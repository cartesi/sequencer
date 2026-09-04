// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Host wiring for a C application: the sequencer library over `c-app-engine`'s shim.
//!
//! An application's binary crate is the few lines in `c-wallet-sequencer`, the same shape
//! `wallet-sequencer` has for a Rust application: link an engine, call [`run`].

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
    /// Engine genesis state, read-only and load-bearing for `setup` alone, `run` opens dumps.
    /// Must already hold a deployment written by the application's genesis tool
    #[arg(long, env = "CARTESI_SEQUENCER_STATE_FILE", value_name = "PATH")]
    state_file: PathBuf,
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
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Only `setup` starts from this file, `run` and `flush-mempool` work from the dumps the
    // sequencer took, so demanding it of them would keep a warm deployment from restarting once
    // the genesis state is gone. Opened here rather than inside the closure so a state the engine
    // cannot read names what to do about it instead of raising the closure's panic.
    let mut app = None;
    if matches!(command, sequencer::Command::Setup(_)) {
        match EngineApp::from_dump(&state_file) {
            Ok(engine) => app = Some(engine),
            Err(err) => {
                tracing::error!(state = %state_file.display(),
                    "cannot open the engine state, write one with the application's genesis tool first: {err:?}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Runs only on `setup`, which is the one path that filled `app` above.
    sequencer::dispatch(command, move || app.expect("engine opened for setup")).await
}
