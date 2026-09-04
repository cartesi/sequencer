// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use app_core::application::{WalletApp, WalletConfig};
use std::io::IsTerminal;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // ANSI styling only when a person is watching. `tracing-subscriber`
    // decides styling from `NO_COLOR` alone (no TTY check), so without this
    // a daemon writing to a pipe, a file, or journald interleaves escape
    // codes into its operator log.
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // `setup` is the only subcommand that constructs a genesis app; the
    // closure runs only on that path.
    sequencer::run_main(|| WalletApp::new(WalletConfig::default())).await
}
