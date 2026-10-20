// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Real canonical execution and Application proofs; consensus finalization is a fixture.

use alloy::network::EthereumWallet;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy_primitives::{B256, Bytes, U256};
use app_core::application::default_private_keys;
use cartesi_rollups_contracts::i_application::IApplication::{self, AccountValidityProof};
use cartesi_rollups_contracts::input_box::InputBox;
use rollups_harness::{ManagedSequencer, ReplayWalletApp, TestSigner};
use serde_json::Value;
use tokio::process::Command;

use crate::ScenarioResult;

alloy::sol! {
    #[sol(rpc)]
    interface FinalizationFixture { function finalize(bytes32 root, uint256 count) external; }
    #[sol(rpc)]
    interface Token { function balanceOf(address owner) external view returns (uint256); }
}

async fn lua(arguments: &[String]) -> ScenarioResult<Value> {
    let result = Command::new("lua5.4").args(arguments).output().await?;
    if !result.status.success() {
        return Err(format!("Lua failed: {}", String::from_utf8_lossy(&result.stderr)).into());
    }
    Ok(serde_json::from_slice(&result.stdout)?)
}

fn hashes(value: &Value) -> ScenarioResult<Vec<B256>> {
    value
        .as_array()
        .ok_or("missing proof siblings")?
        .iter()
        .map(|value| Ok(value.as_str().ok_or("invalid proof hash")?.parse()?))
        .collect()
}

pub(crate) async fn run(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    use crate::test_cases::{apply_reconciled_supported_deposit, mine_until_finalized_advances};
    runtime.set_max_batch_open_seconds(Some(1));
    runtime.stop_expecting_clean_exit().await?;
    runtime.respawn().await?;
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let bob_l1 = runtime.wallet_l1(bob.clone()).await?;
    let mut ws = runtime.ws(0).await?;
    let mut replay = ReplayWalletApp::devnet();
    let credited: U256 = (U256::from(1) << 128) + U256::from(10_000_000);
    let pending = U256::from(123_456);
    apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, credited).await?;
    mine_until_finalized_advances(runtime, 0).await?;
    runtime.stop_expecting_clean_exit().await?;
    bob_l1.mint_and_deposit_supported_token(pending).await?;
    runtime.mine_live_l1_blocks(2).await?;
    let block = runtime.l1_safe_block_number().await?;
    let work = tempfile::tempdir()?;
    let snapshot = work.path().join("canonical");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let checkpoint = lua(&[
        "tests/e2e/wallet-exit-replay.lua".into(),
        runtime.l1_endpoint().into(),
        runtime.app_address().to_string(),
        runtime.input_box_address().to_string(),
        root.join("examples/canonical-app/out/canonical-machine-image")
            .display()
            .to_string(),
        block.to_string(),
        snapshot.display().to_string(),
    ])
    .await?;
    let mut proofs = Vec::new();
    for owner in [alice.address(), bob.address()] {
        proofs.push(
            lua(&[
                "scripts/wallet-exit-proof.lua".into(),
                snapshot.display().to_string(),
                owner.to_string(),
            ])
            .await?,
        );
    }
    let signer: PrivateKeySigner = default_private_keys()[0].parse()?;
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(runtime.l1_endpoint().parse()?);
    let app = IApplication::new(runtime.app_address(), &provider);
    let validator = app.getOutputsMerkleRootValidator().call().await?;
    FinalizationFixture::new(validator, &provider)
        .finalize(
            proofs[0]["machine_root"]
                .as_str()
                .ok_or("missing machine root")?
                .parse()?,
            U256::from(
                checkpoint["input_count"]
                    .as_u64()
                    .ok_or("missing input count")?,
            ),
        )
        .send()
        .await?
        .get_receipt()
        .await?;
    runtime.foreclose().await?;
    let actual_index = app.getAccountsDriveStartIndex().call().await?;
    let log2_leaves = app.getLog2LeavesPerAccount().call().await?;
    let log2_accounts = app.getLog2MaxNumOfAccounts().call().await?;
    assert_eq!(actual_index, 0x0090_0000_0000_0000_u64 >> 26);
    assert_eq!((log2_leaves, log2_accounts), (1, 20));
    let expected_root: B256 = proofs[0]["machine_root"].as_str().unwrap().parse()?;
    let mut calculated: B256 = proofs[0]["accounts_drive_root"].as_str().unwrap().parse()?;
    for (height, sibling) in hashes(&proofs[0]["accounts_drive_siblings"])?
        .into_iter()
        .enumerate()
    {
        let mut pair = Vec::with_capacity(64);
        if (actual_index >> height) & 1 == 0 {
            pair.extend(calculated);
            pair.extend(sibling);
        } else {
            pair.extend(sibling);
            pair.extend(calculated);
        }
        calculated = alloy_primitives::keccak256(pair);
    }
    assert_eq!(
        calculated, expected_root,
        "exported drive proof does not reconstruct machine root"
    );
    // The pending deposit is finalized, so its only available path is the drive.
    let events = InputBox::new(runtime.input_box_address(), &provider)
        .InputAdded_filter()
        .topic1(runtime.app_address().into_word())
        .from_block(0)
        .query()
        .await?;
    let (last, _) = events.last().ok_or("missing queued deposit")?;
    let refund = app
        .issueRefund(last.index, last.input.clone())
        .call()
        .await
        .err()
        .ok_or("finalized input refund unexpectedly succeeded")?;
    assert!(
        refund
            .as_decoded_error::<IApplication::CannotRefundFinalizedInput>()
            .is_some()
    );
    assert!(
        app.proveAccountsDriveMerkleRoot(
            B256::ZERO,
            hashes(&proofs[0]["accounts_drive_siblings"])?
        )
        .call()
        .await
        .is_err()
    );
    let receipt = app
        .proveAccountsDriveMerkleRoot(
            proofs[0]["accounts_drive_root"]
                .as_str()
                .ok_or("missing drive root")?
                .parse()?,
            hashes(&proofs[0]["accounts_drive_siblings"])?,
        )
        .send()
        .await?
        .get_receipt()
        .await?;
    assert!(receipt.status());
    let token = Token::new(
        app_core::application::WalletConfig::devnet().supported_erc20_token,
        &provider,
    );
    for ((owner, expected), proof) in [(alice.address(), credited), (bob.address(), pending)]
        .into_iter()
        .zip(proofs)
    {
        let account: Bytes = proof["account"]
            .as_str()
            .ok_or("missing account")?
            .parse()?;
        let validity = AccountValidityProof {
            accountIndex: proof["account_index"]
                .as_u64()
                .ok_or("missing account index")?,
            accountRootSiblings: hashes(&proof["account_root_siblings"])?,
        };
        let mut tampered = account.to_vec();
        tampered[63] ^= 1;
        assert!(
            app.withdraw(tampered.into(), validity.clone())
                .call()
                .await
                .is_err()
        );
        let before = token.balanceOf(owner).call().await?;
        let receipt = app
            .withdraw(account.clone(), validity.clone())
            .send()
            .await?
            .get_receipt()
            .await?;
        assert!(receipt.status());
        assert_eq!(token.balanceOf(owner).call().await? - before, expected);
        assert!(
            app.withdraw(account, validity).call().await.is_err(),
            "duplicate withdrawal must fail"
        );
    }
    assert_eq!(
        token.balanceOf(runtime.app_address()).call().await?,
        U256::ZERO
    );
    Ok(())
}
