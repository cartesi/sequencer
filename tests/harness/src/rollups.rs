// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::providers::ext::AnvilApi;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{BlockNumberOrTag, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use alloy_primitives::{Address, B256, Bytes, U256};
use app_core::application::{WalletConfig, default_private_keys};
use cartesi_rollups_contracts::application_factory::ApplicationFactory::{self, WithdrawalConfig};
use cartesi_rollups_contracts::data_availability::DataAvailability::InputBoxCall;
use serde::Deserialize;
use tokio::process::{Child, Command};

use crate::HarnessResult;
use crate::paths;
use crate::util::{
    build_local_endpoint, ensure_exists, io_other, path_as_str, send_graceful_terminate,
    timestamped_log_path, wait_for_rpc_readiness,
};

pub const DEVNET_CHAIN_ID: u64 = 31_337;

const DEFAULT_ANVIL_START_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_ANVIL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_ANVIL_SLOTS_IN_EPOCH: u64 = 1;
const LIVE_L1_BLOCK_INTERVAL_SECONDS: u64 = 1;
const DEVNET_MOCK_ERC20_DEPLOYER_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044976f1d86dbce6c5bb4f80a8b5148f7f4f6d0d0c0abc";
const DEVNET_MOCK_ERC20_DEPLOYER_FUNDING_WEI: u64 = 1_000_000_000_000_000;

pub struct DevnetRollupsStack {
    anvil: ManagedAnvil,
    app_address: Address,
    mock_erc20_address: Address,
}

impl DevnetRollupsStack {
    pub async fn spawn(log_prefix: &str, logs_dir: &Path) -> HarnessResult<Self> {
        let anvil = ManagedAnvil::spawn(log_prefix, logs_dir).await?;
        let mock_erc20_address = deploy_devnet_mock_erc20(anvil.endpoint.as_str()).await?;
        let app_address = deploy_devnet_application(
            anvil.endpoint.as_str(),
            anvil.application_factory_address,
            anvil.input_box_address,
        )
        .await?;

        Ok(Self {
            anvil,
            app_address,
            mock_erc20_address,
        })
    }

    pub fn l1_endpoint(&self) -> &str {
        self.anvil.endpoint.as_str()
    }

    pub fn app_address(&self) -> Address {
        self.app_address
    }

    pub fn input_box_address(&self) -> Address {
        self.anvil.input_box_address
    }

    pub fn application_factory_address(&self) -> Address {
        self.anvil.application_factory_address
    }

    pub fn erc20_portal_address(&self) -> Address {
        self.anvil.erc20_portal_address
    }

    pub fn supported_erc20_token(&self) -> Address {
        self.mock_erc20_address
    }

    pub async fn deploy_extra_mock_erc20(&self) -> HarnessResult<Address> {
        deploy_mock_erc20_from_default_funder(self.anvil.endpoint.as_str()).await
    }

    /// Mine outage-style L1 progress at the configured 12-second block time.
    /// Tests that model ordinary live progress should use
    /// [`Self::mine_live_l1_blocks`] instead.
    pub async fn mine_l1_blocks(&self, block_count: u64) -> HarnessResult<()> {
        self.anvil.mine_blocks(block_count).await
    }

    /// Mine ordinary live-chain progress without simulating a 12-second outage
    /// per block. The one-second interval keeps Anvil timestamps monotone while
    /// wall-clock-paced polling avoids tripping the clock-usability guard.
    pub async fn mine_live_l1_blocks(&self, block_count: u64) -> HarnessResult<()> {
        self.anvil
            .mine_blocks_with_interval(block_count, LIVE_L1_BLOCK_INTERVAL_SECONDS)
            .await
    }

    pub async fn l1_safe_block_number(&self) -> HarnessResult<u64> {
        let provider = ProviderBuilder::new()
            .connect(self.anvil.endpoint.as_str())
            .await
            .map_err(|err| io_other(format!("failed to connect anvil provider: {err}")))?;
        let block = provider
            .get_block_by_number(BlockNumberOrTag::Safe)
            .await
            .map_err(|err| io_other(format!("failed to read Anvil safe head: {err}")))?
            .ok_or_else(|| io_other("Anvil returned no safe block"))?;
        Ok(block.header.number)
    }

    /// Toggle Anvil's auto-mining mode. When disabled, txs accumulate in
    /// the mempool until an explicit `anvil_mine` call (or re-enable).
    pub async fn set_automine(&self, enabled: bool) -> HarnessResult<()> {
        self.anvil.set_automine(enabled).await
    }

