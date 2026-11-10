// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! A reader replays the era's raw L1 through the canonical scheduler, rebuilds
//! the transfer history a wallet dump cannot hold, and crosses the rebuilt
//! baseline into the live feed.

mod recovery_compatibility;

use std::net::SocketAddr;
use std::time::Duration;

use alloy::signers::{SignerSync, local::PrivateKeySigner};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{Eip712Domain, SolCall, SolStruct};
use app_core::application::{
    DepositNotice, Method, Transfer, TransferNotice, WalletApp, WalletConfig,
};
use futures_util::StreamExt;
use sequencer_core::api::WsTxMessage;
use sequencer_core::application::{
    AppOutput, AppOutputs, Application, CanonicalState, Executed, execute,
};
use sequencer_core::batch::{Batch, Frame, WireUserOp};
use sequencer_core::history::{HistoryClaim, HistoryPolicyError, L2TxCount};
use sequencer_core::history_api::HistoricalL1InputStart;
use sequencer_core::l2_tx::{Direct, L2Tx};
use sequencer_core::scheduler::{
    BatchRejectReason, CanonicalRunner, InputOutcome, L1Input, SchedulerConfig,
};
use sequencer_core::user_op::UserOp;
use sequencer_rust_client::{HistoryReadError, SequencerClient};
use ssz::Encode;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use crate::http::{self, ApiConfig};
use crate::ingress::inclusion_lane::{PendingUserOp, dump_info};
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::test_helpers::default_protocol_timing;
use crate::storage::{
    DirectInputExecution, FrontierMode, IngestedSafeInput, LifecycleCommand, SafeInputRange,
    Storage,
};
use crate::test_support::{pin_test_deployment_identity, temp_db};

const SUBMITTER: Address = Address::repeat_byte(0x33);
const APP: Address = Address::repeat_byte(0x11);
const RECIPIENT: Address = Address::repeat_byte(0x44);
const STOP: u64 = 1240;

fn domain() -> Eip712Domain {
    sequencer_core::build_input_domain(1, APP)
}

fn config() -> WalletConfig {
    WalletConfig {
        sequencer_address: SUBMITTER,
        ..WalletConfig::default()
    }
}

fn deposit(block: u64, recipient: Address, amount: u64) -> IngestedSafeInput {
    let mut payload = Vec::new();
    payload.extend_from_slice(config().supported_erc20_token.as_slice());
    payload.extend_from_slice(recipient.as_slice());
    payload.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
    raw(config().erc20_portal_address, block, payload)
}

fn raw(sender: Address, block: u64, payload: Vec<u8>) -> IngestedSafeInput {
    IngestedSafeInput {
        sender,
        payload,
        block_number: block,
        block_timestamp: 1_700_000_000 + block * 12,
        transaction_hash: B256::repeat_byte(u8::try_from(block % 251).unwrap()),
    }
}

fn transfer_batch(
    signer: &PrivateKeySigner,
    nonce: u32,
    block: u64,
    safe_block: u64,
    amount: u64,
) -> IngestedSafeInput {
    let op = UserOp {
        nonce,
        max_fee: 0,
        data: Method::Transfer(Transfer {
            amount: U256::from(amount),
            to: RECIPIENT,
        })
        .as_ssz_bytes()
        .into(),
    };
    let signature = signer
        .sign_hash_sync(&op.eip712_signing_hash(&domain()))
        .unwrap();
    raw(
        SUBMITTER,
        block,
        Batch {
            nonce: u64::from(nonce),
            frames: vec![Frame {
                safe_block,
                fee_price: 0,
                user_ops: vec![WireUserOp {
                    nonce,
                    max_fee: op.max_fee,
                    data: op.data.to_vec(),
                    signature: signature.as_bytes().to_vec(),
                }],
            }],
        }
        .as_ssz_bytes(),
    )
}

fn notices(outputs: AppOutputs) -> Vec<Vec<u8>> {
    outputs
        .into_iter()
        .map(|output| match output {
            AppOutput::Notice(bytes) => bytes,
            other => panic!("unexpected output in transfer-history fixture: {other:?}"),
        })
        .collect()
}

fn genesis_runner() -> CanonicalRunner<WalletApp> {
    CanonicalRunner::genesis(
        WalletApp::new(config()),
        SchedulerConfig::for_application::<WalletApp>(SUBMITTER),
    )
}

