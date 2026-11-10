// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Setup end to end against Anvil, with a real InputBox and application and a
//! history already on chain: directs, the submitter's accepted and rejected
//! batches, a skipped op, and directs still queued at the stop block. The
//! result must equal what the canonical runner executes over the same inputs.

use std::path::Path;

use alloy::network::EthereumWallet;
use alloy::node_bindings::{Anvil, AnvilInstance};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::BlockNumberOrTag;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy_primitives::{Address, B256};
use alloy_sol_types::SolStruct;
use app_core::application::{WalletApp, WalletConfig};
use cartesi_rollups_contracts::application_factory::ApplicationFactory;
use cartesi_rollups_contracts::input_box::InputBox;
use sequencer_core::application::{AppError, Application, CanonicalState};
use sequencer_core::batch::{Batch, Frame, WireUserOp};
use sequencer_core::history::{HistoryClaim, L2TxCount};
use sequencer_core::l2_tx::L2Tx;
use sequencer_core::scheduler::{CanonicalRunner, SchedulerConfig, SchedulerExport, TxFate};
use ssz::Encode;

use super::*;
use crate::ingress::inclusion_lane::dump_info;
use crate::storage::L2TxContext;

/// Anvil's dev accounts 0 (the submitter) and 1 (a depositor of directs).
const SUBMITTER_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const DEPOSITOR_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHAIN_ID: u64 = 31337;

struct Chain {
    _anvil: AnvilInstance,
    url: String,
    provider: DynProvider,
    input_box: Address,
    app: Address,
    app_deployment_block: u64,
    submitter: Address,
    depositor: Address,
    /// Each posted input's inclusion block, by index.
    blocks: Vec<u64>,
}

impl Chain {
    /// A chain with the InputBox and the application deployed, and nothing
    /// posted yet.
    async fn deploy() -> Self {
        let anvil = Anvil::default()
            .args(["--slots-in-an-epoch", "1"])
            .timeout(30_000)
            .spawn();
        let submitter_key: PrivateKeySigner = SUBMITTER_KEY.parse().unwrap();
        let depositor_key: PrivateKeySigner = DEPOSITOR_KEY.parse().unwrap();
        let (submitter, depositor) = (submitter_key.address(), depositor_key.address());
        let mut wallet = EthereumWallet::from(submitter_key);
        wallet.register_signer(depositor_key);
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(anvil.endpoint_url())
            .erased();

        let input_box = *InputBox::deploy(provider.clone()).await.unwrap().address();
        let factory = ApplicationFactory::deploy(provider.clone(), Address::repeat_byte(0x55))
            .await
            .unwrap();
        let new_application = factory.newApplication_0(
            submitter,
            submitter,
            B256::repeat_byte(1),
            input_box,
            ApplicationFactory::WithdrawalConfig {
                guardian: Address::ZERO,
                log2LeavesPerAccount: 0,
                log2MaxNumOfAccounts: 0,
                accountsDriveStartIndex: 0,
                withdrawalOutputBuilder: Address::ZERO,
            },
        );
        let app = new_application.call().await.unwrap();
        let receipt = new_application
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        assert!(receipt.status());
        Self {
            url: anvil.endpoint_url().to_string(),
            _anvil: anvil,
            provider,
            input_box,
            app,
            app_deployment_block: receipt.block_number.unwrap(),
            submitter,
            depositor,
            blocks: Vec::new(),
        }
    }

    /// The deployed chain with the shared history posted and safe:
    ///
    /// | index | input |
    /// |---|---|
    /// | 0, 1 | directs |
    /// | 2 | batch 0, a frame at input 0's block: drains 0, one op |
    /// | 3 | a batch with a wrong nonce: rejected |
    /// | 4 | a direct |
    /// | 5 | batch 1, a frame at input 1's block: drains 1, one op, one skipped |
    /// | 6 | a direct |
    ///
    /// Inputs 4 and 6 are still queued at the stop block. After the first
    /// five, inputs 1 and 4 are queued.
    async fn with_history() -> Self {
        let mut chain = Self::deploy().await;
        chain.direct(vec![1]).await;
        chain.direct(vec![2]).await;
        let frame = chain.blocks[0];
        chain.batch(0, frame, &[0]).await;
        let frame = chain.latest().await;
        chain.batch(5, frame, &[1]).await;
        chain.direct(vec![3]).await;
        let frame = chain.blocks[1];
        chain.batch(1, frame, &[1, 7]).await;
        chain.direct(vec![4]).await;
        chain.mine_until_safe(*chain.blocks.last().unwrap()).await;
        chain
    }

