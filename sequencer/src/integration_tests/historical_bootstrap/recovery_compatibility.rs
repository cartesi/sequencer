// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sequencer_core::application::{ExecutionOutcome, validate_and_execute_user_op};
use sequencer_core::history::{HistoryVersion, RecoveryGeneration};
use sequencer_core::user_op::SignedUserOp;
use sequencer_rust_client::{
    HistoryPolicyError, HistoryReadError, SubscribeError, SubscribeStream,
};
use tokio::sync::oneshot;

use super::*;
use crate::ingress::inclusion_lane::IncludedUserOp;
use crate::storage::WriteHead;
use crate::storage::test_helpers::local_batch_payload;

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
            next_input: app.executed_input_count(),
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
        assert_eq!(app.executed_input_count(), claim.next_input);
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
    let outcome =
        validate_and_execute_user_op(app, signer.address(), &op, head.frame_fee, head.safe_block)
            .unwrap();
    let ExecutionOutcome::Included(execution) = outcome else {
        panic!("fixture transfer rejected: {outcome:?}")
    };
    history.extend(notices(execution.outputs));
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
        executed_input_offset: execution.offset,
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
        .close_frame_and_batch_with_snapshot(
            head,
            head.safe_block,
            &prefix,
            index,
            app.executed_input_count(),
        )
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
        .recover_aging_tip_for_recovery(head.batch_index, &protocol, crate::clock::unix_now_ms())
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
    assert_eq!(offset, app.executed_input_count().get());
    let op = UserOp {
        nonce,
        max_fee: fee,
        data: alloy_primitives::hex::decode(data).unwrap().into(),
    };
    let outcome =
        validate_and_execute_user_op(app, sender.parse().unwrap(), &op, fee, safe_block).unwrap();
    let ExecutionOutcome::Included(execution) = outcome else {
        panic!("feed transfer rejected: {outcome:?}")
    };
    history.extend(notices(execution.outputs));
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
        .complete_baseline_setup(&baseline, ExecutedInputCount::ZERO, 0, 0, false)
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
            SUBMITTER,
            &default_protocol_timing(),
            FrontierMode::Populate,
        )
        .unwrap();
    let mut executions = Vec::new();
    for (index, direct) in directs.into_iter().enumerate() {
        let execution = execute_direct_input(
            &mut app,
            &DirectInput {
                sender: direct.sender,
                payload: direct.payload,
                block_number: direct.block_number,
            },
        )
        .unwrap();
        history.extend(notices(execution.outputs));
        executions.push(DirectInputExecution {
            safe_input_index: index as u64,
            executed_input_offset: execution.offset,
        });
    }
    storage
        .close_frame_only_with_executions(&mut head, 10, SafeInputRange::new(0, 2), &executions)
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
