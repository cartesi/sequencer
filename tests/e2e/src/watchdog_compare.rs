// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Watchdog compare harness: Anvil + devnet sequencer + live CM inspect.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rollups_harness::ManagedSequencer;
use rollups_harness::paths;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;

use crate::ScenarioResult;

const MACHINE_IMAGE: &str = "examples/canonical-app/out/canonical-machine-image";
const EMPTY_WALLET_STATE: &str = "{\"balances\":{},\"nonces\":{}}";
const GENESIS_SAFE_BLOCK: &str = "0";

pub async fn run_watchdog_genesis_compare_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    if std::env::var("RUN_WATCHDOG_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping watchdog_genesis_compare_test: set RUN_WATCHDOG_E2E=1 to enable");
        return Ok(());
    }

    let workspace = paths::workspace_root();
    let machine_image = workspace.join(MACHINE_IMAGE);
    if !machine_image.is_dir() {
        return Err(format!(
            "canonical machine image missing at {}; run: just canonical-build-machine-image",
            machine_image.display()
        )
        .into());
    }

    eprintln!("[watchdog-harness] step 1/5: wait for sequencer GET /get_state (safe head)");
    let get_state_url = format!("{}/get_state", runtime.endpoint());
    let (_status, body) =
        wait_for_get_state(get_state_url.as_str(), Duration::from_secs(30)).await?;
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|err| format!("invalid /get_state JSON: {err}; body={body}"))?;
    let sequencer_state = parsed["state"]
        .as_str()
        .ok_or("get_state response missing state field")?;
    let safe_block = parsed["safe_block"]
        .as_u64()
        .ok_or("get_state response missing safe_block field")?;
    eprintln!("[watchdog-harness] sequencer safe_block={safe_block} state={sequencer_state}");
    if sequencer_state != EMPTY_WALLET_STATE {
        return Err(format!(
            "expected genesis-empty wallet state {EMPTY_WALLET_STATE}, got {sequencer_state}"
        )
        .into());
    }

    eprintln!("[watchdog-harness] step 2/5: prove CM inspect JSON on genesis image");
    let inspect_state =
        prove_cm_inspect_genesis(workspace.as_path(), machine_image.as_path()).await?;
    if inspect_state != EMPTY_WALLET_STATE {
        return Err(
            format!("CM inspect expected {EMPTY_WALLET_STATE}, got {inspect_state}").into(),
        );
    }

    eprintln!("[watchdog-harness] step 3/5: prepare watchdog checkpoint dir");
    let checkpoint_dir = tempfile::tempdir()
        .map_err(|err| format!("temp checkpoint dir: {err}"))?
        .keep();

    eprintln!("[watchdog-harness] step 4/5: run Lua compare-mode pass");
    let lua_deps = workspace.join(".deps/lua");
    let compare_status = Command::new("lua")
        .current_dir(workspace.as_path())
        .arg("watchdog/tests/run_compare_once.lua")
        .env("WATCHDOG_LUA_DEPS", lua_deps.as_os_str())
        .env("WATCHDOG_MODE", "compare")
        .env("WATCHDOG_SEQUENCER_URL", runtime.endpoint())
        .env("WATCHDOG_L1_RPC_URL", runtime.l1_endpoint())
        .env(
            "WATCHDOG_INPUTBOX_ADDRESS",
            runtime.input_box_address().to_string(),
        )
        .env("WATCHDOG_APP_ADDRESS", runtime.app_address().to_string())
        .env("WATCHDOG_CHECKPOINT_DIR", checkpoint_dir.as_os_str())
        .env("WATCHDOG_CM_SNAPSHOT_DIR", machine_image.as_os_str())
        .env("WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK", GENESIS_SAFE_BLOCK)
        .env("WATCHDOG_CM_EXECUTABLE", "cartesi-machine")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|err| format!("failed to run watchdog compare lua: {err}"))?;
    if !compare_status.success() {
        return Err(format!("watchdog compare lua exited with {}", compare_status).into());
    }

    eprintln!("[watchdog-harness] step 5/5: compare pass completed successfully");
    Ok(())
}

async fn prove_cm_inspect_genesis(
    workspace: &Path,
    machine_image: &Path,
) -> ScenarioResult<String> {
    let work_dir = tempfile::tempdir().map_err(|err| format!("temp cm work dir: {err}"))?;
    let query_path = work_dir.path().join("inspect-query.bin");
    let report_path = work_dir.path().join("inspect-report-0.bin");
    std::fs::write(query_path.as_path(), b"state")
        .map_err(|err| format!("write inspect query: {err}"))?;

    let status = Command::new("cartesi-machine")
        .current_dir(workspace)
        .arg("--no-rollback")
        .arg(format!("--load={},sharing:none", machine_image.display()))
        .arg(format!(
            "--cmio-inspect-state=query:{},report:{}",
            query_path.display(),
            report_path.display()
        ))
        .arg("--quiet")
        .status()
        .await
        .map_err(|err| format!("cartesi-machine inspect failed to start: {err}"))?;
    if !status.success() {
        return Err(format!("cartesi-machine inspect exited with {status}").into());
    }

    let report = std::fs::read_to_string(report_path.as_path())
        .map_err(|err| format!("read inspect report: {err}"))?;
    if report.contains("inspect endpoint not implemented") {
        return Err(
            "CM dapp is stale (inspect not implemented); rebuild with just canonical-build-machine-image"
                .into(),
        );
    }
    Ok(report)
}

async fn http_get(url: &str) -> std::io::Result<(u16, String)> {
    let remainder = url.strip_prefix("http://").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "only http:// supported in harness",
        )
    })?;
    let (host_port, path) = match remainder.split_once('/') {
        Some((host_port, path)) => (host_port.to_string(), format!("/{path}")),
        None => (remainder.to_string(), "/".to_string()),
    };

    let mut stream = TcpStream::connect(host_port.as_str()).await?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8(raw).map_err(std::io::Error::other)?;
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("missing HTTP body"))?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(500);
    Ok((status, body.to_string()))
}

async fn wait_for_get_state(url: &str, deadline: Duration) -> ScenarioResult<(u16, String)> {
    let started = std::time::Instant::now();
    let mut last = String::new();
    while started.elapsed() < deadline {
        match http_get(url).await {
            Ok((200, body)) => return Ok((200, body)),
            Ok((503, body)) => {
                last = body;
            }
            Ok((status, body)) => {
                return Err(format!("GET /get_state returned HTTP {status}: {body}").into());
            }
            Err(err) => {
                last = err.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(format!("timed out waiting for GET /get_state 200; last response: {last}").into())
}