    /// Drop every pending tx from Anvil's mempool. Useful for simulating
    /// mempool eviction or gateway packet loss.
    pub async fn drop_all_pending_txs(&self) -> HarnessResult<()> {
        self.anvil.drop_all_pending_txs().await
    }

    pub async fn shutdown(self) -> HarnessResult<()> {
        self.anvil.shutdown().await
    }

    pub fn try_wait_anvil(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.anvil.try_wait()
    }

    pub fn anvil_log_path(&self) -> &Path {
        self.anvil.log_path()
    }
}
struct ManagedAnvil {
    child: Child,
    shutdown_timeout: Duration,
    endpoint: String,
    log_path: PathBuf,
    input_box_address: Address,
    application_factory_address: Address,
    erc20_portal_address: Address,
}

impl ManagedAnvil {
    async fn spawn(log_prefix: &str, logs_dir: &Path) -> HarnessResult<Self> {
        let state_dir = paths::resolved_anvil_state_dir();
        let state_path = state_dir.join("state.json");
        let input_box_path = state_dir.join("deployments/31337/InputBox.json");
        let application_factory_path = state_dir.join("deployments/31337/ApplicationFactory.json");
        let erc20_portal_path = state_dir.join("deployments/31337/ERC20Portal.json");

        ensure_exists(
            state_path.as_path(),
            format!("missing {}; run `just setup` first", state_path.display()),
        )?;
        ensure_exists(
            input_box_path.as_path(),
            format!(
                "missing {}; run `just setup` first",
                input_box_path.display()
            ),
        )?;
        ensure_exists(
            application_factory_path.as_path(),
            format!(
                "missing {}; run `just setup` first",
                application_factory_path.display()
            ),
        )?;
        ensure_exists(
            erc20_portal_path.as_path(),
            format!(
                "missing {}; run `just setup` first",
                erc20_portal_path.display()
            ),
        )?;

        let input_box_address = read_deployment_address(input_box_path.as_path(), "InputBox")?;
        let application_factory_address =
            read_deployment_address(application_factory_path.as_path(), "ApplicationFactory")?;
        let erc20_portal_address =
            read_deployment_address(erc20_portal_path.as_path(), "ERC20Portal")?;

        fs::create_dir_all(logs_dir)?;
        let log_path = timestamped_log_path(logs_dir, &format!("{log_prefix}-anvil"));
        let stdout_log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(log_path.as_path())?;
        let stderr_log = stdout_log.try_clone()?;

        let (endpoint, http_addr) = build_local_endpoint()?;
        let mut child = Command::new("anvil")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(http_addr.rsplit(':').next().expect("port"))
            .arg("--slots-in-an-epoch")
            .arg(DEFAULT_ANVIL_SLOTS_IN_EPOCH.to_string())
            .arg("--load-state")
            .arg(path_as_str(state_path.as_path())?)
            .stdout(Stdio::from(stdout_log))
            .stderr(Stdio::from(stderr_log))
            .spawn()
            .map_err(|err| io_other(format!("failed to spawn anvil: {err}")))?;

        wait_for_rpc_readiness(endpoint.as_str(), &mut child, DEFAULT_ANVIL_START_TIMEOUT).await?;

        Ok(Self {
            child,
            shutdown_timeout: DEFAULT_ANVIL_SHUTDOWN_TIMEOUT,
            endpoint,
            log_path,
            input_box_address,
            application_factory_address,
            erc20_portal_address,
        })
    }

    fn log_path(&self) -> &Path {
        self.log_path.as_path()
    }

