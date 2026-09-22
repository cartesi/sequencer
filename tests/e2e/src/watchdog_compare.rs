// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Watchdog scenarios against a live devnet: Anvil, the wallet sequencer, and
//! the canonical machine image. The watchdog runs through its production
//! wrapper; the divergence drill walks the incident runbook
//! (docs/watchdog/incident-runbook.md) with the real commands.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use app_core::application::{WalletApp, WalletConfig};
use app_core::wallet_snapshot;
use rollups_harness::{DEVNET_CHAIN_ID, ManagedSequencer, paths};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;

use crate::ScenarioResult;

const DEVNET_MACHINE_IMAGE: &str = "examples/canonical-app/out/canonical-machine-image";
const SEPOLIA_MACHINE_IMAGE: &str = "examples/canonical-app/out/canonical-machine-image-sepolia";

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

fn machine_image(relative: &str) -> ScenarioResult<PathBuf> {
    let image = paths::workspace_root().join(relative);
    if !image.is_dir() {
        return Err(format!(
            "machine image missing at {}; run: just canonical-build-machine-image(-sepolia)",
            image.display()
        )
        .into());
    }
    Ok(image)
}

/// One watchdog state directory, bootstrapped from `image` at `block`, driven
/// through the production `sequencer-watchdog` wrapper.
struct Watchdog {
    state_dir: PathBuf,
    image: PathBuf,
    block: u64,
}

impl Watchdog {
    fn new(image: PathBuf, block: u64) -> ScenarioResult<Self> {
        let state_dir = tempfile::tempdir()
            .map_err(|err| format!("temp watchdog state dir: {err}"))?
            .keep();
        Ok(Self {
            state_dir,
            image,
            block,
        })
    }

    async fn run(
        &self,
        runtime: &ManagedSequencer,
        args: &[&str],
    ) -> ScenarioResult<std::process::Output> {
        let workspace = paths::workspace_root();
        let lua_deps = workspace.join(".deps/lua");
        for module in ["lcurl.so", "lfs.so"] {
            if !lua_deps.join(module).is_file() {
                return Err(format!(
                    "{module} missing in {}; run: just watchdog-lua-deps",
                    lua_deps.display()
                )
                .into());
            }
        }
        let mut command = Command::new(workspace.join("watchdog/sequencer-watchdog"));
        command
            .current_dir(&workspace)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CARTESI_WATCHDOG_LUA_ROOT", &workspace)
            .env("CARTESI_WATCHDOG_LUA_DEPS", &lua_deps)
            .env("CARTESI_WATCHDOG_STATE_DIR", &self.state_dir)
            .env("CARTESI_WATCHDOG_SEQUENCER_URL", runtime.endpoint())
            .env(
                "CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT",
                runtime.l1_endpoint(),
            )
            .env(
                "CARTESI_WATCHDOG_BLOCKCHAIN_ID",
                DEVNET_CHAIN_ID.to_string(),
            )
            .env(
                "CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS",
                runtime.input_box_address().to_string(),
            )
            .env(
                "CARTESI_WATCHDOG_APP_ADDRESS",
                runtime.app_address().to_string(),
            )
            .env("CARTESI_WATCHDOG_STATE_SOURCE", "inspect")
            .env("CARTESI_WATCHDOG_CM_SNAPSHOT_DIR", &self.image)
            .env(
                "CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK",
                self.block.to_string(),
            );
        let output = command
            .output()
            .await
            .map_err(|err| format!("failed to run sequencer-watchdog: {err}"))?;
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        Ok(output)
    }