    async fn post(&mut self, sender: Address, payload: Vec<u8>) {
        let receipt = InputBox::new(self.input_box, &self.provider)
            .addInput(self.app, payload.into())
            .from(sender)
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        assert!(receipt.status());
        self.blocks.push(receipt.block_number.unwrap());
    }

    async fn direct(&mut self, payload: Vec<u8>) {
        self.post(self.depositor, payload).await;
    }

    /// A batch with one frame at `safe_block` holding one op of the user per
    /// nonce in `op_nonces`.
    async fn batch(&mut self, nonce: u64, safe_block: u64, op_nonces: &[u32]) {
        let user: PrivateKeySigner = format!("{:064x}", 7).parse().unwrap();
        let domain = sequencer_core::build_input_domain(CHAIN_ID, self.app);
        let user_ops = op_nonces
            .iter()
            .map(|&nonce| {
                let op = sequencer_core::user_op::UserOp {
                    nonce,
                    max_fee: 0,
                    data: vec![0xde, 0xad].into(),
                };
                let signature = user
                    .sign_hash_sync(&op.eip712_signing_hash(&domain))
                    .unwrap();
                WireUserOp {
                    nonce,
                    max_fee: 0,
                    data: op.data.to_vec(),
                    signature: signature.as_bytes().to_vec(),
                }
            })
            .collect();
        let payload = Batch {
            nonce,
            frames: vec![Frame {
                safe_block,
                fee_price: 0,
                user_ops,
            }],
        }
        .as_ssz_bytes();
        self.post(self.submitter, payload).await;
    }

    async fn latest(&self) -> u64 {
        self.provider.get_block_number().await.unwrap()
    }

    async fn safe(&self) -> u64 {
        self.provider
            .get_block_by_number(BlockNumberOrTag::Safe)
            .await
            .unwrap()
            .unwrap()
            .header
            .number
    }

    async fn mine_until_safe(&self, block: u64) {
        while self.safe().await < block {
            self.provider
                .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
                .await
                .unwrap();
        }
    }

    fn setup_config(&self, data_dir: &Path, extra: &[&str]) -> SetupConfig {
        use crate::harness::{Cli, Command};
        use clap::Parser;
        let app = self.app.to_string();
        let submitter = self.submitter.to_string();
        let mut args = vec![
            "sequencer",
            "setup",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--eth-rpc-url",
            &self.url,
            "--chain-id",
            "31337",
            "--app-address",
            &app,
            "--batch-submitter-address",
            &submitter,
            "--seconds-per-block",
            "1",
        ];
        if !extra.contains(&"--fixed-log-gas-price") {
            args.extend_from_slice(&["--fixed-log-gas-price", "0"]);
        }
        args.extend_from_slice(extra);
        let Command::Setup(config) = Cli::try_parse_from(args).unwrap().command else {
            panic!("expected setup subcommand");
        };
        *config
    }

    fn wallet(&self) -> WalletApp {
        WalletApp::new(WalletConfig {
            sequencer_address: self.submitter,
            ..WalletConfig::default()
        })
    }

    fn scheduler_config(&self) -> SchedulerConfig {
        SchedulerConfig::for_application::<WalletApp>(self.submitter)
    }

    /// The canonical runner over the first `count` inputs of the synced table.
    fn reference(
        &self,
        storage: &mut Storage,
        count: u64,
    ) -> (CanonicalRunner<WalletApp>, Vec<Row>) {
        let mut runner = CanonicalRunner::genesis(self.wallet(), self.scheduler_config());
        let mut rows = Vec::new();
        for input in storage.l1_inputs_page(0, count, usize::MAX).unwrap() {
            for record in runner.process_input(input).unwrap().txs {
                if let TxFate::Done { index } = record.fate {
                    rows.push((
                        index.get(),
                        record.scheduled.tx,
                        record.scheduled.batch_nonce,
                    ));
                }
            }
        }
        (runner, rows)
    }
}

