// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Local Anvil + rollups devnet + `wallet-sequencer-devnet` for manual watchdog runs.
//!
//! Prints `CARTESI_WATCHDOG_*` exports, then blocks until Ctrl+C.

use rollups_harness::{
    DEVNET_CHAIN_ID, HarnessResult, ManagedSequencer, devnet_sequencer_config_no_faketime, paths,
};

#[tokio::main]
async fn main() -> HarnessResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime =
        ManagedSequencer::spawn(devnet_sequencer_config_no_faketime("devnet-stack")).await?;

    let machine_image = paths::devnet_machine_image_path();
    let state_dir = std::env::temp_dir().join("watchdog-state-devnet");

    eprintln!();
    eprintln!("=== Devnet stack is up ===");
    eprintln!("Sequencer HTTP:  {}", runtime.endpoint());
    eprintln!("L1 RPC:          {}", runtime.l1_endpoint());
    eprintln!("App address:     {}", runtime.app_address());
    eprintln!("InputBox:        {}", runtime.input_box_address());
    eprintln!();
    eprintln!(
        "--- export these, then run: ./watchdog/sequencer-watchdog init && ./watchdog/sequencer-watchdog tick ---"
    );
    eprintln!(
        "export CARTESI_WATCHDOG_SEQUENCER_URL={}",
        runtime.endpoint()
    );
    eprintln!(
        "export CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT={}",
        runtime.l1_endpoint()
    );
    eprintln!("export CARTESI_WATCHDOG_BLOCKCHAIN_ID={DEVNET_CHAIN_ID}");
    eprintln!(
        "export CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS={}",
        runtime.input_box_address()
    );
    eprintln!(
        "export CARTESI_WATCHDOG_APP_ADDRESS={}",
        runtime.app_address()
    );
    eprintln!("export CARTESI_WATCHDOG_STATE_DIR={}", state_dir.display());
    eprintln!(
        "export CARTESI_WATCHDOG_CM_SNAPSHOT_DIR={}",
        machine_image.display()
    );
    eprintln!("export CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK=0");
    eprintln!(
        "export CARTESI_WATCHDOG_LUA_DEPS={}/.deps/lua",
        paths::workspace_root().display()
    );
    eprintln!();
    eprintln!("Wait for finalized snapshot (404 until promotion):");
    eprintln!(
        "  curl -s {}/finalized_state/inclusion_block",
        runtime.endpoint()
    );
    eprintln!();
    eprintln!("Run watchdog (from repo root, after `just watchdog-lua-deps`):");
    eprintln!("  export CARTESI_WATCHDOG_LUA_ROOT=$(pwd)");
    eprintln!("  export CARTESI_WATCHDOG_LUA_BIN=lua");
    eprintln!("  ./watchdog/sequencer-watchdog init");
    eprintln!("  ./watchdog/sequencer-watchdog tick");
    eprintln!();
    eprintln!("Press Ctrl+C here to stop Anvil + sequencer.");

    tokio::signal::ctrl_c()
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    runtime.shutdown().await
}