    async fn shutdown(mut self) -> HarnessResult<()> {
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

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    async fn mine_blocks(&self, block_count: u64) -> HarnessResult<()> {
        // L1 block-time coupling: each mined block advances the L1
        // timestamp by `SECONDS_PER_BLOCK` (Ethereum mainnet parity, also
        // what `ManagedSequencer::advance_wall_and_mine` assumes when
        // pairing faketime advances with block counts).
        //
        // Without the explicit `interval`, anvil defaults to 1s/block —
        // which then desyncs from faketime, making large advances trip
        // spurious `L1ViewStale` even when wall clock and L1 should move
        // together. See `ManagedSequencer::advance_wall_and_mine`.
        const SECONDS_PER_BLOCK: u64 = 12;
        self.mine_blocks_with_interval(block_count, SECONDS_PER_BLOCK)
            .await
    }

    async fn mine_blocks_with_interval(
        &self,
        block_count: u64,
        interval_seconds: u64,
    ) -> HarnessResult<()> {
        if block_count == 0 {
            return Ok(());
        }

        let provider = ProviderBuilder::new()
            .connect(self.endpoint.as_str())
            .await
            .map_err(|err| io_other(format!("failed to connect anvil provider: {err}")))?;
        provider
            .anvil_mine(Some(block_count), Some(interval_seconds))
            .await
            .map_err(|err| {
                io_other(format!(
                    "failed to mine {block_count} Anvil block(s): {err}"
                ))
            })?;
        Ok(())
    }

    async fn set_automine(&self, enabled: bool) -> HarnessResult<()> {
        let provider = ProviderBuilder::new()
            .connect(self.endpoint.as_str())
            .await
            .map_err(|err| io_other(format!("failed to connect anvil provider: {err}")))?;
        provider
            .anvil_set_auto_mine(enabled)
            .await
            .map_err(|err| io_other(format!("failed to set auto_mine={enabled}: {err}")))?;
        Ok(())
    }

    async fn drop_all_pending_txs(&self) -> HarnessResult<()> {
        let provider = ProviderBuilder::new()
            .connect(self.endpoint.as_str())
            .await
            .map_err(|err| io_other(format!("failed to connect anvil provider: {err}")))?;
        provider
            .anvil_drop_all_transactions()
            .await
            .map_err(|err| io_other(format!("failed to drop all pending txs: {err}")))?;
        Ok(())
    }
}

fn read_deployment_address(path: &Path, contract_name: &str) -> HarnessResult<Address> {
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

fn load_mock_erc20_init_code() -> HarnessResult<Bytes> {
    let artifact_path = paths::mock_erc20_artifact_path();
    ensure_exists(
        artifact_path.as_path(),
        format!(
            "missing {}; run `just setup` first",
            artifact_path.display()
        ),
    )?;

    let artifact: serde_json::Value = serde_json::from_str(&fs::read_to_string(&artifact_path)?)
        .map_err(|err| {
            io_other(format!(
                "failed to parse {}: {err}",
                artifact_path.display()
            ))
        })?;
    let bytecode = artifact
        .get("bytecode")
        .and_then(|value| value.get("object"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            io_other(format!(
                "missing bytecode.object in {}",
                artifact_path.display()
            ))
        })?;

    let bytecode_hex = bytecode.strip_prefix("0x").unwrap_or(bytecode);
    let bytecode = alloy_primitives::hex::decode(bytecode_hex).map_err(|err| {
        io_other(format!(
            "invalid mock ERC20 bytecode hex in {}: {err}",
            artifact_path.display()
        ))
    })?;
    Ok(bytecode.into())
}

async fn deploy_devnet_mock_erc20(endpoint: &str) -> HarnessResult<Address> {
    let expected_token_address = WalletConfig::devnet().supported_erc20_token;
    let deployer_signer: PrivateKeySigner = DEVNET_MOCK_ERC20_DEPLOYER_PRIVATE_KEY
        .parse()
        .map_err(|err| io_other(format!("invalid devnet mock ERC20 deployer key: {err}")))?;
    let deployer_address = deployer_signer.address();
    let derived_expected_address = deployer_address.create(0);
    if derived_expected_address != expected_token_address {
        return Err(io_other(format!(
            "devnet mock ERC20 address drifted: WalletConfig::devnet() expects {expected_token_address}, but deployer {deployer_address} at nonce 0 yields {derived_expected_address}; update the canonical app devnet address or the deployer invariant"
        ))
        .into());
    }

    let funding_signer: PrivateKeySigner = default_private_keys()
        .first()
        .ok_or_else(|| io_other("missing default Anvil private key"))?
        .parse()
        .map_err(|err| io_other(format!("invalid default Anvil private key: {err}")))?;
    let funding_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(funding_signer))
        .connect(endpoint)
        .await
        .map_err(|err| io_other(format!("failed to connect funding wallet provider: {err}")))?;
    let funding_receipt = funding_provider
        .send_transaction(
            TransactionRequest::default()
                .to(deployer_address)
                .value(U256::from(DEVNET_MOCK_ERC20_DEPLOYER_FUNDING_WEI)),
        )
        .await
        .map_err(|err| io_other(format!("failed to fund devnet mock ERC20 deployer: {err}")))?
        .get_receipt()
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to confirm deployer funding transaction: {err}"
            ))
        })?;
    if !funding_receipt.status() {
        return Err(io_other("devnet mock ERC20 deployer funding transaction reverted").into());
    }

    let deployer_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(deployer_signer))
        .connect(endpoint)
        .await
        .map_err(|err| io_other(format!("failed to connect deployer wallet provider: {err}")))?;
    let deploy_receipt = deployer_provider
        .send_transaction(
            TransactionRequest::default().with_deploy_code(load_mock_erc20_init_code()?),
        )
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to send mock ERC20 deployment transaction: {err}"
            ))
        })?
        .get_receipt()
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to confirm mock ERC20 deployment transaction: {err}"
            ))
        })?;
    if !deploy_receipt.status() {
        return Err(io_other("mock ERC20 deployment transaction reverted").into());
    }

    let deployed_address = deploy_receipt
        .contract_address
        .ok_or_else(|| io_other("mock ERC20 deployment receipt missing contract address"))?;
    if deployed_address != expected_token_address {
        return Err(io_other(format!(
            "mock ERC20 deployed at {deployed_address}, expected {expected_token_address}; update the devnet canonical app token address, then update this assert"
        ))
        .into());
    }

    Ok(deployed_address)
}

