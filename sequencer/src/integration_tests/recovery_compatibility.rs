// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! A reader's saved checkpoints across standard recoveries: which survive
//! the generations it missed, and a recovery racing its lookup.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use alloy::signers::{SignerSync, local::PrivateKeySigner};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use app_core::application::{Method, Transfer, WalletApp, WalletConfig};
use futures_util::StreamExt;
use sequencer_core::api::WsTxMessage;
use sequencer_core::application::{
    AppOutput, AppOutputs, Application, CanonicalState, Executed, check_live_user_op, execute,
};
use sequencer_core::history::{HistoryClaim, HistoryVersion, L2TxCount, RecoveryGeneration};
use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
use sequencer_core::user_op::{SignedUserOp, UserOp};
use sequencer_rust_client::{
    HistoryPolicyError, HistoryReadError, SequencerClient, SubscribeError, SubscribeStream,
};
use ssz::Encode;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use crate::http::{self, ApiConfig};
use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp, dump_info};
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::test_helpers::{default_protocol_timing, local_batch_payload};
use crate::storage::{
    DirectInputExecution, IngestedSafeInput, LifecycleCommand, SafeInputRange, Storage, WriteHead,
};
use crate::test_support::{pin_test_deployment_identity, temp_db};

const SUBMITTER: Address = Address::repeat_byte(0x33);
const APP: Address = Address::repeat_byte(0x11);
const RECIPIENT: Address = Address::repeat_byte(0x44);

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

fn notices(outputs: AppOutputs) -> Vec<Vec<u8>> {
    outputs
        .into_iter()
        .map(|output| match output {
            AppOutput::Notice(bytes) => bytes,
            other => panic!("unexpected output in transfer-history fixture: {other:?}"),
        })
        .collect()
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

struct Backup {
    claim: HistoryClaim,
    path: PathBuf,
}

impl Backup {
    fn save(
        directory: &Path,
        name: &str,
        app: &mut WalletApp,
        history: &[Vec<u8>],
        version: HistoryVersion,
    ) -> Self {
        let claim = HistoryClaim {
            version,
            next_input: app.l2_tx_count(),
        };
        let path = directory.join(name);
        std::fs::create_dir(&path).unwrap();
        app.create_dump(&path.join("wallet")).unwrap();
        std::fs::write(
            path.join("projection.json"),
            serde_json::to_vec(&(claim, history)).unwrap(),
        )
        .unwrap();
        Self { claim, path }
    }

    fn restore(&self) -> (WalletApp, Vec<Vec<u8>>) {
        let app = WalletApp::from_dump(&self.path.join("wallet")).unwrap();
        let (claim, history): (HistoryClaim, Vec<Vec<u8>>) =
            serde_json::from_slice(&std::fs::read(self.path.join("projection.json")).unwrap())
                .unwrap();
        assert_eq!(claim, self.claim);
        assert_eq!(app.l2_tx_count(), claim.next_input);
        (app, history)
    }
}

fn append_transfer(
    storage: &mut Storage,
    head: &mut WriteHead,
    app: &mut WalletApp,
    history: &mut Vec<Vec<u8>>,
    signer: &PrivateKeySigner,
    amount: u64,
) {
    let op = UserOp {
        nonce: app.current_user_nonce(signer.address()),
        max_fee: head.frame_fee,
        data: Method::Transfer(Transfer {
            to: RECIPIENT,
            amount: U256::from(amount),
        })
        .as_ssz_bytes()
        .into(),
    };
    let refusal = check_live_user_op(app, signer.address(), &op, head.frame_fee).unwrap();
    assert_eq!(refusal, None, "fixture transfer refused");
    let tx = UserOpTx::new(signer.address(), &op, head.frame_fee, head.safe_block);
    let (offset, outputs) = execute_done(app, &L2Tx::UserOp(tx));
    history.extend(notices(outputs));
    let (respond_to, _response) = oneshot::channel();
    let included = IncludedUserOp {
        pending: PendingUserOp {
            signed: SignedUserOp {
                sender: signer.address(),
                signature: signer
                    .sign_hash_sync(&op.eip712_signing_hash(&domain()))
                    .unwrap(),
                user_op: op,
            },
            respond_to,
            received_at: SystemTime::now(),
        },
        l2_tx_index: offset,
    };
    storage
        .append_executed_user_ops_chunk(head, &[included])
        .unwrap();
}

fn close_and_accept(
    storage: &mut Storage,
    head: &mut WriteHead,
    app: &mut WalletApp,
    dumps: &Path,
    inclusion_block: u64,
) {
    let index = head.batch_index;
    let nonce = storage.batch_nonce(index).unwrap();
    let prefix = dumps.join(format!("accepted-{index}"));
    dump_info::create_dump_dir_with_info(
        app,
        &prefix,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce: nonce + 1,
        },
    )
    .unwrap();
    storage
        .close_frame_and_batch_with_snapshot(head, head.safe_block, &prefix, app.l2_tx_count())
        .unwrap();
    let payload = local_batch_payload(storage, nonce);
    storage
        .append_safe_inputs(
            inclusion_block,
            &[crate::storage::StoredSafeInput {
                sender: SUBMITTER,
                block_number: inclusion_block,
                payload,
            }],
            SUBMITTER,
            &default_protocol_timing(),
        )
        .unwrap();
}

