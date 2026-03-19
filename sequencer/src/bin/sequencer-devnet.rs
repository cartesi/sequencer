// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use app_core::application::{WalletApp, WalletConfig};
use clap::Parser;
use sequencer::{RunConfig, run};

#[tokio::main]
async fn main() -> Result<(), sequencer::RunError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = RunConfig::parse();
    run(WalletApp::new(WalletConfig::devnet()), config).await
}