fn l1_input(index: u64, input: &IngestedSafeInput) -> L1Input {
    L1Input {
        index,
        sender: input.sender,
        block: input.block_number,
        timestamp: input.block_timestamp,
        chain_id: 1,
        app_contract: APP,
        payload: input.payload.clone(),
    }
}

/// Execute a transaction the fixture expects to run: a direct, or a user op at
/// its sender's next nonce.
fn execute_done(app: &mut WalletApp, tx: &L2Tx) -> (L2TxCount, AppOutputs) {
    match execute(app, tx).unwrap() {
        Executed::Done { index, outputs } => (index, outputs),
        Executed::Skipped(reason) => panic!("fixture transaction skipped: {reason}"),
    }
}

struct Server {
    addr: SocketAddr,
    shutdown: RuntimeScope,
    task: Option<http::ApiServerTask>,
    _rx: mpsc::Receiver<PendingUserOp>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.request_shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Server {
    async fn start(db_path: &str) -> Self {
        let (listeners, _public_addr, addr) =
            crate::test_support::server::bind_api_listeners().await;
        let shutdown = RuntimeScope::default();
        let (tx, rx) = mpsc::channel(1);
        let feed = L2TxFeed::new(
            db_path.to_owned(),
            shutdown.clone(),
            L2TxFeedConfig::default(),
        );
        let task = http::start_on_listeners(
            listeners,
            tx,
            shutdown.clone(),
            feed,
            ApiConfig::new(domain(), WalletApp::max_method_payload_bytes()),
            http::SnapshotState {
                db_path: db_path.to_owned(),
                state_file_in_dump: |prefix| {
                    WalletApp::state_file_in_dump(&dump_info::app_prefix(prefix))
                },
            },
        );
        Self {
            addr,
            shutdown,
            task: Some(task),
            _rx: rx,
        }
    }

    async fn stop(mut self) {
        self.shutdown.request_shutdown();
        tokio::time::timeout(Duration::from_secs(3), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_bootstrap_restores_transfer_history_and_hands_off_at_baseline() {
    let signer: PrivateKeySigner = format!("{:064x}", 1).parse().unwrap();
    let user = signer.address();
    // The direct at block 1231 lands after the malformed batch and is drained
    // by the next frame, whose safe block is its own: nothing stays queued.
    let inputs = vec![
        deposit(5, user, 100),
        deposit(12, user, 20),
        transfer_batch(&signer, 0, 20, 10, 7),
        deposit(20, user, 30),
        deposit(24, user, 40),
        raw(SUBMITTER, 1230, vec![0]),
        deposit(1231, user, 50),
        transfer_batch(&signer, 1, 1232, 1231, 11),
    ];

    let mut reference = genesis_runner();
    let mut expected_history = Vec::new();
    for (index, input) in (0_u64..).zip(&inputs) {
        let advanced = reference.process_input(l1_input(index, input)).unwrap();
        expected_history.extend(notices(advanced.outputs));
    }
    let reference_export = reference.export();
    assert_eq!(reference_export.l2_tx_count, 7);
    assert_eq!(reference_export.fridge_len, 0);
    let reference_nonce = reference_export.next_batch_nonce;
    assert_eq!(reference_nonce, 2);
    let (_, mut reference_app) = reference.into_parts();
    let deposit_notice = |amount| {
        DepositNotice {
            token: config().supported_erc20_token,
            sender: user,
            amount: U256::from(amount),
        }
        .abi_encode()
    };
    let transfer_notice = |amount| {
        TransferNotice {
            sender: user,
            recipient: RECIPIENT,
            amount: U256::from(amount),
        }
        .abi_encode()
    };
    assert_eq!(
        expected_history,
        vec![
            deposit_notice(100_u64),
            transfer_notice(7_u64),
            deposit_notice(20),
            deposit_notice(30),
            deposit_notice(40),
            deposit_notice(50),
            transfer_notice(11),
        ]
    );

    let db = temp_db("historical-projection-bootstrap");
    let dumps = tempfile::tempdir().unwrap();
    let baseline_dump = dumps.path().join("baseline");
    dump_info::create_dump_dir_with_info(
        &mut reference_app,
        &baseline_dump,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce: reference_nonce,
        },
    )
    .unwrap();
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    pin_test_deployment_identity(&mut storage, SUBMITTER);
    storage
        .append_ingested_safe_inputs_with_timestamp(
            STOP,
            1_700_000_000 + STOP * 12,
            &inputs,
            SUBMITTER,
            &default_protocol_timing(),
            FrontierMode::DeferUntilAnchorSet,
        )
        .unwrap();
    storage
        .publish_era(
            &baseline_dump,
            &crate::storage::EraRecord {
                base_count: reference_app.l2_tx_count(),
                rebuilt_history_end: reference_app.l2_tx_count(),
                stop_block: STOP,
                first_queued_input: reference_export.first_queued_input,
                next_batch_nonce: reference_nonce,
            },
        )
        .unwrap();
    // Setup opens no Tip; run's startup roots the batch tree at the anchor.
    storage.ensure_open_tip().unwrap();
    let server = Server::start(&db.path).await;
    let client = SequencerClient::new(format!("http://{}", server.addr)).unwrap();
    let metadata = client.history(None, None).await.unwrap();
    let era = metadata.history.version.era_id;
    assert_eq!(metadata.history.available_from.get(), 7);
    assert_eq!(metadata.history.head.get(), 7);
    assert!(matches!(
        client.subscribe(HistoryClaim {
            version: metadata.history.version,
            next_input: L2TxCount::new(6),
        }).await,
        Err(sequencer_rust_client::SubscribeError::History(HistoryPolicyError::HistoryUnavailable { available_from }))
            if available_from == metadata.history.available_from
    ));
    assert_eq!(metadata.baseline.l1_stop_block, STOP);
    assert_eq!(metadata.baseline.l1_end_input_index, 8);
    assert_eq!(metadata.baseline.next_batch_nonce, reference_nonce);
    // The era baseline is canonical at the stop block: the first checkpoint.
    assert_eq!(
        metadata.accepted_checkpoint.map(|checkpoint| (
            checkpoint.inclusion_block,
            checkpoint.executed_input_count.get(),
            checkpoint.next_batch_nonce
        )),
        Some((STOP, 7, reference_nonce))
    );
    assert_eq!(metadata.deployment.batch_submitter_address, SUBMITTER);
    assert_eq!(
        sequencer_core::build_input_domain(
            metadata.deployment.chain_id,
            metadata.deployment.app_address
        ),
        domain()
    );
    let mut other_era_bytes = *era.as_bytes();
    other_era_bytes[0] ^= 1;
    let other_era = sequencer_core::history::EraId::from_bytes(other_era_bytes).unwrap();
    assert!(matches!(
        client.history(Some(other_era), None).await,
        Err(HistoryReadError::History(HistoryPolicyError::EraChanged { current }))
            if current == metadata.history.version
    ));
    assert!(matches!(
        client
            .historical_l1_inputs(
                other_era,
                HistoricalL1InputStart::NextInputIndex(u64::MAX),
                None
            )
            .await,
        Err(HistoryReadError::History(
            HistoryPolicyError::EraChanged { .. }
        ))
    ));
    assert!(matches!(
        client
            .historical_l1_inputs(era, HistoricalL1InputStart::NextInputIndex(9), None)
            .await,
        Err(HistoryReadError::Http { status: 400, .. })
    ));

    // The reader replays from the era's first input: the wallet dump holds no
    // notices, and the application keeps no clock to resume a replay from.
    let mut replay = CanonicalRunner::genesis(
        WalletApp::new(config()),
        SchedulerConfig::for_application::<WalletApp>(metadata.deployment.batch_submitter_address),
    );
    let mut history = Vec::new();
    let mut seen = Vec::new();
    let mut start = HistoricalL1InputStart::NextInputIndex(0);
    loop {
        // One row per page crosses every page boundary.
        let page = client
            .historical_l1_inputs(era, start, Some(1))
            .await
            .unwrap();
        assert_eq!(page.era_id, era);
        assert_eq!(page.l1_stop_block, STOP);
        assert_eq!(page.end_input_index, 8);
        for item in page.items {
            let source = &inputs[usize::try_from(item.input_index).unwrap()];
            assert_eq!(item.payload.as_ref(), source.payload);
            assert_eq!(item.sender, source.sender);
            assert_eq!(item.block_timestamp, source.block_timestamp);
            assert_eq!(item.transaction_hash, source.transaction_hash);
            seen.push(item.input_index);
            let advanced = replay
                .process_input(L1Input {
                    index: item.input_index,
                    sender: item.sender,
                    block: item.block_number,
                    timestamp: item.block_timestamp,
                    chain_id: metadata.deployment.chain_id,
                    app_contract: metadata.deployment.app_address,
                    payload: item.payload.to_vec(),
                })
                .unwrap();
            if item.input_index == 5 {
                assert_eq!(
                    advanced.outcome,
                    InputOutcome::BatchRejected(BatchRejectReason::DecodeFailed)
                );
                assert_eq!(
                    advanced.outputs.len(),
                    3,
                    "malformed batch still drains overdue directs"
                );
                assert_eq!(replay.export().next_batch_nonce, 1);
            }
            history.extend(notices(advanced.outputs));
        }
        if page.next_input_index == page.end_input_index {
            break;
        }
        start = HistoricalL1InputStart::NextInputIndex(page.next_input_index);
    }
    assert_eq!(seen, (0..8).collect::<Vec<u64>>());
    let replay_export = replay.export();
    assert_eq!(replay_export, reference_export);
    assert_eq!(
        replay_export.next_batch_nonce,
        metadata.baseline.next_batch_nonce
    );
    assert_eq!(
        replay_export.l2_tx_count,
        metadata.history.available_from.get()
    );
    let (_, mut reader_app) = replay.into_parts();
    assert_eq!(
        reader_app.canonical_snapshot_bytes().unwrap(),
        reference_app.canonical_snapshot_bytes().unwrap()
    );
    assert_eq!(history, expected_history);
    let eof = client
        .historical_l1_inputs(era, HistoricalL1InputStart::NextInputIndex(8), None)
        .await
        .unwrap();
    assert!(eof.items.is_empty());
    assert_eq!(eof.next_input_index, eof.end_input_index);

    let current = client.history(Some(era), None).await.unwrap();
    assert_eq!(current.baseline, metadata.baseline);
    let mut stream = client
        .subscribe(HistoryClaim {
            version: current.history.version,
            next_input: reader_app.l2_tx_count(),
        })
        .await
        .unwrap();
    let live = deposit(1245, user, 60);
    storage
        .append_ingested_safe_inputs_with_timestamp(
            1245,
            live.block_timestamp,
            std::slice::from_ref(&live),
            SUBMITTER,
            &default_protocol_timing(),
            FrontierMode::Populate,
        )
        .unwrap();
    let mut head = storage.open_state().unwrap().unwrap();
    let live_direct = L2Tx::Direct(Direct {
        input_index: 8,
        sender: live.sender,
        block: live.block_number,
        timestamp: live.block_timestamp,
        payload: live.payload.clone(),
    });
    let (live_offset, outputs) = execute_done(&mut reference_app, &live_direct);
    expected_history.extend(notices(outputs));
    storage
        .close_frame_only_with_executions(
            &mut head,
            1245,
            SafeInputRange::new(8, 9),
            WalletApp::scheduler_limits().max_direct_payload_bytes,
            &[DirectInputExecution {
                safe_input_index: 8,
                l2_tx_index: live_offset,
            }],
        )
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(text) = message else {
        panic!("expected application input")
    };
    let message: WsTxMessage = serde_json::from_str(text.as_str()).unwrap();
    let WsTxMessage::DirectInput {
        offset,
        sender,
        block_number,
        payload,
        input_index,
        batch_nonce,
        block_timestamp,
        ..
    } = message
    else {
        panic!("expected post-baseline deposit")
    };
    assert_eq!(offset, 7, "the baseline's history is not delivered again");
    assert_eq!(batch_nonce, 2);
    let received = L2Tx::Direct(Direct {
        input_index,
        sender: sender.parse().unwrap(),
        block: block_number,
        timestamp: block_timestamp,
        payload: alloy_primitives::hex::decode(payload).unwrap(),
    });
    assert_eq!(
        received, live_direct,
        "the feed carries the direct's index and timestamp"
    );
    let (offset, outputs) = execute_done(&mut reader_app, &received);
    assert_eq!(offset, L2TxCount::new(7));
    history.extend(notices(outputs));
    assert_eq!(history, expected_history);
    assert_eq!(
        reader_app.canonical_snapshot_bytes().unwrap(),
        reference_app.canonical_snapshot_bytes().unwrap()
    );
    let fixed = client
        .historical_l1_inputs(era, HistoricalL1InputStart::AfterBlock(STOP), None)
        .await
        .unwrap();
    assert!(
        fixed.items.is_empty(),
        "new L1 inputs never extend this era's historical prefix"
    );
    assert_eq!(fixed.end_input_index, 8);
    drop(stream);
    server.stop().await;
}
