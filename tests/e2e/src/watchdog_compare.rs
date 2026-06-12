// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Watchdog compare harness: Anvil + devnet sequencer + live CM inspect.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use app_core::application::{WalletApp, WalletConfig};
use app_core::wallet_snapshot;
use rollups_harness::ManagedSequencer;
use rollups_harness::paths;
use sequencer_core::application::Application;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;

use crate::ScenarioResult;

const MACHINE_IMAGE: &str = "examples/canonical-app/out/canonical-machine-image";
const GENESIS_SAFE_BLOCK: &str = "0";

fn require_cartesi_machine() {
    assert!(
        std::process::Command::new("cartesi-machine")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok(),
        "cartesi-machine not found on PATH — install Cartesi tools (or run `nix develop`)"
    );
}

pub async fn run_watchdog_genesis_compare_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    require_cartesi_machine();

    let workspace = paths::workspace_root();
    let machine_image = workspace.join(MACHINE_IMAGE);
    if !machine_image.is_dir() {
        return Err(format!(
            "canonical machine image missing at {}; run: just canonical-build-machine-image",
            machine_image.display()
        )
        .into());
    }

    // `sequencer-devnet` uses `WalletConfig::devnet()` (not `default()` / Sepolia).
    let expected_snapshot = wallet_snapshot::encode(&WalletApp::new(WalletConfig::devnet()));

    eprintln!("[watchdog-harness] step 1/6: wait for sequencer GET /finalized_state");
    let finalized_url = format!("{}/finalized_state", runtime.endpoint());
    let (_status, body, headers) =
        wait_for_finalized_state(finalized_url.as_str(), Duration::from_secs(30)).await?;
    let inclusion_block = header_u64(&headers, "x-inclusion-block")
        .ok_or("finalized_state response missing X-Inclusion-Block header")?;
    eprintln!(
        "[watchdog-harness] sequencer inclusion_block={inclusion_block} snapshot_bytes={}",
        body.len()
    );
    if body.as_slice() != expected_snapshot.as_slice() {
        return Err(format!(
            "finalized_state bytes mismatch (len {} vs expected {})",
            body.len(),
            expected_snapshot.len()
        )
        .into());
    }

    eprintln!("[watchdog-harness] step 2/6: prove CM inspect SSZ on genesis image");
    let inspect_state =
        prove_cm_inspect_genesis(workspace.as_path(), machine_image.as_path()).await?;
    if inspect_state.as_slice() != expected_snapshot.as_slice() {
        return Err(format!(
            "CM inspect bytes mismatch (len {} vs expected {})",
            inspect_state.len(),
            expected_snapshot.len()
        )
        .into());
    }

    eprintln!("[watchdog-harness] step 3/6: prepare watchdog checkpoint dir");
    let checkpoint_dir = tempfile::tempdir()
        .map_err(|err| format!("temp checkpoint dir: {err}"))?
        .keep();

    eprintln!("[watchdog-harness] step 4/6: run Lua compare-mode pass (CLI binding)");
    run_lua_compare(runtime, workspace.as_path(), checkpoint_dir.as_path()).await?;

    eprintln!("[watchdog-harness] step 5/6: run production main.lua pass (machine_cartesi)");
    run_lua_main_compare(runtime, workspace.as_path(), checkpoint_dir.as_path()).await?;

    eprintln!("[watchdog-harness] step 6/6: compare pass completed successfully");
    Ok(())
}

pub async fn run_watchdog_non_genesis_compare_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    require_cartesi_machine();

    let workspace = paths::workspace_root();
    let machine_image = workspace.join(MACHINE_IMAGE);
    if !machine_image.is_dir() {
        return Err(format!(
            "canonical machine image missing at {}; run: just canonical-build-machine-image",
            machine_image.display()
        )
        .into());
    }

    eprintln!("[watchdog-harness] step 1/4: wait for non-genesis GET /finalized_state");
    let finalized_url = format!("{}/finalized_state", runtime.endpoint());
    let (_status, body, headers) = wait_for_non_genesis_finalized_state(
        finalized_url.as_str(),
        runtime,
        Duration::from_secs(60),
    )
    .await?;
    let inclusion_block = header_u64(&headers, "x-inclusion-block")
        .ok_or("finalized_state response missing X-Inclusion-Block header")?;
    eprintln!(
        "[watchdog-harness] finalized non-genesis snapshot inclusion_block={inclusion_block} snapshot_bytes={}",
        body.len()
    );

    let genesis_snapshot = wallet_snapshot::encode(&WalletApp::new(WalletConfig::devnet()));
    if body.as_slice() == genesis_snapshot.as_slice() {
        return Err(
            "non-genesis finalized_state still matches empty devnet genesis snapshot".into(),
        );
    }
    let decoded = wallet_snapshot::decode(body.as_slice())
        .map_err(|err| format!("decode non-genesis finalized_state: {err}"))?;
    if decoded.executed_input_count() == 0 {
        return Err("expected non-genesis finalized_state executed_input_count > 0".into());
    }

    eprintln!("[watchdog-harness] step 2/4: prepare watchdog checkpoint dir");
    let checkpoint_dir = tempfile::tempdir()
        .map_err(|err| format!("temp checkpoint dir: {err}"))?
        .keep();

    eprintln!("[watchdog-harness] step 3/4: run Lua compare-mode pass (CLI binding)");
    run_lua_compare(runtime, workspace.as_path(), checkpoint_dir.as_path()).await?;

    eprintln!("[watchdog-harness] step 4/4: run production main.lua pass (machine_cartesi)");
    run_lua_main_compare(runtime, workspace.as_path(), checkpoint_dir.as_path()).await?;
    Ok(())
}