    /// Run a command that must exit with `code`; returns its stdout.
    async fn expect(
        &self,
        runtime: &ManagedSequencer,
        args: &[&str],
        code: i32,
    ) -> ScenarioResult<String> {
        let output = self.run(runtime, args).await?;
        if output.status.code() != Some(code) {
            return Err(format!(
                "sequencer-watchdog {} exited with {}, expected {code}",
                args.join(" "),
                output.status
            )
            .into());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn status(&self, runtime: &ManagedSequencer) -> ScenarioResult<serde_json::Value> {
        let stdout = self.expect(runtime, &["status"], 0).await?;
        serde_json::from_str(&stdout).map_err(|err| format!("status JSON: {err}: {stdout}").into())
    }

    fn wipe(&self) -> ScenarioResult<()> {
        std::fs::remove_dir_all(&self.state_dir)
            .map_err(|err| format!("wipe watchdog state: {err}").into())
    }
}

/// The sequencer's accepted block and comparison-file digest.
async fn sequencer_digest(runtime: &ManagedSequencer) -> ScenarioResult<(u64, String)> {
    let url = format!("{}/finalized_state/digest", runtime.endpoint());
    let (status, body, _) = http_get(&url)
        .await
        .map_err(|err| format!("GET {url}: {err}"))?;
    if status != 200 {
        return Err(format!(
            "GET {url} returned HTTP {status}: {}",
            body_snippet_for_error(&body)
        )
        .into());
    }
    let digest: serde_json::Value =
        serde_json::from_slice(&body).map_err(|err| format!("digest JSON: {err}"))?;
    let block = digest["inclusion_block"]
        .as_u64()
        .ok_or("digest without inclusion_block")?;
    let sha256 = digest["sha256"]
        .as_str()
        .ok_or("digest without sha256")?
        .to_string();
    Ok((block, sha256))
}

fn head_block(status: &serde_json::Value) -> Option<u64> {
    status["head"]["block"].as_u64()
}

pub async fn run_watchdog_genesis_compare_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    require_cartesi_machine();
    let image = machine_image(DEVNET_MACHINE_IMAGE)?;

    // `wallet-sequencer-devnet` uses `WalletConfig::devnet()` (not `default()` / Sepolia).
    let expected_snapshot = wallet_snapshot::encode(&WalletApp::new(WalletConfig::devnet()));

    eprintln!("[watchdog-harness] step 1/4: the sequencer serves the devnet genesis state");
    let finalized_url = format!("{}/finalized_state", runtime.endpoint());
    let (_status, body, headers) =
        wait_for_finalized_state(finalized_url.as_str(), Duration::from_secs(30)).await?;
    let inclusion_block = header_u64(&headers, "x-inclusion-block")
        .ok_or("finalized_state response missing X-Inclusion-Block header")?;
    if body.as_slice() != expected_snapshot.as_slice() {
        return Err(format!(
            "finalized_state bytes mismatch (len {} vs expected {})",
            body.len(),
            expected_snapshot.len()
        )
        .into());
    }

    eprintln!(
        "[watchdog-harness] step 2/4: the canonical image answers the state query with the same bytes"
    );
    let inspect_state = cm_inspect_state(image.as_path()).await?;
    if inspect_state.as_slice() != expected_snapshot.as_slice() {
        return Err(format!(
            "CM inspect bytes mismatch (len {} vs expected {})",
            inspect_state.len(),
            expected_snapshot.len()
        )
        .into());
    }

    eprintln!("[watchdog-harness] step 3/4: init checks the image against the on-chain template");
    let watchdog = Watchdog::new(image, 0)?;
    watchdog.expect(runtime, &["init"], 0).await?;

    eprintln!("[watchdog-harness] step 4/4: ticks idle while the accepted block stays at genesis");
    watchdog.expect(runtime, &["tick"], 0).await?;
    watchdog.expect(runtime, &["tick"], 0).await?;
    let status = watchdog.status(runtime).await?;
    if head_block(&status) != Some(inclusion_block) || status["last_tick"]["outcome"] != "idle" {
        return Err(
            format!("expected an idle watchdog at block {inclusion_block}: {status}").into(),
        );
    }
    Ok(())
}

pub async fn run_watchdog_non_genesis_compare_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    require_cartesi_machine();
    let image = machine_image(DEVNET_MACHINE_IMAGE)?;

    eprintln!("[watchdog-harness] step 1/3: wait for a non-genesis accepted checkpoint");
    let finalized_url = format!("{}/finalized_state", runtime.endpoint());
    wait_for_non_genesis_finalized_state(finalized_url.as_str(), runtime, Duration::from_secs(60))
        .await?;
    let (block, _) = sequencer_digest(runtime).await?;

    eprintln!("[watchdog-harness] step 2/3: init at genesis and replay to block {block}");
    let watchdog = Watchdog::new(image, 0)?;
    watchdog.expect(runtime, &["init"], 0).await?;
    watchdog.expect(runtime, &["tick"], 0).await?;
    let status = watchdog.status(runtime).await?;
    let agreed = head_block(&status).ok_or("status without a head")?;
    if agreed < block || status["last_tick"]["outcome"] != "agreed" {
        return Err(format!("expected agreement at block {block} or later: {status}").into());
    }

    eprintln!("[watchdog-harness] step 3/3: a repeated tick is idle or agrees again");
    watchdog.expect(runtime, &["tick"], 0).await?;
    Ok(())
}