/// One executed row: its offset, the transaction and its batch nonce.
type Row = (u64, L2Tx, u64);

fn with_key(extra: &[&'static str]) -> Vec<&'static str> {
    let mut args = vec!["--batch-submitter-private-key", SUBMITTER_KEY];
    args.extend_from_slice(extra);
    args
}

/// The rows the feed serves from `from` on.
fn feed(storage: &mut Storage, from: u64) -> Vec<Row> {
    let version = storage.history_version().unwrap();
    let page = storage
        .canonical_history_page(
            HistoryClaim {
                version,
                next_input: L2TxCount::new(from),
            },
            1024,
        )
        .unwrap();
    page.rows
        .into_iter()
        .map(|row| match row.context {
            L2TxContext::UserOp { tx, batch_nonce } => {
                (row.offset.get(), L2Tx::UserOp(tx), batch_nonce)
            }
            L2TxContext::Direct {
                tx, batch_nonce, ..
            } => (row.offset.get(), L2Tx::Direct(tx), batch_nonce),
        })
        .collect()
}

/// How many rows the rebuilt history holds.
fn rebuilt_rows(storage: &mut Storage) -> u64 {
    storage
        .read(|tx| tx.query_row("SELECT COUNT(*) FROM rebuilt_history", [], |row| row.get(0)))
        .map(|count: i64| u64::try_from(count).unwrap())
        .unwrap()
}

fn genesis_wallet(chain: &Chain) -> impl FnOnce() -> Result<WalletApp, AppError> {
    let app = chain.wallet();
    move || Ok(app)
}

/// A machine reader that answers with `runner`'s export and state, as the
/// stored machine of the same history would.
fn machine_like(runner: &CanonicalRunner<WalletApp>, export: SchedulerExport) -> ReadMachineExport {
    let state = runner.app().canonical_snapshot_bytes().unwrap();
    Box::new(move |_machine, state_file| {
        std::fs::write(state_file, &state).unwrap();
        Ok(export)
    })
}

fn no_machine() -> ReadMachineExport {
    Box::new(|_, _| unreachable!("a genesis setup reads no machine"))
}

fn setup_refusal(error: CommandError) -> SetupError {
    match error {
        CommandError::Bootstrap(BootstrapError::Setup(error)) => *error,
        other => panic!("expected a setup refusal, got {other:?}"),
    }
}

/// Assert the published era is the reference's at the stop block, row for
/// row, with the baseline's bytes.
fn assert_matches_reference(
    chain: &Chain,
    db_path: &str,
    base_count: u64,
    summary: &SetupSummary,
) -> CanonicalRunner<WalletApp> {
    let mut storage = Storage::open(db_path).unwrap();
    let era = summary.era.expect("setup publishes an era");
    let history = storage.era().unwrap();
    let stop_block = history.stop_block;
    assert_eq!(era.stop_block, stop_block);
    let through_stop = storage.safe_input_count_through_block(stop_block).unwrap();
    assert_eq!(
        through_stop, 7,
        "every posted input lies at or below the stop block"
    );
    let (runner, reference) = chain.reference(&mut storage, through_stop);
    let state = runner.scheduler().state();

    assert_eq!(history.base.get(), base_count);
    assert_eq!(history.rebuilt_end, runner.app().l2_tx_count());
    assert_eq!(history.first_queued_input, state.first_queued_input);
    assert_eq!((state.first_queued_input, state.fridge_len), (4, 2));
    assert_eq!(history.anchor_batch_nonce, state.next_batch_nonce);
    assert_eq!(state.next_batch_nonce, 2);
    assert_eq!(
        era,
        NewEra {
            base: L2TxCount::new(base_count),
            rebuilt_end: runner.app().l2_tx_count(),
            stop_block,
            first_queued_input: 4,
            anchor_batch_nonce: 2,
        }
    );

    // The feed serves, from the base count, what the reference executed.
    let expected: Vec<Row> = reference
        .into_iter()
        .filter(|(offset, _, _)| *offset >= base_count)
        .collect();
    assert!(!expected.is_empty());
    assert_eq!(feed(&mut storage, base_count), expected);

    // The baseline is the reference's state at the end of the stop block, and
    // the era's first comparison checkpoint.
    let baseline = storage.latest_snapshot().unwrap().expect("baseline");
    let prefix = dump_info::app_prefix(&baseline.dump.prefix);
    assert_eq!(
        std::fs::read(WalletApp::state_file_in_dump(&prefix)).unwrap(),
        runner.app().canonical_snapshot_bytes().unwrap()
    );
    let checkpoint = storage.finalized_dump().unwrap().expect("era baseline");
    assert_eq!(checkpoint.inclusion_block, stop_block);
    assert_eq!(checkpoint.l2_tx_count, runner.app().l2_tx_count());
    assert_eq!(checkpoint.next_batch_nonce, 2);
    assert!(
        storage.open_state().unwrap().is_none(),
        "setup opens no root"
    );

    assert_eq!(summary.through_block, stop_block);
    assert_eq!(
        summary.submitter_inputs_past_base,
        if base_count == 0 { 3 } else { 1 }
    );
    assert_eq!(summary.unsettled_wallet_txs, 0);
    runner
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_from_genesis_rebuilds_a_chain_with_batches() {
    let chain = Chain::with_history().await;
    let data_dir = tempfile::tempdir().unwrap();

    let summary = run_setup(
        chain.setup_config(data_dir.path(), &with_key(&[])),
        genesis_wallet(&chain),
        no_machine(),
    )
    .await
    .unwrap();

    assert_eq!(
        (summary.base_l1_input_count, summary.base_l2_tx_count),
        (0, 0)
    );
    assert_eq!(summary.inputs_to_replay, 7);
    let db_path = crate::commands::config::db_path_in(data_dir.path().to_str().unwrap());
    let runner = assert_matches_reference(&chain, &db_path, 0, &summary);
    assert!(
        feed(&mut Storage::open(&db_path).unwrap(), 0)
            .iter()
            .any(|(_, tx, _)| matches!(tx, L2Tx::UserOp(_))),
        "the rebuilt history holds the accepted batches' ops"
    );
    let skipped = {
        let mut storage = Storage::open(&db_path).unwrap();
        let mut reference = CanonicalRunner::genesis(chain.wallet(), chain.scheduler_config());
        storage
            .l1_inputs_page(0, 7, usize::MAX)
            .unwrap()
            .into_iter()
            .flat_map(|input| reference.process_input(input).unwrap().txs)
            .filter(|record| matches!(record.fate, TxFate::Skipped(_)))
            .count()
    };
    assert_eq!(skipped, 1, "the canonical run of this chain skips one op");
    assert_eq!(
        rebuilt_rows(&mut Storage::open(&db_path).unwrap()),
        runner.app().l2_tx_count().get(),
        "only executed transactions are stored: the skipped op has no row"
    );
    let left: Vec<String> = std::fs::read_dir(data_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        left.iter()
            .all(|name| !name.starts_with("sequencer-setup-scratch")),
        "the scratch directory is removed: {left:?}"
    );
}

/// From a machine after five inputs, with two directs queued; then `run`
/// starts on the era and its root frame drains the directs still queued at
/// the stop block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_from_a_base_with_queued_directs_then_run_drains_them() {
    let chain = Chain::with_history().await;
    let data_dir = tempfile::tempdir().unwrap();
    let db_path = crate::commands::config::db_path_in(data_dir.path().to_str().unwrap());

    // The base: the canonical runner over the first five inputs, read from a
    // scratch sync of the same chain.
    let scratch = crate::test_support::temp_db("setup-base-source");
    let base = {
        let lock = ProcessLock::test();
        let mut reader = discover(
            &chain.setup_config(data_dir.path(), &with_key(&[])),
            &scratch.path,
            crate::storage::test_helpers::default_protocol_timing(),
            lock,
        )
        .await
        .unwrap();
        let mut storage =
            Storage::initialize_for_command(&scratch.path, LifecycleCommand::Setup).unwrap();
        storage
            .pin_deployment_identity(DeploymentIdentity {
                chain_id: CHAIN_ID,
                app_address: chain.app,
                input_box_address: chain.input_box,
                app_deployment_block: chain.app_deployment_block,
                batch_submitter_address: chain.submitter,
                fee_oracle: FeeOracleIdentity::Fixed { log_gas_price: 0 },
            })
            .unwrap();
        reader.sync_to_current_safe_head().await.unwrap();
        chain.reference(&mut storage, 5).0
    };
    let export = base.export();
    assert_eq!(
        (
            export.l1_input_count,
            export.first_queued_input,
            export.fridge_len
        ),
        (5, 1, 2)
    );

    let summary = run_setup(
        chain.setup_config(
            data_dir.path(),
            &with_key(&["--base", "/machines/after-five"]),
        ),
        || -> Result<WalletApp, AppError> { unreachable!("a machine base needs no genesis") },
        machine_like(&base, export),
    )
    .await
    .unwrap();
    assert_eq!(
        (summary.base_l1_input_count, summary.base_l2_tx_count),
        (5, export.l2_tx_count)
    );
    assert_eq!(summary.inputs_to_replay, 2);
    let runner = assert_matches_reference(&chain, &db_path, export.l2_tx_count, &summary);
    let rebuilt_end = runner.app().l2_tx_count().get();

    // `run`'s startup: sync with the frontier on, then the root frame at its
    // own safe head.
    let identity = crate::commands::load_setup_identity(data_dir.path().to_str().unwrap()).unwrap();
    let timing = crate::storage::test_helpers::default_protocol_timing();
    let lock = ProcessLock::acquire(data_dir.path().to_str().unwrap()).unwrap();
    let mut reader = InputReader::new(
        db_path.clone(),
        crate::l1::reader::InputReaderConfig {
            rpc_url: chain.url.clone(),
            allow_insecure_rpc: false,
            app_address: chain.app,
            poll_interval: std::time::Duration::from_secs(1),
            expected_chain_id: CHAIN_ID,
        },
        timing,
        lock,
    )
    .await
    .unwrap();
    crate::recovery::run_startup_recovery(
        &db_path,
        &mut reader,
        &crate::l1::L1Config {
            identity,
            eth_rpc_url: chain.url.clone(),
            batch_submitter_private_key: crate::l1::SubmitterKey::new(SUBMITTER_KEY.to_owned()),
            allow_insecure_rpc: false,
        },
        &timing,
        WalletApp::scheduler_limits().max_direct_payload_bytes,
    )
    .await
    .unwrap();

    let mut storage = Storage::open(&db_path).unwrap();
    let root = storage.open_state().unwrap().expect("run opened the root");
    assert!(root.safe_block >= summary.through_block);
    let drained: Vec<(u64, u64, u64)> = feed(&mut storage, rebuilt_end)
        .into_iter()
        .map(|(offset, tx, batch_nonce)| match tx {
            L2Tx::Direct(direct) => (offset, direct.input_index, batch_nonce),
            other => panic!("the root frame drains only directs, got {other:?}"),
        })
        .collect();
    assert_eq!(
        drained,
        vec![(rebuilt_end, 4, 2), (rebuilt_end + 1, 6, 2)],
        "the root frame drains the directs queued at the stop block, in the anchor batch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_refuses_an_export_that_disagrees_with_l1_before_publishing() {
    let chain = Chain::with_history().await;
    let data_dir = tempfile::tempdir().unwrap();
    let db_path = crate::commands::config::db_path_in(data_dir.path().to_str().unwrap());
    // A machine at genesis that claims a queued direct L1 does not hold.
    let runner = CanonicalRunner::genesis(chain.wallet(), chain.scheduler_config());
    let export = SchedulerExport {
        fridge_len: 1,
        ..runner.export()
    };

    let error = run_setup(
        chain.setup_config(data_dir.path(), &with_key(&["--base", "/machines/bad"])),
        || -> Result<WalletApp, AppError> { unreachable!("a machine base needs no genesis") },
        machine_like(&runner, export),
    )
    .await
    .expect_err("an export that disagrees with L1 is refused");
    let refusal = setup_refusal(error);
    assert!(
        matches!(
            refusal,
            SetupError::ExportDisagrees {
                fridge_len: 1,
                admitted: 0,
                ..
            }
        ),
        "{refusal:?}"
    );
    let mut storage = Storage::open_read_only(&db_path).unwrap();
    assert!(
        !storage.is_setup_complete().unwrap(),
        "nothing is published"
    );
    assert_eq!(rebuilt_rows(&mut storage), 0);
    assert!(
        storage
            .latest_terminal_fault()
            .unwrap()
            .is_some_and(|fault| fault.cause.contains("disagrees with L1")),
        "the terminal cause is in the black box"
    );
}

/// A setup before the deployment block is safe exits 20 and leaves the
/// directory empty, so the retry may run in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_not_yet_safe_exits_20_and_leaves_the_directory_empty() {
    let chain = Chain::deploy().await;
    assert!(chain.safe().await < chain.app_deployment_block);
    let data_dir = tempfile::tempdir().unwrap();

    let error = run_setup(
        chain.setup_config(data_dir.path(), &with_key(&[])),
        genesis_wallet(&chain),
        no_machine(),
    )
    .await
    .expect_err("the deployment is not safe yet");
    assert!(
        matches!(
            error,
            CommandError::Bootstrap(BootstrapError::Identity(
                IdentityError::DeploymentNotYetSafe { .. }
            ))
        ),
        "{error:?}"
    );
    assert_eq!(
        error.exit_code(),
        crate::commands::error::EXIT_RESTART_TRANSIENT
    );
    let left: Vec<String> = std::fs::read_dir(data_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec![LOCK_FILE_NAME.to_owned()]);

    // Once it is safe, the retry in the same directory completes.
    chain.mine_until_safe(chain.app_deployment_block).await;
    let summary = run_setup(
        chain.setup_config(data_dir.path(), &with_key(&[])),
        genesis_wallet(&chain),
        no_machine(),
    )
    .await
    .unwrap();
    assert_eq!(summary.era.unwrap().rebuilt_end, L2TxCount::ZERO);
}

/// The dry run needs no key, reports the base and the old instance's
/// activity past it, and never touches the data directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dry_run_reports_and_changes_nothing() {
    let chain = Chain::with_history().await;
    let parent = tempfile::tempdir().unwrap();
    let absent = parent.path().join("absent");
    let occupied = parent.path().join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    std::fs::write(occupied.join("sequencer.db"), b"someone else's").unwrap();

    for data_dir in [&absent, &occupied] {
        let summary = run_setup(
            chain.setup_config(data_dir, &["--dry-run"]),
            genesis_wallet(&chain),
            no_machine(),
        )
        .await
        .unwrap();
        assert_eq!(
            summary,
            SetupSummary {
                base_l1_input_count: 0,
                base_l2_tx_count: 0,
                submitter_inputs_past_base: 3,
                unsettled_wallet_txs: 0,
                inputs_to_replay: 7,
                through_block: chain.safe().await,
                era: None,
            }
        );
        assert!(summary.to_string().contains("re-judges"));
    }
    assert!(
        !absent.exists(),
        "the dry run never creates the data directory"
    );
    let entries: Vec<_> = std::fs::read_dir(&occupied).unwrap().collect();
    assert_eq!(
        entries.len(),
        1,
        "the dry run never touches the data directory"
    );
    assert_eq!(
        std::fs::read(occupied.join("sequencer.db")).unwrap(),
        b"someone else's"
    );
}

/// The escape check against a table synced from the chain: a submitter
/// input past the stop block refuses, and so does a wallet left unsettled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_escape_check_refuses_activity_past_the_stop_block() {
    let chain = Chain::with_history().await;
    let scratch = crate::test_support::temp_db("setup-escape");
    let data_dir = tempfile::tempdir().unwrap();
    let mut reader = discover(
        &chain.setup_config(data_dir.path(), &with_key(&[])),
        &scratch.path,
        crate::storage::test_helpers::default_protocol_timing(),
        ProcessLock::test(),
    )
    .await
    .unwrap();
    let mut storage =
        Storage::initialize_for_command(&scratch.path, LifecycleCommand::Setup).unwrap();
    storage
        .pin_deployment_identity(DeploymentIdentity {
            chain_id: CHAIN_ID,
            app_address: chain.app,
            input_box_address: chain.input_box,
            app_deployment_block: chain.app_deployment_block,
            batch_submitter_address: chain.submitter,
            fee_oracle: FeeOracleIdentity::Fixed { log_gas_price: 0 },
        })
        .unwrap();
    reader.sync_to_current_safe_head().await.unwrap();
    let safe_block = storage.current_safe_block().unwrap().unwrap();
    let settled = read_submitter_wallet_nonces(&chain.url, chain.submitter, safe_block, false)
        .await
        .unwrap();
    assert_eq!(settled.0, settled.1);

    // The batch at input 5 lies past a stop block just below it.
    let stop_block = chain.blocks[5] - 1;
    match check_escape(&mut storage, chain.submitter, stop_block, settled) {
        Err(CommandError::Bootstrap(BootstrapError::SetupRefuse(
            SetupRefuse::SubmitterInputPastStop {
                found_block,
                safe_input_index: 5,
                ..
            },
        ))) => assert_eq!(found_block, chain.blocks[5]),
        other => panic!("expected SubmitterInputPastStop, got {other:?}"),
    }
    check_escape(&mut storage, chain.submitter, chain.blocks[6], settled).unwrap();

    // The old instance broadcasts a batch that is not mined yet.
    chain
        .provider
        .raw_request::<_, serde_json::Value>("evm_setAutomine".into(), (false,))
        .await
        .unwrap();
    let _pending = InputBox::new(chain.input_box, &chain.provider)
        .addInput(chain.app, vec![9].into())
        .from(chain.submitter)
        .send()
        .await
        .unwrap();
    let unsettled = read_submitter_wallet_nonces(&chain.url, chain.submitter, safe_block, false)
        .await
        .unwrap();
    let error = check_escape(&mut storage, chain.submitter, chain.blocks[6], unsettled)
        .expect_err("a wallet left unsettled refuses");
    assert!(
        matches!(
            error,
            CommandError::Bootstrap(BootstrapError::SetupRefuse(
                SetupRefuse::WalletUnsettledAfterFlush { .. }
            ))
        ),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), crate::commands::error::EXIT_SETUP_ESCAPE);
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A key that does not sign for the pinned submitter, or a fixed price below
/// the fee floor, is refused with exit 30 before anything is written: no
/// base is read, no transaction is sent, and the directory holds only the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_key_or_a_price_below_the_floor_is_refused_before_any_write() {
    let chain = Chain::with_history().await;
    let nonce_before = chain
        .provider
        .get_transaction_count(chain.submitter)
        .await
        .unwrap();

    let data_dir = tempfile::tempdir().unwrap();
    let error = run_setup(
        chain.setup_config(
            data_dir.path(),
            &["--batch-submitter-private-key", DEPOSITOR_KEY],
        ),
        || -> Result<WalletApp, AppError> { unreachable!("a refused key reads no base") },
        Box::new(|_, _| unreachable!("a refused key reads no machine")),
    )
    .await
    .expect_err("the key signs for another address");
    assert!(
        matches!(
            &error,
            CommandError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch { fields, .. }))
                if fields == "batch_submitter_address"
        ),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), crate::commands::error::EXIT_TERMINAL);
    assert_eq!(entries(data_dir.path()), vec![LOCK_FILE_NAME.to_owned()]);
    assert_eq!(
        chain
            .provider
            .get_transaction_count(chain.submitter)
            .await
            .unwrap(),
        nonce_before,
        "nothing was sent from either account"
    );

    let data_dir = tempfile::tempdir().unwrap();
    let error = run_setup(
        chain.setup_config(
            data_dir.path(),
            &with_key(&["--fixed-log-gas-price", "-1357"]),
        ),
        || -> Result<WalletApp, AppError> { unreachable!("a refused price reads no base") },
        Box::new(|_, _| unreachable!("a refused price reads no machine")),
    )
    .await
    .expect_err("the price is below the floor");
    assert!(
        matches!(
            error,
            CommandError::Bootstrap(BootstrapError::FeeOracleMisconfig { .. })
        ),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), crate::commands::error::EXIT_TERMINAL);
    assert_eq!(entries(data_dir.path()), vec![LOCK_FILE_NAME.to_owned()]);
}

