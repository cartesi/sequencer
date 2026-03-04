// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy::network::EthereumWallet;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy_primitives::{Address, B256, Bytes};
use app_core::application::default_private_keys;
use cartesi_rollups_contracts::application_factory::ApplicationFactory;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::{BenchResult, BenchmarkDomain, SELF_CONTAINED_DOMAIN_CHAIN_ID};

pub const DEFAULT_SEQUENCER_BIN: &str = "target/release/sequencer";
pub const DEFAULT_MEMORY_SAMPLE_INTERVAL_MS: u64 = 500;
pub const DEFAULT_SEQUENCER_LOGS_DIR: &str = "benchmarks/results";
pub const DEFAULT_ANVIL_STATE_DIR: &str = "benchmarks/.deps/rollups-contracts-2.2.0-anvil-v1.4.3";
pub const DEFAULT_TEMPLATE_MACHINE_IMAGE_PATH: &str =
    "examples/canonical-app/out/canonical-machine-image";

const DEFAULT_SEQUENCER_START_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_SEQUENCER_RUST_LOG: &str = "info";
const DEFAULT_ANVIL_START_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_ANVIL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
#[derive(Debug, Clone)]
pub struct ManagedSequencerConfig {
    pub sequencer_bin: String,
    pub log_prefix: &'static str,
}

pub struct ManagedSequencer {
    anvil: ManagedAnvil,
    child: Child,
    shutdown_timeout: Duration,
    temp_dir: Option<TempDir>,
    pub endpoint: String,
    domain: BenchmarkDomain,
    log_path: PathBuf,
}

