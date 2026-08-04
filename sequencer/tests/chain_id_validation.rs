// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `run`-side bootstrap guards after the setup/run split.
//!
//! `run` no longer takes `--chain-id` / `--app-address` — it reads the pinned
//! deployment identity from the DB that `setup` created. These tests lock the
//! refusal paths that protect that handoff, without needing the on-chain
//! contracts that `setup`'s L1 discovery requires (those live in the full
//! rollups-e2e harness):
//!
//!   - **No setup** → `BootstrapError::SetupNotComplete` (terminal; operator
//!     must run `setup`). Fires before any L1 contact.
//!   - **Wrong signing key** → `IdentityError::Mismatch { batch_submitter_address }`
//!     (the key's address must match the pinned submitter). Fires before L1.
//!   - **Wrong-chain RPC** (review F6) → `ChainIdMismatch`: a reachable RPC
//!     whose `eth_chainId` differs from the pinned chain id is refused. Needs
//!     a live RPC, so it uses Anvil.
//!   - **Matching-chain RPC** positive control: a matching chain must NOT
//!     produce `ChainIdMismatch`.

use std::time::Duration;

use alloy_primitives::{Address, address};
use clap::Parser;
use sequencer::runtime::{BootstrapError, IdentityError, RunError};
use sequencer::storage::{DeploymentIdentity, Storage};
use sequencer::{Cli, Command};
use tempfile::TempDir;

// Anvil's default devnet private key #0 and its address.
const ANVIL_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const ANVIL_ADDR: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
const OTHER_KEY: &str = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const TEST_APP_ADDR: Address = address!("0x1111111111111111111111111111111111111111");
const TEST_INPUT_BOX: Address = address!("0x2222222222222222222222222222222222222222");

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

/// Parse a `run` config through the harness CLI and extract it.
fn run_config(data_dir: &str, eth_rpc_url: &str, key: &str) -> sequencer::RunConfig {
    let cli = Cli::try_parse_from([
        "sequencer",
        "run",
        "--http-addr",
        "127.0.0.1:0",
        "--data-dir",
        data_dir,
        "--eth-rpc-url",
        eth_rpc_url,
        "--batch-submitter-private-key",
        key,
    ])
    .expect("parse run config");
    match cli.command {
        Command::Run(c) => *c,
        other => panic!("expected run subcommand, got {other:?}"),
    }
}

/// Seed a pinned deployment identity (chain id `chain_id`, submitter
/// `submitter`) and mark setup complete — the minimal DB state `run` expects
/// from a completed `setup`, without `setup`'s L1 discovery.
fn seed_setup_complete(db_path: &str, chain_id: u64, submitter: Address) {
    let mut storage = Storage::open(db_path).expect("open db for seed");
    storage
        .load_or_insert_deployment_identity(DeploymentIdentity {
            chain_id,
            app_address: TEST_APP_ADDR,
            input_box_address: TEST_INPUT_BOX,
            app_deployment_block: 100,
            batch_submitter_address: submitter,
        })
        .expect("seed deployment identity");
    storage.mark_setup_complete().expect("mark setup complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_when_setup_incomplete() {
    // Fresh DB, no marker: `run` must refuse before touching L1.
    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();
    let config = run_config(data_dir, "http://127.0.0.1:1", ANVIL_KEY);

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        sequencer::run::<app_core::application::WalletApp>(config),
    )
    .await
    .expect("run() must return quickly without a setup-complete marker");

    assert!(
        matches!(
            result,
            Err(RunError::Bootstrap(BootstrapError::SetupNotComplete))
        ),
        "expected SetupNotComplete, got: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_on_submitter_key_mismatch() {
    // Setup pinned ANVIL_ADDR as the submitter; running with OTHER_KEY
    // (a different address) must fail loud before any L1 contact.
    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();
    let db_path = format!("{data_dir}/sequencer.db");
    seed_setup_complete(&db_path, 31_337, ANVIL_ADDR);

    let config = run_config(data_dir, "http://127.0.0.1:1", OTHER_KEY);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        sequencer::run::<app_core::application::WalletApp>(config),
    )
    .await
    .expect("run() must return quickly on key/identity mismatch");

    match result {
        Err(RunError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch {
            fields,
            ..
        }))) => assert_eq!(fields, "batch_submitter_address"),
        other => panic!("expected batch_submitter_address mismatch, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_on_wrong_chain_rpc() {
    // Pinned chain id 31337, but the (reachable) RPC reports a different
    // chain id — review F6: a wrong-chain RPC after setup must be refused.
    require_anvil();
    let anvil = alloy::node_bindings::Anvil::default().chain_id(99).spawn();
    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();
    let db_path = format!("{data_dir}/sequencer.db");
    // Submitter = anvil key address so the key check passes and we reach the
    // chain-id check.
    seed_setup_complete(&db_path, 31_337, ANVIL_ADDR);

    let config = run_config(data_dir, &anvil.endpoint(), ANVIL_KEY);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        sequencer::run::<app_core::application::WalletApp>(config),
    )
    .await
    .expect("run() must return quickly on chain-id mismatch");

    match result {
        Err(RunError::Bootstrap(BootstrapError::ChainIdMismatch { rpc, config })) => {
            assert_eq!(rpc, 99);
            assert_eq!(config, 31_337);
        }
        other => panic!("expected ChainIdMismatch, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_accepts_matching_chain_rpc() {
    // Positive control: a matching chain id must NOT produce ChainIdMismatch.
    // The DB has identity + marker but no genesis snapshot, so `run` passes
    // the chain-id check and then refuses at the always-load gate with
    // SetupNotComplete — a deterministic proof the chain-id guard let it
    // through (the gate sits immediately after the chain-id check, before any
    // recovery write).
    require_anvil();
    let anvil = alloy::node_bindings::Anvil::default().spawn(); // chain id 31337
    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();
    let db_path = format!("{data_dir}/sequencer.db");
    seed_setup_complete(&db_path, 31_337, ANVIL_ADDR);

    let config = run_config(data_dir, &anvil.endpoint(), ANVIL_KEY);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        sequencer::run::<app_core::application::WalletApp>(config),
    )
    .await
    .expect("run() returns promptly: chain-id passes, then the no-snapshot gate fires");

    // Assert the chain-id check positively did NOT fail, independent of which
    // gate fires next: if `seed_setup_complete` ever starts registering a
    // genesis snapshot, the `SetupNotComplete` arm below would stop firing and
    // silently stop witnessing "chain id passed" — this negative pins it.
    assert!(
        !matches!(
            result,
            Err(RunError::Bootstrap(
                BootstrapError::ChainIdMismatch { .. } | BootstrapError::ChainIdRpc { .. }
            ))
        ),
        "matching chain id must not produce a chain-id error, got: {result:?}"
    );
    assert!(
        matches!(
            result,
            Err(RunError::Bootstrap(BootstrapError::SetupNotComplete))
        ),
        "matching chain id must pass the chain-id check and reach the \
         always-load gate (SetupNotComplete), got: {result:?}"
    );
}