/// The dry run runs the scheduler's own resume checks too, so it refuses a
/// base that only setup's rebuild would otherwise find wrong. This export
/// queues nothing, so the table check passes, but it names a first queued
/// input past its own input count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dry_run_refuses_a_base_that_resume_refuses() {
    let chain = Chain::with_history().await;
    let data_dir = tempfile::tempdir().unwrap();
    let runner = CanonicalRunner::genesis(chain.wallet(), chain.scheduler_config());
    let export = SchedulerExport {
        first_queued_input: 2,
        ..runner.export()
    };

    let error = run_setup(
        chain.setup_config(data_dir.path(), &["--dry-run", "--base", "/machines/bad"]),
        || -> Result<WalletApp, AppError> { unreachable!("a machine base needs no genesis") },
        machine_like(&runner, export),
    )
    .await
    .expect_err("the dry run refuses what setup refuses");
    let refusal = setup_refusal(error);
    assert!(
        matches!(
            refusal,
            SetupError::ResumeRefused(
                sequencer_core::scheduler::ResumeError::FirstQueuedAfterCount {
                    first_queued_input: 2,
                    l1_input_count: 0,
                }
            )
        ),
        "{refusal:?}"
    );
    assert!(!data_dir.path().join(LOCK_FILE_NAME).exists());
}

