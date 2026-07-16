// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Local Anvil + rollups devnet + `wallet-sequencer-devnet` for manual watchdog runs.
//!
//! Prints `CARTESI_WATCHDOG_*` exports, then blocks until Ctrl+C or a child exits.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use rollups_harness::{
    DEVNET_CHAIN_ID, HarnessResult, ManagedSequencer, StackChildExit,
    devnet_sequencer_config_no_faketime, paths,
};

const CHILD_POLL: Duration = Duration::from_secs(5);
const HEARTBEAT_EVERY: u32 = 12; // 12 * 5s = 60s
const LOG_TAIL_LINES: usize = 40;

#[tokio::main]
async fn main() -> HarnessResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut runtime =
        ManagedSequencer::spawn(devnet_sequencer_config_no_faketime("devnet-stack")).await?;

    let machine_image = paths::devnet_machine_image_path();
    let state_dir = std::env::temp_dir().join("watchdog-state-devnet");
    let logs_glob = paths::resolve_from_workspace(std::path::Path::new("tests/e2e/results"));

    eprintln!();
    eprintln!("=== Devnet stack is up ===");
    eprintln!("Sequencer HTTP:  {}", runtime.endpoint());
    eprintln!("L1 RPC:          {}", runtime.l1_endpoint());
    eprintln!("App address:     {}", runtime.app_address());
    eprintln!("InputBox:        {}", runtime.input_box_address());
    eprintln!("Sequencer logs:  {}", runtime.log_path().display());
    eprintln!("Anvil/other logs: {}/devnet-stack-*", logs_glob.display());
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
    eprintln!(
        "Note: ports are ephemeral. After restarting this stack, re-export the printed URLs."
    );
    eprintln!(
        "Tick can override a stale persisted sequencer URL with CARTESI_WATCHDOG_SEQUENCER_URL;"
    );
    eprintln!("a new Anvil history still needs a fresh state dir + init.");
    eprintln!();
    eprintln!("Leave this terminal running. Ctrl+C stops Anvil + sequencer.");
    eprintln!(
        "If Anvil or the sequencer exits on its own, this process prints status + log paths and exits."
    );
    eprintln!(
        "[devnet-stack] ready at {}; waiting for Ctrl+C or child exit",
        runtime.endpoint()
    );

    let mut polls_since_heartbeat = 0u32;
    loop {
        tokio::select! {
            ctrl = tokio::signal::ctrl_c() => {
                ctrl.map_err(|err| std::io::Error::other(err.to_string()))?;
                eprintln!("Shutting down Anvil + sequencer...");
                return runtime.shutdown().await;
            }
            observed = runtime.observe_stack_for(CHILD_POLL) => {
                match observed? {
                    Some((which, status)) => {
                        report_child_exit(&runtime, which, status);
                        let _ = runtime.shutdown().await;
                        let label = match which {
                            StackChildExit::Sequencer => "sequencer",
                            StackChildExit::Anvil => "anvil",
                        };
                        return Err(std::io::Error::other(format!(
                            "{label} exited unexpectedly ({status}); see logs above"
                        ))
                        .into());
                    }
                    None => {
                        polls_since_heartbeat += 1;
                        if polls_since_heartbeat >= HEARTBEAT_EVERY {
                            polls_since_heartbeat = 0;
                            eprintln!(
                                "[devnet-stack] still running ({})",
                                runtime.endpoint()
                            );
                        }
                    }
                }
            }
        }
    }
}

fn report_child_exit(runtime: &ManagedSequencer, which: StackChildExit, status: ExitStatus) {
    let (label, log_label, log_path) = match which {
        StackChildExit::Sequencer => ("sequencer", "Sequencer log", runtime.log_path()),
        StackChildExit::Anvil => ("anvil", "Anvil log", runtime.anvil_log_path()),
    };
    eprintln!();
    eprintln!("=== {label} exited unexpectedly ({status}) ===");
    eprintln!("{log_label}: {}", log_path.display());
    eprintln!(
        "Other logs under: {}/devnet-stack-*",
        paths::resolve_from_workspace(std::path::Path::new("tests/e2e/results")).display()
    );
    eprintln!("Tail last lines:");
    match tail_last_lines(log_path, LOG_TAIL_LINES) {
        Ok(lines) => {
            for line in lines {
                eprintln!("  {line}");
            }
        }
        Err(err) => {
            eprintln!("  (could not read log: {err})");
        }
    }
    eprintln!();
}

fn tail_last_lines(path: &Path, max_lines: usize) -> std::io::Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines = VecDeque::with_capacity(max_lines);
    for line in reader.lines() {
        let line = line?;
        if lines.len() == max_lines {
            lines.pop_front();
        }
        lines.push_back(line);
    }
    Ok(lines.into_iter().collect())
}
