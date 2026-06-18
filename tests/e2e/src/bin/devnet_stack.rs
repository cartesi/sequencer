// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Local Anvil + rollups devnet + `sequencer-devnet` for manual watchdog runs.
//!
//! Prints `WATCHDOG_*` exports, then blocks until Ctrl+C.

use rollups_harness::{
    HarnessResult, ManagedSequencer, devnet_sequencer_config_no_faketime, paths,
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
        "--- export these, then run: lua watchdog/main.lua init && lua watchdog/main.lua tick ---"
    );
    eprintln!("export WATCHDOG_SEQUENCER_URL={}", runtime.endpoint());
    eprintln!("export WATCHDOG_L1_RPC_URL={}", runtime.l1_endpoint());
    eprintln!(
        "export WATCHDOG_INPUTBOX_ADDRESS={}",
        runtime.input_box_address()
    );
    eprintln!("export WATCHDOG_APP_ADDRESS={}", runtime.app_address());
    eprintln!("export WATCHDOG_STATE_DIR={}", state_dir.display());
    eprintln!(
        "export WATCHDOG_CM_SNAPSHOT_DIR={}",
        machine_image.display()
    );
    eprintln!("export WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK=0");
    eprintln!(
        "export WATCHDOG_LUA_DEPS={}/.deps/lua",
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
    eprintln!("  lua watchdog/main.lua init");
    eprintln!("  lua watchdog/main.lua tick");
    eprintln!();
    eprintln!("Press Ctrl+C here to stop Anvil + sequencer.");

    tokio::signal::ctrl_c()
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    runtime.shutdown().await
}