fn recover_tip(storage: &mut Storage, head: &mut WriteHead, safe_block: u64) {
    let protocol = default_protocol_timing();
    storage
        .append_safe_inputs(safe_block, &[], SUBMITTER, &protocol)
        .unwrap();
    let invalidated = storage
        .recover_aging_tip_for_recovery(
            head.batch_index,
            &protocol,
            crate::clock::unix_now_ms(),
            WalletApp::scheduler_limits().max_direct_payload_bytes,
        )
        .unwrap();
    assert_eq!(invalidated, vec![head.batch_index]);
    *head = storage.open_state().unwrap().unwrap();
}

async fn replay_one(stream: &mut SubscribeStream, app: &mut WalletApp, history: &mut Vec<Vec<u8>>) {
    let message = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(text) = message else {
        panic!("expected application input")
    };
    let message: WsTxMessage = serde_json::from_str(text.as_str()).unwrap();
    let WsTxMessage::UserOp {
        offset,
        sender,
        nonce,
        fee,
        data,
        safe_block,
        ..
    } = message
    else {
        panic!("expected transfer")
    };
    let tx = UserOpTx {
        sender: sender.parse().unwrap(),
        nonce,
        fee,
        block: safe_block,
        data: alloy_primitives::hex::decode(data).unwrap(),
    };
    let (executed_at, outputs) = execute_done(app, &L2Tx::UserOp(tx));
    assert_eq!(executed_at.get(), offset);
    history.extend(notices(outputs));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_compatibility_survives_missed_recoveries_and_retries_admission_races() {
    let db = temp_db("reader-checkpoint-compatibility");
    let dumps = tempfile::tempdir().unwrap();
    let signer: PrivateKeySigner = format!("{:064x}", 1).parse().unwrap();
    let mut app = WalletApp::new(config());
    let mut history = Vec::new();
    let baseline = dumps.path().join("genesis");
    dump_info::create_dump_dir_with_info(
        &mut app,
        &baseline,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce: 0,
        },
    )
    .unwrap();
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    pin_test_deployment_identity(&mut storage, SUBMITTER);
    storage
        .publish_era(&baseline, &crate::storage::EraRecord::EMPTY_GENESIS)
        .unwrap();
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    let directs = [
        deposit(5, signer.address(), 1_000_000_000_000_000),
        deposit(6, RECIPIENT, 100),
    ];
    storage
        .append_ingested_safe_inputs_with_timestamp(
            10,
            crate::clock::unix_now_ms() / 1000,
            &directs,
            &default_protocol_timing(),
        )
        .unwrap();
    let mut executions = Vec::new();
    for (input_index, direct) in (0_u64..).zip(directs) {
        let tx = L2Tx::Direct(Direct {
            input_index,
            sender: direct.sender,
            block: direct.block_number,
            timestamp: direct.block_timestamp,
            payload: direct.payload,
        });
        let (offset, outputs) = execute_done(&mut app, &tx);
        history.extend(notices(outputs));
        executions.push(DirectInputExecution {
            safe_input_index: input_index,
            l2_tx_index: offset,
        });
    }
    storage
        .close_frame_only_with_executions(
            &mut head,
            10,
            SafeInputRange::new(0, 2),
            WalletApp::scheduler_limits().max_direct_payload_bytes,
            &executions,
        )
        .unwrap();
    append_transfer(&mut storage, &mut head, &mut app, &mut history, &signer, 10);
    close_and_accept(&mut storage, &mut head, &mut app, dumps.path(), 20);
    let generation_zero = storage.history_state().unwrap().version;
    let good_zero = Backup::save(dumps.path(), "g0-at3", &mut app, &history, generation_zero);
    append_transfer(&mut storage, &mut head, &mut app, &mut history, &signer, 11);
    let bad_zero = Backup::save(dumps.path(), "g0-at4", &mut app, &history, generation_zero);

    let server = Server::start(&db.path).await;
    let client = SequencerClient::new(format!("http://{}", server.addr)).unwrap();
    let era = generation_zero.era_id;
    recover_tip(&mut storage, &mut head, 1500);
    (app, history) = good_zero.restore();
    let generation_one = storage.history_state().unwrap().version;
    assert_eq!(generation_one.recovery_generation.get(), 1);
    append_transfer(&mut storage, &mut head, &mut app, &mut history, &signer, 20);
    let middle_one = Backup::save(dumps.path(), "g1-at4", &mut app, &history, generation_one);
    append_transfer(&mut storage, &mut head, &mut app, &mut history, &signer, 21);
    close_and_accept(&mut storage, &mut head, &mut app, dumps.path(), 1501);
    let good_one = Backup::save(dumps.path(), "g1-at5", &mut app, &history, generation_one);
    append_transfer(&mut storage, &mut head, &mut app, &mut history, &signer, 22);
    let bad_one = Backup::save(dumps.path(), "g1-at6", &mut app, &history, generation_one);

    recover_tip(&mut storage, &mut head, 3000);
    (app, history) = good_one.restore();
    for amount in [30, 31, 32] {
        append_transfer(
            &mut storage,
            &mut head,
            &mut app,
            &mut history,
            &signer,
            amount,
        );
    }
    let current = client.history(Some(era), None).await.unwrap();
    assert_eq!(current.history.head.get(), 8);
    assert_eq!(current.history.version.recovery_generation.get(), 2);
    assert_eq!(current.compatibility, None);
    let same = client
        .history(Some(era), Some(RecoveryGeneration::new(2)))
        .await
        .unwrap();
    assert_eq!(same.compatibility.unwrap().preserved_input_count.get(), 8);
    let future = Some(RecoveryGeneration::new(u64::MAX));
    assert!(matches!(
        client.history(Some(era), future).await,
        Err(HistoryReadError::Http { status: 400, .. })
    ));
    let mut other_era_bytes = *era.as_bytes();
    other_era_bytes[0] ^= 1;
    let other_era = sequencer_core::history::EraId::from_bytes(other_era_bytes).unwrap();
    assert!(matches!(
        client.history(Some(other_era), future).await,
        Err(HistoryReadError::History(HistoryPolicyError::EraChanged { current: version }))
            if version == current.history.version
    ));

    // Every backup is evaluated under its own saved generation. Latest-cut-only
    // matching would incorrectly resurrect the discarded g0 transfer at offset 3.
    let mut selected = None;
    for (candidate, eligible, cut) in [
        (&good_zero, true, 3),
        (&bad_zero, false, 3),
        (&middle_one, true, 5),
        (&good_one, true, 5),
        (&bad_one, false, 5),
    ] {
        let info = client
            .history(Some(era), Some(candidate.claim.version.recovery_generation))
            .await
            .unwrap();
        let compatibility = info.compatibility.unwrap();
        assert_eq!(
            compatibility.from_generation,
            candidate.claim.version.recovery_generation
        );
        assert_eq!(compatibility.preserved_input_count.get(), cut);
        let survives = candidate.claim.next_input >= info.history.available_from
            && candidate.claim.next_input <= compatibility.preserved_input_count;
        assert_eq!(survives, eligible);
        if survives
            && selected.is_none_or(|previous: &Backup| {
                previous.claim.next_input < candidate.claim.next_input
            })
        {
            selected = Some(candidate);
        }
    }
    let selected = selected.unwrap();
    assert_eq!(selected.claim, good_one.claim);
    let (mut reader_app, mut reader_history) = selected.restore();
    let mut stream = client
        .subscribe(HistoryClaim {
            version: current.history.version,
            next_input: selected.claim.next_input,
        })
        .await
        .unwrap();
    for _ in 5..8 {
        replay_one(&mut stream, &mut reader_app, &mut reader_history).await;
    }
    assert_eq!(
        reader_app.canonical_snapshot_bytes().unwrap(),
        app.canonical_snapshot_bytes().unwrap()
    );
    assert_eq!(
        reader_history, history,
        "restored projection loses invalidated transfers and follows their replacements"
    );
    drop(stream);

    let lookup = client
        .history(Some(era), Some(good_one.claim.version.recovery_generation))
        .await
        .unwrap();
    assert_eq!(lookup.compatibility.unwrap().preserved_input_count.get(), 5);
    recover_tip(&mut storage, &mut head, 4500);
    let stale = client
        .subscribe(HistoryClaim {
            version: lookup.history.version,
            next_input: good_one.claim.next_input,
        })
        .await;
    assert!(
        matches!(stale, Err(SubscribeError::History(HistoryPolicyError::StaleGeneration { current })) if current.recovery_generation.get() == 3)
    );
    let fresh = client
        .history(Some(era), Some(good_one.claim.version.recovery_generation))
        .await
        .unwrap();
    assert_eq!(fresh.compatibility.unwrap().preserved_input_count.get(), 5);
    (reader_app, reader_history) = good_one.restore();
    (app, history) = good_one.restore();
    let mut stream = client
        .subscribe(HistoryClaim {
            version: fresh.history.version,
            next_input: good_one.claim.next_input,
        })
        .await
        .unwrap();
    append_transfer(&mut storage, &mut head, &mut app, &mut history, &signer, 40);
    replay_one(&mut stream, &mut reader_app, &mut reader_history).await;
    assert_eq!(
        reader_app.canonical_snapshot_bytes().unwrap(),
        app.canonical_snapshot_bytes().unwrap()
    );
    assert_eq!(reader_history, history);
    drop(stream);
    server.stop().await;
}