/// The divergence runbook, end to end: a watchdog bootstrapped from the wrong
/// machine latches a mismatch; `status` shows it; `clear` refuses the wrong
/// block; `replay` from the trusted image reproduces the sequencer's digest,
/// which places the fault on the watchdog; clearing without a fix re-latches;
/// re-initializing from the trusted image agrees.
pub async fn run_watchdog_divergence_drill_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    require_cartesi_machine();
    let trusted = machine_image(DEVNET_MACHINE_IMAGE)?;
    let wrong = machine_image(SEPOLIA_MACHINE_IMAGE)?;

    let finalized_url = format!("{}/finalized_state", runtime.endpoint());
    wait_for_non_genesis_finalized_state(finalized_url.as_str(), runtime, Duration::from_secs(60))
        .await?;
    let (block, _) = sequencer_digest(runtime).await?;
    let first_input_block = *runtime
        .input_blocks()
        .await
        .map_err(|err| format!("read InputBox inputs: {err}"))?
        .first()
        .ok_or("no InputBox inputs before the accepted checkpoint")?;
    eprintln!(
        "[watchdog-harness] drill target block={block} first input block={first_input_block}"
    );

    eprintln!("[watchdog-harness] drill 1/7: init refuses a non-template image at genesis");
    let refused = Watchdog::new(wrong.clone(), 0)?;
    let output = refused.run(runtime, &["init"]).await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() != Some(1)
        || !stderr.contains("differs from the on-chain template hash")
    {
        return Err(
            format!("expected init to refuse the sepolia image at genesis: {stderr}").into(),
        );
    }

    eprintln!(
        "[watchdog-harness] drill 2/7: a wrong non-genesis bootstrap latches a state mismatch"
    );
    let watchdog = Watchdog::new(wrong, first_input_block)?;
    watchdog.expect(runtime, &["init"], 0).await?;
    watchdog.expect(runtime, &["tick"], 2).await?;
    watchdog.expect(runtime, &["tick"], 2).await?;

    eprintln!("[watchdog-harness] drill 3/7: status shows the latched incident and its evidence");
    let status = watchdog.status(runtime).await?;
    let divergence = &status["divergence"];
    if status["latched"] != true
        || divergence["kind"] != "state_mismatch"
        || divergence["evidence"]["canonical_machine"] != "incident/canonical"
    {
        return Err(format!("unexpected latched status: {status}").into());
    }
    let latched_block = divergence["target_block"]
        .as_u64()
        .ok_or("marker without target_block")?;

    eprintln!("[watchdog-harness] drill 4/7: clear refuses another block");
    watchdog
        .expect(
            runtime,
            &[
                "clear",
                "--block",
                &(latched_block + 1).to_string(),
                "--reason",
                "drill",
            ],
            1,
        )
        .await?;

    eprintln!(
        "[watchdog-harness] drill 5/7: replay from the trusted image reproduces the sequencer"
    );
    let out = tempfile::tempdir().map_err(|err| format!("replay dir: {err}"))?;
    let replayed = out.path().join("replayed");
    let stdout = watchdog
        .expect(
            runtime,
            &[
                "replay",
                "--from",
                &trusted.to_string_lossy(),
                "--from-block",
                "0",
                "--to-block",
                &latched_block.to_string(),
                "--out",
                &replayed.to_string_lossy(),
            ],
            0,
        )
        .await?;
    let replay: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|err| format!("replay JSON: {err}: {stdout}"))?;
    // The marker records the digest the sequencer served for the latched block.
    if replay["sha256"] != divergence["sequencer_sha256"] {
        return Err(format!(
            "replay from the trusted image disagrees with the sequencer: {replay}"
        )
        .into());
    }

    eprintln!("[watchdog-harness] drill 6/7: clearing without a fix re-latches");
    watchdog
        .expect(
            runtime,
            &[
                "clear",
                "--block",
                &latched_block.to_string(),
                "--reason",
                "drill",
            ],
            0,
        )
        .await?;
    watchdog.expect(runtime, &["tick"], 2).await?;

    eprintln!("[watchdog-harness] drill 7/7: re-initializing from the trusted image agrees");
    watchdog.wipe()?;
    let fixed = Watchdog {
        state_dir: watchdog.state_dir.clone(),
        image: trusted,
        block: 0,
    };
    fixed.expect(runtime, &["init"], 0).await?;
    fixed.expect(runtime, &["tick"], 0).await?;
    if fixed.status(runtime).await?["latched"] != false {
        return Err("re-initialized watchdog is latched".into());
    }
    Ok(())
}

/// The canonical image's answer to the `state` inspect query, via the CLI.
async fn cm_inspect_state(machine_image: &Path) -> ScenarioResult<Vec<u8>> {
    let work_dir = tempfile::tempdir().map_err(|err| format!("temp cm work dir: {err}"))?;
    let query_path = work_dir.path().join("inspect-query.bin");
    let report_path = work_dir.path().join("inspect-report-0.bin");
    std::fs::write(query_path.as_path(), b"state")
        .map_err(|err| format!("write inspect query: {err}"))?;
    let status = Command::new("cartesi-machine")
        .arg("--no-revert")
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
    std::fs::read(report_path.as_path()).map_err(|err| format!("read inspect report: {err}").into())
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