impl ManagedSequencer {
    pub async fn spawn(config: ManagedSequencerConfig) -> BenchResult<Self> {
        let (endpoint, http_addr) = build_local_endpoint()?;
        let anvil = ManagedAnvil::spawn(config.log_prefix).await?;
        let domain = anvil.domain();

        let dir = tempfile::tempdir()?;
        let db_path = dir_path_join(dir.path(), "sequencer.db");
        let temp_dir = Some(dir);

        let log_path = default_sequencer_log_path(config.log_prefix);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stdout_log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(log_path.as_path())?;
        let stderr_log = stdout_log.try_clone()?;

        let mut child = Command::new(config.sequencer_bin.as_str())
            .arg("--http-addr")
            .arg(http_addr)
            .arg("--db-path")
            .arg(path_as_str(db_path.as_path())?)
            .arg("--eth-rpc-url")
            .arg(anvil.endpoint.as_str())
            .arg("--domain-chain-id")
            .arg(domain.chain_id.to_string())
            .arg("--domain-verifying-contract")
            .arg(domain.verifying_contract.to_string())
            .env("RUST_LOG", DEFAULT_SEQUENCER_RUST_LOG)
            .stdout(Stdio::from(stdout_log))
            .stderr(Stdio::from(stderr_log))
            .spawn()
            .map_err(|err| {
                io_other(format!(
                    "failed to spawn sequencer binary '{}': {err}",
                    config.sequencer_bin
                ))
            })?;

        wait_for_readiness(
            endpoint.as_str(),
            &mut child,
            DEFAULT_SEQUENCER_START_TIMEOUT,
        )
        .await?;

        Ok(Self {
            anvil,
            child,
            shutdown_timeout: DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT,
            temp_dir,
            endpoint,
            domain,
            log_path,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn log_path(&self) -> &Path {
        self.log_path.as_path()
    }

    pub fn domain(&self) -> BenchmarkDomain {
        self.domain
    }

    pub async fn shutdown(mut self) -> BenchResult<()> {
        let _ = self.temp_dir.take();
        send_graceful_terminate(&mut self.child).await;
        let sequencer_result: BenchResult<()> =
            match tokio::time::timeout(self.shutdown_timeout, self.child.wait()).await {
                Ok(wait_result) => {
                    let _ = wait_result?;
                    Ok(())
                }
                Err(_) => {
                    self.child.start_kill()?;
                    let _ = self.child.wait().await;
                    Ok(())
                }
            };
        let anvil_result = self.anvil.shutdown().await;
        sequencer_result?;
        anvil_result
    }
}

struct ManagedAnvil {
    child: Child,
    shutdown_timeout: Duration,
    endpoint: String,
    domain: BenchmarkDomain,
}

impl ManagedAnvil {
    async fn spawn(log_prefix: &str) -> BenchResult<Self> {
        let state_dir = PathBuf::from(DEFAULT_ANVIL_STATE_DIR);
        let state_path = dir_path_join(state_dir.as_path(), "state.json");
        let deployment_path = dir_path_join(state_dir.as_path(), "deployments/31337/InputBox.json");
        let application_factory_path = dir_path_join(
            state_dir.as_path(),
            "deployments/31337/ApplicationFactory.json",
        );

        ensure_exists(
            state_path.as_path(),
            format!("missing {}; run `just setup` first", state_path.display()),
        )?;
        ensure_exists(
            deployment_path.as_path(),
            format!(
                "missing {}; run `just setup` first",
                deployment_path.display()
            ),
        )?;
        ensure_exists(
            application_factory_path.as_path(),
            format!(
                "missing {}; run `just setup` first",
                application_factory_path.display()
            ),
        )?;

        let input_box_address = read_input_box_address(deployment_path.as_path())?;
        let application_factory_address =
            read_deployment_address(application_factory_path.as_path(), "ApplicationFactory")?;
        let (endpoint, http_addr) = build_local_endpoint()?;
        let log_path = default_anvil_log_path(log_prefix);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stdout_log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(log_path.as_path())?;
        let stderr_log = stdout_log.try_clone()?;

        let mut child = Command::new("anvil")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(http_addr.rsplit(':').next().expect("port"))
            .arg("--load-state")
            .arg(path_as_str(state_path.as_path())?)
            .stdout(Stdio::from(stdout_log))
            .stderr(Stdio::from(stderr_log))
            .spawn()
            .map_err(|err| io_other(format!("failed to spawn anvil: {err}")))?;

        wait_for_rpc_readiness(endpoint.as_str(), &mut child, DEFAULT_ANVIL_START_TIMEOUT).await?;
        let domain = deploy_benchmark_application(
            endpoint.as_str(),
            application_factory_address,
            input_box_address,
        )
        .await?;

        Ok(Self {
            child,
            shutdown_timeout: DEFAULT_ANVIL_SHUTDOWN_TIMEOUT,
            endpoint,
            domain,
        })
    }

    fn domain(&self) -> BenchmarkDomain {
        self.domain
    }

    async fn shutdown(mut self) -> BenchResult<()> {
        send_graceful_terminate(&mut self.child).await;
        match tokio::time::timeout(self.shutdown_timeout, self.child.wait()).await {
            Ok(wait_result) => {
                let _ = wait_result?;
                Ok(())
            }
            Err(_) => {
                self.child.start_kill()?;
                let _ = self.child.wait().await;
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryReport {
    pub method: String,
    pub sample_interval_ms: u64,
    pub rss_start_mb: Option<f64>,
    pub rss_peak_mb: Option<f64>,
    pub rss_end_mb: Option<f64>,
    pub rss_growth_mb: Option<f64>,
    pub rss_growth_per_1k_accepted_tx_mb: Option<f64>,
}

pub struct MemorySampler {
    stop_tx: Option<oneshot::Sender<u64>>,
    join: JoinHandle<MemoryReport>,
}

impl MemorySampler {
    pub fn start(pid: u32, sample_interval: Duration) -> Self {
        let (stop_tx, stop_rx) = oneshot::channel::<u64>();
        let join =
            tokio::spawn(
                async move { sample_memory_until_stop(pid, sample_interval, stop_rx).await },
            );
        Self {
            stop_tx: Some(stop_tx),
            join,
        }
    }

    pub async fn stop(mut self, accepted_count: u64) -> BenchResult<MemoryReport> {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(accepted_count);
        }
        let report = self
            .join
            .await
            .map_err(|err| io_other(format!("memory sampler task join failed: {err}")))?;
        Ok(report)
    }
}

async fn sample_memory_until_stop(
    pid: u32,
    sample_interval: Duration,
    mut stop_rx: oneshot::Receiver<u64>,
) -> MemoryReport {
    let mut start_mb = sample_rss_mb(pid);
    let mut end_mb = start_mb;
    let mut peak_mb = start_mb;
    let mut accepted_count = 0_u64;

    loop {
        tokio::select! {
            maybe_accepted = &mut stop_rx => {
                if let Ok(value) = maybe_accepted {
                    accepted_count = value;
                }
                end_mb = sample_rss_mb(pid).or(end_mb);
                if let Some(end) = end_mb {
                    peak_mb = Some(peak_mb.unwrap_or(end).max(end));
                }
                break;
            }
            _ = tokio::time::sleep(sample_interval) => {
                if let Some(current) = sample_rss_mb(pid) {
                    if start_mb.is_none() {
                        start_mb = Some(current);
                    }
                    end_mb = Some(current);
                    peak_mb = Some(peak_mb.unwrap_or(current).max(current));
                }
            }
        }
    }

    let rss_growth_mb = match (start_mb, end_mb) {
        (Some(start), Some(end)) => Some(end - start),
        _ => None,
    };
    let rss_growth_per_1k_accepted_tx_mb = match (rss_growth_mb, accepted_count) {
        (Some(growth), value) if value > 0 => Some(growth / (value as f64 / 1000.0)),
        _ => None,
    };

    MemoryReport {
        method: "ps-rss-sampling".to_string(),
        sample_interval_ms: u64::try_from(sample_interval.as_millis()).unwrap_or(u64::MAX),
        rss_start_mb: start_mb,
        rss_peak_mb: peak_mb,
        rss_end_mb: end_mb,
        rss_growth_mb,
        rss_growth_per_1k_accepted_tx_mb,
    }
}

fn sample_rss_mb(pid: u32) -> Option<f64> {
    let output = std::process::Command::new("ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let rss_kib = text.trim().parse::<f64>().ok()?;
    Some(rss_kib / 1024.0)
}

pub fn default_sequencer_log_path(prefix: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    PathBuf::from(format!("{DEFAULT_SEQUENCER_LOGS_DIR}/{prefix}-{ts}.log"))
}

fn default_anvil_log_path(prefix: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    PathBuf::from(format!(
        "{DEFAULT_SEQUENCER_LOGS_DIR}/{prefix}-anvil-{ts}.log"
    ))
}

async fn wait_for_readiness(
    endpoint: &str,
    child: &mut Child,
    timeout: Duration,
) -> BenchResult<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io_other(format!(
                "sequencer exited before readiness: status={status}"
            ))
            .into());
        }
        if http_endpoint_is_ready(endpoint).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io_other(format!(
                "timed out waiting for sequencer readiness at {endpoint}"
            ))
            .into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_rpc_readiness(
    endpoint: &str,
    child: &mut Child,
    timeout: Duration,
) -> BenchResult<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io_other(format!("anvil exited before readiness: status={status}")).into());
        }
        if rpc_endpoint_is_ready(endpoint).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io_other(format!(
                "timed out waiting for anvil readiness at {endpoint}"
            ))
            .into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn http_endpoint_is_ready(endpoint: &str) -> bool {
    let Some(host_port) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let mut stream =
        match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(host_port)).await
        {
            Ok(Ok(value)) => value,
            _ => return false,
        };

    let request = format!("GET /tx HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0_u8; 64];
    match tokio::time::timeout(Duration::from_millis(300), stream.read(&mut head)).await {
        Ok(Ok(read)) if read > 0 => std::str::from_utf8(&head[..read])
            .ok()
            .map(|text| text.starts_with("HTTP/1.1") || text.starts_with("HTTP/1.0"))
            .unwrap_or(false),
        _ => false,
    }
}

async fn rpc_endpoint_is_ready(endpoint: &str) -> bool {
    let Some(host_port) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let mut stream =
        match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(host_port)).await
        {
            Ok(Ok(value)) => value,
            _ => return false,
        };

    let body = r#"{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}"#;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0_u8; 128];
    match tokio::time::timeout(Duration::from_millis(300), stream.read(&mut head)).await {
        Ok(Ok(read)) if read > 0 => std::str::from_utf8(&head[..read])
            .ok()
            .map(|text| text.contains("200 OK"))
            .unwrap_or(false),
        _ => false,
    }
}

async fn send_graceful_terminate(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };

    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }

    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
}