/// Mines a block every `interval` until dropped.
struct Miner(tokio::task::JoinHandle<()>);

impl Miner {
    fn start(provider: DynProvider, interval: std::time::Duration) -> Self {
        Self(tokio::spawn(async move {
            loop {
                let _ = provider
                    .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
                    .await;
                tokio::time::sleep(interval).await;
            }
        }))
    }
}

impl Drop for Miner {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The old instance left a submitter transaction in the mempool. Setup's
/// flush settles that wallet slot, by the original or a no-op, and pins the
/// stop block at or after it; the era is then published.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_flushes_a_pending_submitter_transaction_before_the_stop_block() {
    let chain = Chain::with_history().await;
    let data_dir = tempfile::tempdir().unwrap();
    let settled = chain
        .provider
        .get_transaction_count(chain.submitter)
        .await
        .unwrap();

    chain
        .provider
        .raw_request::<_, serde_json::Value>("evm_setAutomine".into(), (false,))
        .await
        .unwrap();
    let _pending = InputBox::new(chain.input_box, &chain.provider)
        .addInput(chain.app, vec![9].into())
        .from(chain.submitter)
        .send()
        .await
        .unwrap();
    let pending = chain
        .provider
        .get_transaction_count(chain.submitter)
        .block_id(BlockNumberOrTag::Pending.into())
        .await
        .unwrap();
    assert_eq!(
        pending,
        settled + 1,
        "the old instance's transaction is pending"
    );
    let _miner = Miner::start(
        chain.provider.clone(),
        std::time::Duration::from_millis(100),
    );

    let summary = run_setup(
        chain.setup_config(data_dir.path(), &with_key(&[])),
        genesis_wallet(&chain),
        no_machine(),
    )
    .await
    .unwrap();
    let era = summary.era.expect("setup publishes an era");
    assert_eq!(summary.unsettled_wallet_txs, 0);
    let at_stop = chain
        .provider
        .get_transaction_count(chain.submitter)
        .block_id(BlockNumberOrTag::Number(era.stop_block).into())
        .await
        .unwrap();
    assert!(
        at_stop >= pending,
        "the pending slot is consumed at or below the stop block {}: nonce {at_stop}",
        era.stop_block
    );
    let db_path = crate::commands::config::db_path_in(data_dir.path().to_str().unwrap());
    assert!(
        Storage::open(&db_path)
            .unwrap()
            .is_setup_complete()
            .unwrap()
    );
}