fn compare_env(
    runtime: &ManagedSequencer,
    workspace: &Path,
    checkpoint_dir: &Path,
    machine_image: &Path,
    lua_deps: &Path,
) -> Vec<(String, String)> {
    vec![
        (
            "WATCHDOG_LUA_DEPS".into(),
            lua_deps.to_string_lossy().into_owned(),
        ),
        ("WATCHDOG_MODE".into(), "compare".into()),
        ("WATCHDOG_ONCE".into(), "1".into()),
        (
            "WATCHDOG_SEQUENCER_URL".into(),
            runtime.endpoint().to_string(),
        ),
        (
            "WATCHDOG_L1_RPC_URL".into(),
            runtime.l1_endpoint().to_string(),
        ),
        (
            "WATCHDOG_INPUTBOX_ADDRESS".into(),
            runtime.input_box_address().to_string(),
        ),
        (
            "WATCHDOG_APP_ADDRESS".into(),
            runtime.app_address().to_string(),
        ),
        (
            "WATCHDOG_CHECKPOINT_DIR".into(),
            checkpoint_dir.to_string_lossy().into_owned(),
        ),
        (
            "WATCHDOG_CM_SNAPSHOT_DIR".into(),
            machine_image.to_string_lossy().into_owned(),
        ),
        (
            "WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK".into(),
            GENESIS_SAFE_BLOCK.into(),
        ),
        ("WATCHDOG_CM_EXECUTABLE".into(), "cartesi-machine".into()),
        (
            "WATCHDOG_CM_WORK_DIR".into(),
            workspace
                .join(".watchdog-cm-work")
                .to_string_lossy()
                .into_owned(),
        ),
    ]
}

async fn run_lua_compare(
    runtime: &mut ManagedSequencer,
    workspace: &Path,
    checkpoint_dir: &Path,
) -> ScenarioResult<()> {
    let machine_image = workspace.join(MACHINE_IMAGE);
    let lua_deps = workspace.join(".deps/lua");
    ensure_lcurl(lua_deps.as_path())?;
    let mut command = Command::new("lua");
    command
        .current_dir(workspace)
        .arg("watchdog/tests/run_compare_once.lua")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (key, value) in compare_env(
        runtime,
        workspace,
        checkpoint_dir,
        machine_image.as_path(),
        lua_deps.as_path(),
    ) {
        command.env(key, value);
    }
    let compare_status = command
        .status()
        .await
        .map_err(|err| format!("failed to run watchdog compare lua: {err}"))?;
    if !compare_status.success() {
        return Err(format!("watchdog compare lua exited with {}", compare_status).into());
    }
    Ok(())
}

async fn run_lua_main_compare(
    runtime: &mut ManagedSequencer,
    workspace: &Path,
    checkpoint_dir: &Path,
) -> ScenarioResult<()> {
    let machine_image = workspace.join(MACHINE_IMAGE);
    let lua_deps = workspace.join(".deps/lua");
    ensure_lcurl(lua_deps.as_path())?;
    std::fs::create_dir_all(workspace.join(".watchdog-cm-work"))
        .map_err(|err| format!("create watchdog CM work dir: {err}"))?;

    let mut command = Command::new("lua");
    command
        .current_dir(workspace)
        .arg("watchdog/main.lua")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (key, value) in compare_env(
        runtime,
        workspace,
        checkpoint_dir,
        machine_image.as_path(),
        lua_deps.as_path(),
    ) {
        command.env(key, value);
    }
    let status = command
        .status()
        .await
        .map_err(|err| format!("failed to run watchdog main.lua: {err}"))?;
    if !status.success() {
        return Err(format!("watchdog main.lua exited with {}", status).into());
    }
    Ok(())
}

fn ensure_lcurl(lua_deps: &Path) -> ScenarioResult<()> {
    let lcurl_so = lua_deps.join("lcurl.so");
    if !lcurl_so.is_file() {
        return Err(format!(
            "lcurl.so missing at {}; run: just watchdog-lua-deps (needs libcurl + Lua dev headers)",
            lcurl_so.display()
        )
        .into());
    }
    Ok(())
}