fn build_local_endpoint() -> BenchResult<(String, String)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let http_addr = format!("127.0.0.1:{}", addr.port());
    let endpoint = format!("http://{http_addr}");
    Ok((endpoint, http_addr))
}

fn dir_path_join(base: &Path, file: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    path.push(file);
    path
}

fn ensure_exists(path: &Path, missing_message: String) -> BenchResult<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(io_other(missing_message).into())
    }
}

fn path_as_str(path: &Path) -> BenchResult<&str> {
    path.to_str()
        .ok_or_else(|| io_other(format!("path is not valid UTF-8: {}", path.display())).into())
}

fn read_input_box_address(path: &Path) -> BenchResult<Address> {
    read_deployment_address(path, "InputBox")
}

fn read_deployment_address(path: &Path, contract_name: &str) -> BenchResult<Address> {
    #[derive(Deserialize)]
    struct DeploymentInfo {
        address: String,
        #[serde(rename = "contractName")]
        contract_name: String,
    }

    let deployment: DeploymentInfo = serde_json::from_str(&fs::read_to_string(path)?)
        .map_err(|err| io_other(format!("failed to parse {}: {err}", path.display())))?;
    if deployment.contract_name != contract_name {
        return Err(io_other(format!(
            "expected {} deployment in {}, found {}",
            contract_name,
            path.display(),
            deployment.contract_name
        ))
        .into());
    }
    deployment.address.parse().map_err(|err| {
        io_other(format!(
            "invalid {} address in {}: {err}",
            contract_name,
            path.display()
        ))
        .into()
    })
}

