// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use app_core::application::{WalletApp, WalletConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // `setup` is the only subcommand that constructs a genesis app; the
    // closure runs only on that path.
    sequencer::run_main(|| WalletApp::new(WalletConfig::devnet())).await
}