async fn prove_cm_inspect_genesis(
    workspace: &Path,
    machine_image: &Path,
) -> ScenarioResult<Vec<u8>> {
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

    let report = std::fs::read(report_path.as_path())
        .map_err(|err| format!("read inspect report: {err}"))?;
    if report.starts_with(b"inspect endpoint not implemented".as_slice())
        || report.starts_with("inspect endpoint not implemented".as_bytes())
    {
        return Err(
            "CM dapp is stale (inspect not implemented); rebuild with: just canonical-build-machine-image"
                .into(),
        );
    }
    // Pre-SSZ images returned JSON from export_state (~27 bytes for empty wallet).
    if report.first() == Some(&b'{') {
        return Err(format!(
            "CM inspect returned JSON ({} bytes), expected SSZ; rebuild devnet image: \
             just canonical-build-machine-image (report starts with {:?})",
            report.len(),
            String::from_utf8_lossy(&report[..report.len().min(40)])
        )
        .into());
    }
    Ok(report)
}

async fn http_get(url: &str) -> std::io::Result<(u16, Vec<u8>, Vec<(String, String)>)> {
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
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("missing HTTP body"))?;
    let header_bytes = &raw[..header_end];
    let body_raw = raw[header_end + 4..].to_vec();
    let header_text = std::str::from_utf8(header_bytes).map_err(std::io::Error::other)?;
    let status = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(500);
    let header_pairs: Vec<(String, String)> = header_text
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
        .collect();
    let body = decode_response_body(&header_pairs, body_raw)?;
    Ok((status, body, header_pairs))
}

/// Axum streams snapshot files without `Content-Length`, so bodies are often
/// `Transfer-Encoding: chunked`. Raw TCP clients must decode (lcurl does this
/// automatically; this harness must too).
fn decode_response_body(headers: &[(String, String)], body: Vec<u8>) -> std::io::Result<Vec<u8>> {
    let chunked = headers.iter().any(|(name, value)| {
        name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    if chunked {
        return decode_chunked_body(body.as_slice());
    }
    if let Some(len) = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
    {
        let mut out = body;
        out.truncate(len.min(out.len()));
        return Ok(out);
    }
    Ok(body)
}

fn decode_chunked_body(mut input: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = input
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| std::io::Error::other("chunked body: missing size line"))?;
        let size_line = std::str::from_utf8(&input[..line_end])
            .map_err(std::io::Error::other)?
            .trim_end_matches('\r');
        let chunk_size = usize::from_str_radix(size_line, 16).map_err(std::io::Error::other)?;
        input = &input[line_end + 1..];
        if chunk_size == 0 {
            break;
        }
        if input.len() < chunk_size + 2 {
            return Err(std::io::Error::other("chunked body: truncated chunk"));
        }
        out.extend_from_slice(&input[..chunk_size]);
        input = &input[chunk_size + 2..];
    }
    Ok(out)
}

fn body_snippet_for_error(body: &[u8]) -> String {
    if body.is_empty() {
        return "(empty body)".to_string();
    }
    match std::str::from_utf8(body) {
        Ok(text) if text.len() <= 512 => text.to_string(),
        Ok(text) => format!("{}…", &text[..512.min(text.len())]),
        Err(_) => format!("{} binary octets", body.len()),
    }
}

fn header_u64(headers: &[(String, String)], name: &str) -> Option<u64> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.parse().ok())
}

async fn wait_for_non_genesis_finalized_state(
    url: &str,
    runtime: &ManagedSequencer,
    deadline: Duration,
) -> ScenarioResult<(u16, Vec<u8>, Vec<(String, String)>)> {
    let started = std::time::Instant::now();
    let mut last = String::new();
    while started.elapsed() < deadline {
        match http_get(url).await {
            Ok((200, body, headers)) => {
                let inclusion_block = header_u64(&headers, "x-inclusion-block")
                    .ok_or("finalized_state response missing X-Inclusion-Block header")?;
                if inclusion_block > 0 {
                    return Ok((200, body, headers));
                }
                last = format!("inclusion_block still at 0 (snapshot_bytes={})", body.len());
            }
            Ok((404, body, _)) => {
                last = body_snippet_for_error(body.as_slice());
            }
            Ok((status, body, _)) => {
                return Err(format!(
                    "GET /finalized_state returned HTTP {status}: {}",
                    body_snippet_for_error(body.as_slice())
                )
                .into());
            }
            Err(err) => {
                last = err.to_string();
            }
        }
        runtime
            .mine_l1_blocks(1)
            .await
            .map_err(|err| format!("mine while waiting for non-genesis finalized state: {err}"))?;
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    Err(format!(
        "timed out waiting for GET /finalized_state with inclusion_block > 0; last: {last}"
    )
    .into())
}

async fn wait_for_finalized_state(
    url: &str,
    deadline: Duration,
) -> ScenarioResult<(u16, Vec<u8>, Vec<(String, String)>)> {
    let started = std::time::Instant::now();
    let mut last = String::new();
    while started.elapsed() < deadline {
        match http_get(url).await {
            Ok((200, body, headers)) => return Ok((200, body, headers)),
            Ok((404, body, _)) => {
                last = body_snippet_for_error(body.as_slice());
            }
            Ok((status, body, _)) => {
                return Err(format!(
                    "GET /finalized_state returned HTTP {status}: {}",
                    body_snippet_for_error(body.as_slice())
                )
                .into());
            }
            Err(err) => {
                last = err.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(format!("timed out waiting for GET /finalized_state 200; last response: {last}").into())
}