async fn deploy_benchmark_application(
    endpoint: &str,
    application_factory_address: Address,
    input_box_address: Address,
) -> BenchResult<BenchmarkDomain> {
    let private_key = default_private_keys()
        .first()
        .ok_or_else(|| io_other("missing default Anvil private key"))?;
    let signer: PrivateKeySigner = private_key
        .parse()
        .map_err(|err| io_other(format!("invalid default Anvil private key: {err}")))?;
    let app_owner = signer.address();
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect(endpoint)
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to connect wallet provider to {endpoint}: {err}"
            ))
        })?;

    let factory = ApplicationFactory::new(application_factory_address, &provider);
    let template_hash = load_template_hash(Path::new(DEFAULT_TEMPLATE_MACHINE_IMAGE_PATH))?;
    let data_availability: Bytes = input_box_address.as_slice().to_vec().into();
    let create_application =
        factory.newApplication_1(Address::ZERO, app_owner, template_hash, data_availability);
    let application_address = create_application
        .clone()
        .call()
        .await
        .map_err(|err| io_other(format!("failed to simulate application deployment: {err}")))?;
    let receipt = create_application
        .send()
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to send application deployment transaction: {err}"
            ))
        })?
        .get_receipt()
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to confirm application deployment transaction: {err}"
            ))
        })?;
    if !receipt.status() {
        return Err(io_other("application deployment transaction reverted").into());
    }

    Ok(BenchmarkDomain {
        chain_id: SELF_CONTAINED_DOMAIN_CHAIN_ID,
        verifying_contract: application_address,
    })
}

fn load_template_hash(machine_image_path: &Path) -> BenchResult<B256> {
    ensure_exists(
        machine_image_path,
        format!(
            "missing {}; run `just canonical-build-machine-image` first",
            machine_image_path.display()
        ),
    )?;

    let output = std::process::Command::new("cartesi-machine")
        .arg(format!("--load={}", path_as_str(machine_image_path)?))
        .arg("--initial-hash")
        .output()
        .map_err(|err| {
            io_other(format!(
                "failed to run cartesi-machine for {}: {err}",
                machine_image_path.display()
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io_other(format!(
            "cartesi-machine --initial-hash failed for {}: {}",
            machine_image_path.display(),
            stderr.trim()
        ))
        .into());
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|err| io_other(format!("cartesi-machine stdout was not valid UTF-8: {err}")))?;
    let stderr = String::from_utf8(output.stderr)
        .map_err(|err| io_other(format!("cartesi-machine stderr was not valid UTF-8: {err}")))?;
    let combined_output = format!("{stdout}\n{stderr}");
    let hash = extract_template_hash(&combined_output).ok_or_else(|| {
        io_other(format!(
            "could not find template hash in cartesi-machine output for {}",
            machine_image_path.display()
        ))
    })?;

    format!("0x{hash}").parse().map_err(|err| {
        io_other(format!("invalid template hash from cartesi-machine: {err}")).into()
    })
}

fn extract_template_hash(output: &str) -> Option<&str> {
    const HASH_LEN: usize = 64;

    output.split_whitespace().find_map(|token| {
        let trimmed = token.trim_end_matches(':');
        if trimmed.len() >= HASH_LEN {
            trimmed.as_bytes().windows(HASH_LEN).find_map(|window| {
                std::str::from_utf8(window)
                    .ok()
                    .filter(|candidate| candidate.chars().all(|ch| ch.is_ascii_hexdigit()))
            })
        } else {
            None
        }
    })
}

fn io_other(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

#[cfg(test)]
mod tests {
    use super::{default_anvil_log_path, default_sequencer_log_path, extract_template_hash};

    #[test]
    fn default_log_path_uses_results_dir() {
        let value = default_sequencer_log_path("ack-latency");
        assert!(value.to_string_lossy().contains("benchmarks/results/"));
        assert!(value.to_string_lossy().contains("ack-latency"));
    }

    #[test]
    fn default_anvil_log_path_uses_results_dir() {
        let value = default_anvil_log_path("ack-latency");
        assert!(value.to_string_lossy().contains("benchmarks/results/"));
        assert!(value.to_string_lossy().contains("ack-latency-anvil"));
    }

    #[test]
    fn extract_template_hash_parses_cartesi_machine_output() {
        let output = "Loading machine: please wait\n53799514: 6b7e5d3e45079545ffa46edee3a7a8e68dbff4ccadf0520b772a357807d42de8\n";
        assert_eq!(
            extract_template_hash(output),
            Some("6b7e5d3e45079545ffa46edee3a7a8e68dbff4ccadf0520b772a357807d42de8")
        );
    }
}