async fn deploy_mock_erc20_from_default_funder(endpoint: &str) -> HarnessResult<Address> {
    let private_key = default_private_keys()
        .first()
        .ok_or_else(|| io_other("missing default Anvil private key"))?;
    let signer: PrivateKeySigner = private_key
        .parse()
        .map_err(|err| io_other(format!("invalid default Anvil private key: {err}")))?;
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(endpoint)
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to connect mock ERC20 deployer wallet provider: {err}"
            ))
        })?;
    let deploy_receipt = provider
        .send_transaction(
            TransactionRequest::default().with_deploy_code(load_mock_erc20_init_code()?),
        )
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to send extra mock ERC20 deployment transaction: {err}"
            ))
        })?
        .get_receipt()
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to confirm extra mock ERC20 deployment transaction: {err}"
            ))
        })?;
    if !deploy_receipt.status() {
        return Err(io_other("extra mock ERC20 deployment transaction reverted").into());
    }

    deploy_receipt.contract_address.ok_or_else(|| {
        io_other("extra mock ERC20 deployment receipt missing contract address").into()
    })
}

async fn deploy_devnet_application(
    endpoint: &str,
    application_factory_address: Address,
    input_box_address: Address,
) -> HarnessResult<Address> {
    let private_key = default_private_keys()
        .first()
        .ok_or_else(|| io_other("missing default Anvil private key"))?;
    let signer: PrivateKeySigner = private_key
        .parse()
        .map_err(|err| io_other(format!("invalid default Anvil private key: {err}")))?;
    let app_owner = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(endpoint)
        .await
        .map_err(|err| {
            io_other(format!(
                "failed to connect wallet provider to {endpoint}: {err}"
            ))
        })?;

    let factory = ApplicationFactory::new(application_factory_address, &provider);
    let template_hash = load_template_hash(paths::devnet_machine_image_path().as_path())?;
    let data_availability: Bytes = InputBoxCall {
        inputBox: input_box_address,
    }
    .abi_encode()
    .into();
    // Withdrawals disabled: a zero guardian can never `foreclose()`, so the
    // InputBox's v3 `notForeclosed` gate on `addInput` stays permanently open,
    // and no withdrawal output builder is needed for the placeholder wallet.
    let withdrawal_config = WithdrawalConfig {
        guardian: Address::ZERO,
        log2LeavesPerAccount: 0,
        log2MaxNumOfAccounts: 0,
        accountsDriveStartIndex: 0,
        withdrawalOutputBuilder: Address::ZERO,
    };
    let create_application = factory.newApplication_0(
        Address::ZERO,
        app_owner,
        template_hash,
        data_availability,
        withdrawal_config,
    );
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

    Ok(application_address)
}

fn load_template_hash(machine_image_path: &Path) -> HarnessResult<B256> {
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

#[cfg(test)]
mod tests {
    use super::extract_template_hash;

    #[test]
    fn extract_template_hash_parses_cartesi_machine_output() {
        let output = "Loading machine: please wait\n53799514: 6b7e5d3e45079545ffa46edee3a7a8e68dbff4ccadf0520b772a357807d42de8\n";
        assert_eq!(
            extract_template_hash(output),
            Some("6b7e5d3e45079545ffa46edee3a7a8e68dbff4ccadf0520b772a357807d42de8")
        );
    }
}
