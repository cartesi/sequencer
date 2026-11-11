// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `run`'s bootstrap guards on the identity `setup` pinned: no setup, a key for
//! another submitter, a binary with another direct-payload cap, and a
//! wrong-chain RPC (against Anvil) are refused, and a matching chain is not.
//! Setup's on-chain discovery is covered by the e2e suite.

use std::time::Duration;

use alloy_primitives::{Address, address};
use clap::Parser;
use sequencer::commands::{BootstrapError, CommandError, IdentityError};
use sequencer::l1::reader::InputReaderError;
use sequencer::storage::{DeploymentIdentity, Storage};
use sequencer::{Cli, Command};
use sequencer_core::application::Application;
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
        "--internal-http-addr",
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

/// The scheduler limits of the binary these tests run.
fn wallet_limits() -> sequencer_core::application::SchedulerLimits {
    app_core::application::WalletApp::scheduler_limits()
}

/// Seed a pinned deployment identity (chain id `chain_id`, submitter
/// `submitter`, scheduler limits `limits`), finalized-snapshot fact, and
/// completed setup lifecycle — the minimal local state needed to reach `run`'s
/// initial L1 sync without doing `setup`'s on-chain discovery. Uses the same
/// typed controller path `setup`
/// itself records through (admission → facts → completion), so this seed
/// cannot drift from the lifecycle schema or encode a state the controller
/// would never write.
fn seed_setup_complete(
    db_path: &str,
    chain_id: u64,
    submitter: Address,
    limits: sequencer_core::application::SchedulerLimits,
) {
    use sequencer::storage::LifecycleCommand;

    let mut storage = Storage::initialize_for_command(db_path, LifecycleCommand::Setup)
        .expect("initialize setup lifecycle");
    storage
        .pin_deployment_identity(DeploymentIdentity {
            chain_id,
            app_address: TEST_APP_ADDR,
            input_box_address: TEST_INPUT_BOX,
            app_deployment_block: 100,
            batch_submitter_address: submitter,
            max_direct_payload_bytes: limits.max_direct_payload_bytes,
            max_wait_blocks: limits.max_wait_blocks,
            fee_oracle: sequencer::storage::FeeOracleIdentity::Fixed { log_gas_price: 0 },
        })
        .expect("seed deployment identity");
    let snapshot_prefix = std::path::Path::new(db_path).with_file_name("seed-finalized");
    storage
        .publish_era(&snapshot_prefix, &sequencer::storage::NewEra::EMPTY_GENESIS)
        .expect("seed finalized snapshot fact");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_when_setup_incomplete() {
    // Fresh DB, no lifecycle: `run` must refuse before touching L1.
    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();
    let config = run_config(data_dir, "http://127.0.0.1:1", ANVIL_KEY);

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        sequencer::run::<app_core::application::WalletApp>(config),
    )
    .await
    .expect("run() must return quickly without a completed setup lifecycle");

    assert!(
        matches!(
            result,
            Err(CommandError::Bootstrap(
                BootstrapError::SetupNotComplete { .. }
            ))
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
    seed_setup_complete(&db_path, 31_337, ANVIL_ADDR, wallet_limits());

    let config = run_config(data_dir, "http://127.0.0.1:1", OTHER_KEY);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        sequencer::run::<app_core::application::WalletApp>(config),
    )
    .await
    .expect("run() must return quickly on key/identity mismatch");

    match result {
        Err(CommandError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch {
            fields,
            ..
        }))) => assert_eq!(fields, "batch_submitter_address"),
        other => panic!("expected batch_submitter_address mismatch, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_scheduler_limits_other_than_the_pinned_ones() {
    // Setup pinned a cap one byte larger than the binary's: storage would
    // classify directs the lane's application does not execute. Or it pinned
    // a window one block longer: acceptance would be judged by another rule
    // than the template's. Either way `run` must fail loud before any L1
    // contact.
    let binary = wallet_limits();
    let cases = [
        (
            sequencer_core::application::SchedulerLimits {
                max_direct_payload_bytes: binary.max_direct_payload_bytes + 1,
                ..binary
            },
            "max_direct_payload_bytes",
        ),
        (
            sequencer_core::application::SchedulerLimits {
                max_wait_blocks: binary.max_wait_blocks + 1,
                ..binary
            },
            "max_wait_blocks",
        ),
    ];
    for (pinned, field) in cases {
        let dir = TempDir::new().expect("tempdir");
        let data_dir = dir.path().to_str().unwrap();
        let db_path = format!("{data_dir}/sequencer.db");
        seed_setup_complete(&db_path, 31_337, ANVIL_ADDR, pinned);

        let config = run_config(data_dir, "http://127.0.0.1:1", ANVIL_KEY);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            sequencer::run::<app_core::application::WalletApp>(config),
        )
        .await
        .expect("run() must return quickly on a limits mismatch");

        match result {
            Err(CommandError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch {
                fields,
                ..
            }))) => assert_eq!(fields, field),
            other => panic!("expected {field} mismatch, got: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_on_wrong_chain_rpc() {
    // Pinned chain id 31337, but the (reachable) RPC reports a different
    // chain id: a wrong-chain RPC after setup must be refused.
    require_anvil();
    let anvil = alloy::node_bindings::Anvil::default().chain_id(99).spawn();
    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();
    let db_path = format!("{data_dir}/sequencer.db");
    // Submitter = anvil key address so the key check passes and we reach the
    // chain-id check.
    seed_setup_complete(&db_path, 31_337, ANVIL_ADDR, wallet_limits());

    let config = run_config(data_dir, &anvil.endpoint(), ANVIL_KEY);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        sequencer::run::<app_core::application::WalletApp>(config),
    )
    .await
    .expect("run() must return quickly on chain-id mismatch");

    match result {
        Err(
            error @ CommandError::Bootstrap(BootstrapError::StartupSync(
                InputReaderError::ChainIdMismatch { rpc, expected },
            )),
        ) => {
            assert_eq!(rpc, 99);
            assert_eq!(expected, 31_337);
            assert_eq!(error.exit_code(), sequencer::commands::error::EXIT_TERMINAL);
        }
        other => panic!("expected ChainIdMismatch, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_accepts_matching_chain_rpc() {
    // Positive control: a matching chain id must NOT produce ChainIdMismatch.
    // The DB has a finalized-snapshot fact whose artifact is deliberately
    // absent. `run` must pass the initial reader chain-id check and reach
    // task-free runtime preparation, where loading that artifact fails.
    require_anvil();
    let anvil = alloy::node_bindings::Anvil::default().spawn(); // chain id 31337
    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();
    let db_path = format!("{data_dir}/sequencer.db");
    seed_setup_complete(&db_path, 31_337, ANVIL_ADDR, wallet_limits());

    let config = run_config(data_dir, &anvil.endpoint(), ANVIL_KEY);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        sequencer::run::<app_core::application::WalletApp>(config),
    )
    .await
    .expect("run() returns promptly: chain-id passes, then snapshot preparation fails");

    // The later artifact failure proves startup crossed the initial sync
    // boundary, which includes the reader's chain-id verification.
    assert!(
        matches!(result, Err(CommandError::ReferencedSnapshotArtifact { .. })),
        "matching chain id must reach snapshot preparation, got: {result:?}"
    );
}

/// Seed a void history on a completed setup: one open batch whose first frame
/// is at safe block 10, under a safe head from which no later block can
/// accept it (`H + 1 - s == max_wait_blocks`).
fn seed_void_history(db_path: &str, limits: sequencer_core::application::SchedulerLimits) {
    let mut storage = Storage::open(db_path).expect("open storage");
    storage
        .append_safe_inputs(10, &[], ANVIL_ADDR)
        .expect("first safe head");
    storage
        .initialize_open_state(10, sequencer::storage::SafeInputRange::empty_at(0))
        .expect("open the era's first batch");
    storage
        .append_safe_inputs(10 + limits.max_wait_blocks - 1, &[], ANVIL_ADDR)
        .expect("the safe head that voids the batch");
}

/// The batches as `(batch_index, nonce, sealed)`.
fn batch_rows(db_path: &str) -> Vec<(i64, i64, bool)> {
    Storage::open(db_path)
        .expect("open storage")
        .raw_read(|tx| {
            tx.prepare(
                "SELECT batch_index, nonce, sealed_at_ms IS NOT NULL \
                 FROM batches ORDER BY batch_index",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect()
        })
        .expect("read batches")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_a_void_history_with_l1_unreachable_on_every_start() {
    // The provider is unreachable (port 1): the void history is a fact of
    // the database alone, so `run` refuses before any L1 call, before any
    // batch is opened, and again on the next start.
    let dir = TempDir::new().expect("tempdir");
    let data_dir = dir.path().to_str().unwrap();
    let db_path = format!("{data_dir}/sequencer.db");
    seed_setup_complete(&db_path, 31_337, ANVIL_ADDR, wallet_limits());
    seed_void_history(&db_path, wallet_limits());
    let before = batch_rows(&db_path);

    for start in ["first", "second"] {
        let config = run_config(data_dir, "http://127.0.0.1:1", ANVIL_KEY);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            sequencer::run::<app_core::application::WalletApp>(config),
        )
        .await
        .expect("run() must refuse promptly without L1");
        match result {
            Err(
                ref error @ CommandError::Bootstrap(BootstrapError::TerminalFact(
                    sequencer::storage::TerminalFact::VoidHistory {
                        lag,
                        max_wait_blocks,
                    },
                )),
            ) => {
                assert_eq!(lag.first_frame_safe_block, 10, "{start} start");
                assert_eq!(max_wait_blocks, wallet_limits().max_wait_blocks);
                assert_eq!(error.exit_code(), sequencer::commands::error::EXIT_TERMINAL);
                assert!(
                    error.to_string().contains("the history is void"),
                    "{start} start: {error}"
                );
            }
            other => panic!("{start} start: expected a void history, got: {other:?}"),
        }
        assert_eq!(
            batch_rows(&db_path),
            before,
            "{start} start changed the batches"
        );
    }
}
