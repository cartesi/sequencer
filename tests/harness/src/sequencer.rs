// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use app_core::application::default_private_keys;
use sequencer_rust_client::SequencerClient;
use tempfile::TempDir;
use tokio::process::{Child, Command};

use crate::HarnessResult;
use crate::paths;
use crate::rollups::{DEVNET_CHAIN_ID, DevnetRollupsStack};
use crate::util::{
    build_local_endpoint, io_other, path_as_str, send_graceful_terminate, timestamped_log_path,
    wait_for_http_readiness,
};
use crate::wallet::{TestSigner, WalletL1Client, WalletL2Client};
use crate::ws::WsClient;

const DEFAULT_SEQUENCER_START_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_SEQUENCER_RUST_LOG: &str = "info";
pub const DEFAULT_DEVNET_SEQUENCER_BIN: &str = "target/debug/sequencer-devnet";
pub const DEFAULT_TEST_LOGS_DIR: &str = "tests/e2e/results";

#[derive(Debug, Clone)]
pub struct ManagedSequencerConfig {
    pub sequencer_bin: PathBuf,
    pub log_prefix: String,
    pub logs_dir: PathBuf,
}

pub struct ManagedSequencer {
    rollups: DevnetRollupsStack,
    child: Child,
    shutdown_timeout: Duration,
    sequencer_bin: PathBuf,
    log_prefix: String,
    logs_dir: PathBuf,
    _data_dir: TempDir,
    data_dir_path: PathBuf,
    endpoint: String,
    log_path: PathBuf,
}

pub fn default_devnet_sequencer_config(log_prefix: impl Into<String>) -> ManagedSequencerConfig {
    ManagedSequencerConfig {
        sequencer_bin: PathBuf::from(DEFAULT_DEVNET_SEQUENCER_BIN),
        log_prefix: log_prefix.into(),
        logs_dir: PathBuf::from(DEFAULT_TEST_LOGS_DIR),
    }
}

