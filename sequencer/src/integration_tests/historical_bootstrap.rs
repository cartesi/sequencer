// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! A reader restores its own transfer history, replays raw L1 through the
//! canonical scheduler, and crosses the rebuilt baseline into the live feed.

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
    AppOutput, AppOutputs, Application, CanonicalState, execute_direct_input,
};
use sequencer_core::batch::{Batch, Frame, WireUserOp};
use sequencer_core::history::{ExecutedInputCount, HistoryClaim, HistoryPolicyError};
use sequencer_core::history_api::{AcceptedCheckpoint, HistoricalL1InputStart};
use sequencer_core::l2_tx::DirectInput;
use sequencer_core::scheduler::{
    BatchRejectReason, ProcessOutcome, Scheduler, SchedulerConfig, SchedulerInput,
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
use crate::storage::test_helpers::{default_protocol_timing, pin_test_deployment_identity};
use crate::storage::{
    DirectInputExecution, FrontierMode, IngestedSafeInput, LifecycleCommand, SafeInputRange,
    Storage,
};

use super::common::temp_db;

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

fn execute(
    scheduler: &mut Scheduler<WalletApp>,
    input: &IngestedSafeInput,
) -> sequencer_core::scheduler::ProcessResult {
    scheduler
        .process_input(SchedulerInput {
            sender: input.sender,
            inclusion_block: input.block_number,
            domain: domain(),
            payload: input.payload.clone(),
        })
        .unwrap()
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = RuntimeScope::default();
        let (tx, rx) = mpsc::channel(1);
        let feed = L2TxFeed::new(
            db_path.to_owned(),
            shutdown.clone(),
            L2TxFeedConfig::default(),
        );
        let task = http::start_on_listener(
            listener,
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
    let inputs = vec![
        deposit(5, user, 100),
        deposit(12, user, 20),
        transfer_batch(&signer, 0, 20, 10, 7),
        deposit(20, user, 30),
        deposit(24, user, 40),
        raw(SUBMITTER, 1230, vec![0]),
        transfer_batch(&signer, 1, 1232, 1228, 11),
        deposit(1235, user, 50),
    ];

    let mut reference = Scheduler::new(WalletApp::new(config()), SchedulerConfig::new(SUBMITTER));
    let mut expected_history = Vec::new();
    for input in &inputs {
        expected_history.extend(notices(execute(&mut reference, input).outputs));
    }
    expected_history.extend(notices(reference.drain_covered_at(STOP).unwrap()));
    let (mut reference_app, reference_nonce) = reference.finish();
    assert_eq!(reference_app.executed_input_count().get(), 7);
    assert_eq!(reference_app.last_executed_safe_block(), 1235);
    assert_eq!(reference_nonce, 2);
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
            transfer_notice(11),
            deposit_notice(50)
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
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
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
        .complete_baseline_setup(
            &baseline_dump,
            reference_app.executed_input_count(),
            STOP,
            reference_nonce,
            true,
        )
        .unwrap();
    let server = Server::start(&db.path).await;
    let client = SequencerClient::new(format!("http://{}", server.addr)).unwrap();
    let metadata = client.history(None, None).await.unwrap();
    let era = metadata.history.version.era_id;
    assert_eq!(metadata.history.available_from.get(), 7);
    assert_eq!(metadata.history.head.get(), 7);
    assert_eq!(metadata.baseline.l1_stop_block, STOP);
    assert_eq!(metadata.baseline.l1_end_input_index, 8);
    assert_eq!(metadata.baseline.next_batch_nonce, reference_nonce);
    assert_eq!(metadata.accepted_checkpoint, None);
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

    // The reader's saved projection includes notices that the core wallet dump
    // cannot reconstruct. Prepare a trusted end-of-block B=20 checkpoint.
    let mut checkpoint_scheduler =
        Scheduler::new(WalletApp::new(config()), SchedulerConfig::new(SUBMITTER));
    let mut saved_history = Vec::new();
    for input in &inputs[..4] {
        saved_history.extend(notices(execute(&mut checkpoint_scheduler, input).outputs));
    }
    assert_eq!(checkpoint_scheduler.queued_direct_len(), 2);
    let (mut checkpoint_app, next_batch_nonce) = checkpoint_scheduler.finish();
    let receipt = AcceptedCheckpoint {
        inclusion_block: 20,
        executed_input_count: checkpoint_app.executed_input_count(),
        next_batch_nonce,
    };
    assert_eq!(receipt.executed_input_count.get(), 2);
    assert_eq!(checkpoint_app.last_executed_safe_block(), 10);
    let backup = dumps.path().join("reader-checkpoint");
    std::fs::create_dir(&backup).unwrap();
    checkpoint_app.create_dump(&backup.join("wallet")).unwrap();
    std::fs::write(
        backup.join("projection.json"),
        serde_json::to_vec(&(receipt, &saved_history)).unwrap(),
    )
    .unwrap();
    drop(checkpoint_app);
    drop(saved_history);

    let restored_app = WalletApp::from_dump(&backup.join("wallet")).unwrap();
    let (receipt, mut history): (AcceptedCheckpoint, Vec<Vec<u8>>) =
        serde_json::from_slice(&std::fs::read(backup.join("projection.json")).unwrap()).unwrap();
    assert_eq!(
        restored_app.executed_input_count(),
        receipt.executed_input_count
    );
    let mut start = HistoricalL1InputStart::AfterBlock(restored_app.last_executed_safe_block());
    let mut replay = Scheduler::resume_at(
        restored_app,
        SchedulerConfig::new(SUBMITTER),
        receipt.next_batch_nonce,
    );
    std::fs::remove_dir_all(backup).unwrap();

    let mut seen = Vec::new();
    loop {
        // One row per page forces a page split between the batch at B and the
        // direct arriving later in that same block.
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
            if item.block_number <= receipt.inclusion_block {
                if item.sender != metadata.deployment.batch_submitter_address {
                    replay.enqueue_direct(item.sender, item.block_number, item.payload.to_vec());
                }
            } else {
                let result = replay
                    .process_input(SchedulerInput {
                        sender: item.sender,
                        inclusion_block: item.block_number,
                        domain: domain(),
                        payload: item.payload.to_vec(),
                    })
                    .unwrap();
                if item.input_index == 5 {
                    assert_eq!(
                        result.outcome,
                        ProcessOutcome::BatchRejected(BatchRejectReason::DecodeFailed)
                    );
                    assert_eq!(
                        result.outputs.len(),
                        3,
                        "malformed batch still drains overdue directs"
                    );
                    assert_eq!(replay.next_expected_batch_nonce(), 1);
                }
                history.extend(notices(result.outputs));
            }
        }
        if page.next_input_index == page.end_input_index {
            break;
        }
        start = HistoricalL1InputStart::NextInputIndex(page.next_input_index);
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(
        history.len(),
        6,
        "the young final direct waits until raw EOF"
    );
    assert_eq!(replay.queued_direct_len(), 1);
    history.extend(notices(
        replay
            .drain_covered_at(metadata.baseline.l1_stop_block)
            .unwrap(),
    ));
    let (mut reader_app, nonce) = replay.finish();
    assert_eq!(nonce, metadata.baseline.next_batch_nonce);
    assert_eq!(
        reader_app.executed_input_count(),
        metadata.history.available_from
    );
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
            next_input: reader_app.executed_input_count(),
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
    let execution = execute_direct_input(
        &mut reference_app,
        &DirectInput {
            sender: live.sender,
            block_number: live.block_number,
            payload: live.payload.clone(),
        },
    )
    .unwrap();
    expected_history.extend(notices(execution.outputs));
    storage
        .close_frame_only_with_executions(
            &mut head,
            1245,
            SafeInputRange::new(8, 9),
            &[DirectInputExecution {
                safe_input_index: 8,
                executed_input_offset: execution.offset,
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
        ..
    } = message
    else {
        panic!("expected post-baseline deposit")
    };
    assert_eq!(offset, 7, "terminal-drained D4 is not delivered again");
    assert_eq!(input_index, 8);
    assert_eq!(batch_nonce, 2);
    let execution = execute_direct_input(
        &mut reader_app,
        &DirectInput {
            sender: sender.parse().unwrap(),
            block_number,
            payload: alloy_primitives::hex::decode(payload).unwrap(),
        },
    )
    .unwrap();
    assert_eq!(execution.offset, ExecutedInputCount::new(7));
    history.extend(notices(execution.outputs));
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
