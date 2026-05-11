// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! H7 regression: chain-id/deployment mismatch is caught early in bootstrap.
//!
//! The H7 hardening moved the live RPC chain-id check before deployment
//! identity writes and replaced `assert_eq!` with typed bootstrap errors. This
//! file locks two code paths where the check matters:
//!
//!   - Identity path: L1 is unreachable but a deployment identity exists with
//!     a different chain_id. Check fires before `InputReader::from_parts`.
//!   - Positive control: with a matched chain_id, `ChainIdMismatch` does NOT
//!     fire, so the check doesn't misfire on the happy path.
//!
//! The RPC path (L1 reachable, chain_id from `eth_chainId` mismatches) is
//! NOT covered here because `InputReader::new` needs a real InputBox contract
//! deployed at `config.app_address` before the chain-id check fires. That
//! setup only exists in the full rollups-e2e harness (after `just setup`) —
//! see `chain_id_mismatch_via_live_rpc_refuses_boot_test` there.

use std::time::Duration;

use alloy_primitives::{Address, address};
use app_core::application::{WalletApp, WalletConfig};
use clap::Parser;
use sequencer::RunConfig;
use sequencer::runtime::{BootstrapError, IdentityError, RunError};
use tempfile::TempDir;

// Anvil's default devnet private key #0.
const ANVIL_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const TEST_APP_ADDR: &str = "0x1111111111111111111111111111111111111111";

/// Verify that `anvil` is available. Panics with a clear message if not found.
fn require_anvil() {
    assert!(
        std::process::Command::new("anvil")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok(),
        "anvil not found on PATH — install Foundry (https://getfoundry.sh)"
    );
}

fn build_config(
    data_dir: &str,
    eth_rpc_url: &str,
    chain_id: u64,
) -> Result<RunConfig, clap::Error> {
    RunConfig::try_parse_from([
        "sequencer",
        "--http-addr",
        "127.0.0.1:0",
        "--data-dir",
        data_dir,
        "--eth-rpc-url",
        eth_rpc_url,
        "--chain-id",
        &chain_id.to_string(),
        "--app-address",
        TEST_APP_ADDR,
        "--batch-submitter-private-key",
        ANVIL_KEY,
    ])
}

fn build_app() -> WalletApp {
    WalletApp::new(WalletConfig::default())
}

// ── Deployment-identity fallback path ───────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_id_mismatch_from_deployment_identity_returns_typed_error() {
    // Scenario: L1 is unreachable, but a deployment identity exists from a
    // previous successful run. The stored chain_id does NOT match the current
    // config. The fallback arm must return a typed identity mismatch.

    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();

    // Pre-populate deployment identity with chain_id=31337.
    let db_path = format!("{data_dir}/sequencer.db");
    {
        let mut storage = sequencer::storage::Storage::open(&db_path).expect("open db for seed");
        storage
            .load_or_insert_deployment_identity(sequencer::storage::DeploymentIdentity {
                chain_id: 31_337,
                app_address: address!("0x1111111111111111111111111111111111111111"),
                input_box_address: Address::from_slice(&[0x22; 20]),
                input_box_genesis_block: 100,
                batch_submitter_address: address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            })
            .expect("seed deployment identity");
    }

    // Point the sequencer at an unreachable RPC (port 1, reliably refused) and
    // a MISMATCHED chain_id=1. L1 is unreachable → identity-fallback path runs
    // → stored chain_id (31337) mismatches config (1).
    let config = build_config(data_dir, "http://127.0.0.1:1", 1).expect("parse config");

    let result = tokio::time::timeout(Duration::from_secs(30), sequencer::run(build_app(), config))
        .await
        .expect("run() must return quickly on mismatch");

    match result {
        Err(RunError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch {
            fields,
            stored,
            expected,
        }))) => {
            assert_eq!(fields, "chain_id");
            assert_eq!(stored.chain_id, 31_337);
            assert_eq!(expected.chain_id, 1);
        }
        other => panic!("expected IdentityError::Mismatch, got: {other:?}"),
    }
}

// ── Positive: matched chain_id does NOT trigger ChainIdMismatch ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_id_match_does_not_produce_mismatch_error() {
    // Positive control: when chain_id matches, we should NOT get ChainIdMismatch.
    // (The sequencer then tries to start the full stack. We don't care about
    // that — a timeout counts as "didn't return ChainIdMismatch early", which
    // is what we want to verify.)
    require_anvil();

    let anvil = alloy::node_bindings::Anvil::default().spawn();
    let rpc_url = anvil.endpoint();
    let dir = TempDir::new().expect("tempdir");
    let config = build_config(dir.path().to_str().unwrap(), &rpc_url, 31_337)
        .expect("parse config with matching chain_id");

    // Short timeout: if ChainIdMismatch is going to fire, it fires fast.
    // A timeout means the check passed and the sequencer is running normally.
    let result =
        tokio::time::timeout(Duration::from_secs(3), sequencer::run(build_app(), config)).await;

    match result {
        Err(_timeout) => {} // expected — sequencer is running
        Ok(Err(RunError::Bootstrap(BootstrapError::ChainIdMismatch { rpc, config }))) => {
            panic!(
                "matched chain_id must not produce ChainIdMismatch, got rpc={rpc} config={config}"
            );
        }
        Ok(Err(other)) => {
            // Some other error is fine — we only care that it's not ChainIdMismatch.
            eprintln!(
                "sequencer returned non-mismatch error (expected under test conditions): {other:?}"
            );
        }
        Ok(Ok(())) => {
            panic!("sequencer should not complete run() in a short test window");
        }
    }
}