impl ManagedSequencer {
    pub async fn spawn(config: ManagedSequencerConfig) -> HarnessResult<Self> {
        let logs_dir = paths::resolve_from_workspace(&config.logs_dir);
        let sequencer_bin = paths::resolve_from_workspace(&config.sequencer_bin);
        let log_prefix = config.log_prefix;
        let rollups = DevnetRollupsStack::spawn(log_prefix.as_str(), logs_dir.as_path()).await?;

        fs::create_dir_all(logs_dir.as_path())?;
        let data_dir = TempDir::new()
            .map_err(|err| io_other(format!("failed to create temp data dir: {err}")))?;
        let data_dir_path = data_dir.path().to_path_buf();
        let SpawnedSequencerProcess {
            child,
            endpoint,
            log_path,
        } = spawn_sequencer_process(
            sequencer_bin.as_path(),
            log_prefix.as_str(),
            logs_dir.as_path(),
            data_dir_path.as_path(),
            &rollups,
        )
        .await?;

        Ok(Self {
            rollups,
            child,
            shutdown_timeout: DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT,
            sequencer_bin,
            log_prefix,
            logs_dir,
            _data_dir: data_dir,
            data_dir_path,
            endpoint,
            log_path,
        })
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn log_path(&self) -> &Path {
        self.log_path.as_path()
    }

    pub fn data_dir(&self) -> &Path {
        self.data_dir_path.as_path()
    }

    pub fn domain_chain_id(&self) -> u64 {
        DEVNET_CHAIN_ID
    }

    pub fn verifying_contract(&self) -> Address {
        self.rollups.app_address()
    }

    pub fn l1_endpoint(&self) -> &str {
        self.rollups.l1_endpoint()
    }

    pub fn app_address(&self) -> Address {
        self.rollups.app_address()
    }

    pub fn erc20_portal_address(&self) -> Address {
        self.rollups.erc20_portal_address()
    }

    pub fn supported_erc20_token(&self) -> Address {
        self.rollups.supported_erc20_token()
    }

    pub async fn deploy_extra_mock_erc20(&self) -> HarnessResult<Address> {
        self.rollups.deploy_extra_mock_erc20().await
    }

    pub async fn mine_l1_blocks(&self, block_count: u64) -> HarnessResult<()> {
        self.rollups.mine_l1_blocks(block_count).await
    }

    pub async fn restart(&mut self) -> HarnessResult<()> {
        self.shutdown_child().await?;
        let SpawnedSequencerProcess {
            child,
            endpoint,
            log_path,
        } = spawn_sequencer_process(
            self.sequencer_bin.as_path(),
            self.log_prefix.as_str(),
            self.logs_dir.as_path(),
            self.data_dir_path.as_path(),
            &self.rollups,
        )
        .await?;
        self.child = child;
        self.endpoint = endpoint;
        self.log_path = log_path;
        Ok(())
    }

    pub async fn ws(&self, from_offset: u64) -> HarnessResult<WsClient> {
        let client = self.sequencer_client()?;
        WsClient::connect(&client, from_offset).await
    }

    pub async fn wallet_l1(&self, signer: TestSigner) -> HarnessResult<WalletL1Client> {
        WalletL1Client::connect(
            self.l1_endpoint(),
            self.app_address(),
            self.erc20_portal_address(),
            self.supported_erc20_token(),
            signer,
        )
        .await
    }

    pub fn wallet_l2(&self, signer: TestSigner) -> HarnessResult<WalletL2Client> {
        WalletL2Client::new(
            self.endpoint(),
            self.domain_chain_id(),
            self.verifying_contract(),
            signer,
        )
    }

    pub async fn shutdown(mut self) -> HarnessResult<()> {
        let sequencer_result = self.shutdown_child().await;
        let rollups_result = self.rollups.shutdown().await;
        sequencer_result?;
        rollups_result
    }

    fn sequencer_client(&self) -> HarnessResult<SequencerClient> {
        SequencerClient::new_with_timeout(self.endpoint.clone(), Duration::from_secs(5))
            .map_err(|err| io_other(format!("failed to create sequencer client: {err}")).into())
    }

    async fn shutdown_child(&mut self) -> HarnessResult<()> {
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

use alloy_primitives::Address;

struct SpawnedSequencerProcess {
    child: Child,
    endpoint: String,
    log_path: PathBuf,
}

async fn spawn_sequencer_process(
    sequencer_bin: &Path,
    log_prefix: &str,
    logs_dir: &Path,
    data_dir: &Path,
    rollups: &DevnetRollupsStack,
) -> HarnessResult<SpawnedSequencerProcess> {
    let (endpoint, http_addr) = build_local_endpoint()?;
    let log_path = timestamped_log_path(logs_dir, log_prefix);
    let stdout_log = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(log_path.as_path())?;
    let stderr_log = stdout_log.try_clone()?;

    let batch_submitter_key = default_private_keys().first().cloned().unwrap_or_else(|| {
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
    });
    let mut child = Command::new(path_as_str(sequencer_bin)?)
        .arg("--http-addr")
        .arg(http_addr)
        .arg("--data-dir")
        .arg(path_as_str(data_dir)?)
        .arg("--eth-rpc-url")
        .arg(rollups.l1_endpoint())
        .arg("--chain-id")
        .arg(DEVNET_CHAIN_ID.to_string())
        .arg("--app-address")
        .arg(rollups.app_address().to_string())
        .arg("--batch-submitter-private-key")
        .arg(&batch_submitter_key)
        .env("RUST_LOG", DEFAULT_SEQUENCER_RUST_LOG)
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .map_err(|err| {
            io_other(format!(
                "failed to spawn sequencer binary '{}': {err}",
                sequencer_bin.display()
            ))
        })?;

    wait_for_http_readiness(
        endpoint.as_str(),
        &mut child,
        DEFAULT_SEQUENCER_START_TIMEOUT,
    )
    .await?;

    Ok(SpawnedSequencerProcess {
        child,
        endpoint,
        log_path,
    })
}
