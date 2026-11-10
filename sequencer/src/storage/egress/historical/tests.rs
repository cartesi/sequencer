// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::Path;

use alloy_primitives::{Address, B256};

use super::*;
use crate::storage::history::{advance_recovery_generation_in, initialize_history_in};
use crate::storage::test_helpers::{
    SENDER_A, SENDER_B, TEST_MAX_DIRECT_PAYLOAD_BYTES, default_protocol_timing, era as era_record,
    local_batch_payload,
};
use crate::storage::{
    DirectInputExecution, EraRecord, FrontierMode, IngestedSafeInput, LifecycleCommand,
    SafeInputRange,
};
use crate::test_support::{TestDb, pin_test_deployment_identity, temp_db};

fn fixture(name: &str, stop: u64, count: u64, nonce: u64) -> (TestDb, Storage) {
    let db = temp_db(name);
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    pin_test_deployment_identity(&mut storage, SENDER_A);
    let record = EraRecord {
        next_batch_nonce: nonce,
        ..era_record(count, count, stop)
    };
    storage
        .write(|tx| initialize_history_in(tx, &record))
        .unwrap();
    storage
        .insert_baseline_snapshot(Path::new("/snapshot/baseline"), L2TxCount::new(count))
        .unwrap();
    (db, storage)
}

fn input(block: u64, sender: Address, payload: Vec<u8>) -> IngestedSafeInput {
    IngestedSafeInput {
        sender,
        payload,
        block_number: block,
        block_timestamp: block * 12,
        transaction_hash: B256::repeat_byte(block as u8),
    }
}

fn append(storage: &mut Storage, safe_block: u64, inputs: &[IngestedSafeInput]) {
    storage
        .append_ingested_safe_inputs_with_timestamp(
            safe_block,
            safe_block * 12,
            inputs,
            SENDER_A,
            &default_protocol_timing(),
            FrontierMode::DeferUntilAnchorSet,
        )
        .unwrap();
}

fn era(storage: &Storage) -> EraId {
    storage.history_state().unwrap().version.era_id
}

#[test]
fn genesis_metadata_is_unmoved_by_live_inputs() {
    let (_db, mut storage) = fixture("historical-genesis", 0, 0, 0);
    append(&mut storage, 20, &[input(10, SENDER_B, vec![1])]);
    let info = storage.history_info(None, None).unwrap();
    assert_eq!(info.history.available_from, L2TxCount::ZERO);
    assert_eq!(info.history.head, L2TxCount::ZERO);
    assert_eq!(info.compatibility, None);
    assert_eq!(
        info.baseline,
        HistoryBaseline {
            l1_stop_block: 0,
            l1_end_input_index: 0,
            next_batch_nonce: 0,
            rebuilt_history_end: L2TxCount::ZERO,
        }
    );
    assert_eq!(
        info.accepted_checkpoint,
        Some(AcceptedCheckpoint {
            inclusion_block: 0,
            executed_input_count: L2TxCount::ZERO,
            next_batch_nonce: 0,
        })
    );
    assert_eq!(info.deployment.chain_id, 1);
    assert_eq!(info.deployment.app_address, Address::repeat_byte(0x11));
    assert_eq!(
        info.deployment.input_box_address,
        Address::repeat_byte(0x22)
    );
    assert_eq!(info.deployment.app_deployment_block, 0);
    assert_eq!(info.deployment.batch_submitter_address, SENDER_A);
}

#[test]
fn a_rebuilt_eras_baseline_survives_new_inputs_and_a_generation_change() {
    let (_db, mut storage) = fixture("historical-rebuilt", 20, 41, 7);
    let inputs = [
        input(5, SENDER_B, vec![0xaa, 0xbb]),
        input(12, SENDER_A, vec![0xff]), // Deliberately malformed batch bytes.
        input(20, SENDER_B, vec![]),
        input(21, SENDER_B, vec![0xcc]),
    ];
    append(&mut storage, 25, &inputs);
    let info = storage.history_info(None, None).unwrap();
    assert_eq!(info.history.available_from, L2TxCount::new(41));
    assert_eq!(info.history.head, L2TxCount::new(41));
    assert_eq!(info.baseline.l1_stop_block, 20);
    assert_eq!(info.baseline.l1_end_input_index, 3);
    assert_eq!(info.baseline.next_batch_nonce, 7);
    assert_eq!(info.baseline.rebuilt_history_end, L2TxCount::new(41));
    // The baseline is canonical at the stop block: the first comparison checkpoint.
    assert_eq!(
        info.accepted_checkpoint,
        Some(AcceptedCheckpoint {
            inclusion_block: 20,
            executed_input_count: L2TxCount::new(41),
            next_batch_nonce: 7,
        })
    );

    append(&mut storage, 40, &[input(35, SENDER_B, vec![9])]);
    storage.write(advance_recovery_generation_in).unwrap();
    let updated = storage
        .history_info(Some(info.history.version.era_id), None)
        .unwrap();
    assert_eq!(updated.baseline, info.baseline);
    assert_eq!(updated.history.version.recovery_generation.get(), 1);
}

#[test]
fn metadata_refuses_another_era() {
    let (_db, mut storage) = fixture("historical-claim", 0, 0, 0);
    let current = storage.history_state().unwrap().version;
    let other: EraId = "00112233-4455-4677-8899-aabbccddeeff".parse().unwrap();
    assert_ne!(other, current.era_id);
    assert!(matches!(
        storage.history_info(Some(other), None),
        Err(HistoricalReadError::Policy(HistoryPolicyError::EraChanged { current: actual }))
            if actual == current
    ));
}

#[test]
fn accepted_checkpoint_uses_the_exact_latest_snapshot_and_preserves_baseline() {
    let (_db, mut storage) = fixture("historical-accepted", 20, 41, 7);
    let mut head = storage
        .initialize_open_state(20, SafeInputRange::empty_at(0))
        .unwrap();
    for index in 0..2 {
        storage
            .close_frame_and_batch_with_snapshot(
                &mut head,
                20,
                Path::new(&format!("/snapshot/{index}")),
                L2TxCount::new(41),
            )
            .unwrap();
    }
    let payloads = [
        local_batch_payload(&mut storage, 7),
        local_batch_payload(&mut storage, 8),
    ];
    storage
        .append_ingested_safe_inputs_with_timestamp(
            30,
            360,
            &[
                input(29, SENDER_A, payloads[0].clone()),
                input(30, SENDER_A, payloads[1].clone()),
            ],
            SENDER_A,
            &default_protocol_timing(),
            FrontierMode::Populate,
        )
        .unwrap();
    storage.gc_unreferenced_dumps().unwrap();
    let info = storage.history_info(None, None).unwrap();
    assert_eq!(info.baseline.next_batch_nonce, 7);
    assert_eq!(info.baseline.l1_stop_block, 20);
    assert_eq!(info.baseline.l1_end_input_index, 0);
    assert_eq!(
        info.accepted_checkpoint,
        Some(AcceptedCheckpoint {
            inclusion_block: 30,
            executed_input_count: L2TxCount::new(41),
            next_batch_nonce: 9,
        })
    );
    storage
        .conn
        .execute("DELETE FROM snapshots WHERE batch_index=1", [])
        .unwrap();
    assert!(matches!(
        storage.history_info(None, None),
        Err(HistoricalReadError::Checkpoint(
            FinalizedSelectionError::Storage(rusqlite::Error::QueryReturnedNoRows)
        ))
    ));
}

#[test]
fn canonical_divergence_cannot_be_advertised_as_an_accepted_receipt() {
    let (_db, mut storage) = fixture("historical-divergence", 0, 0, 0);
    storage
        .conn
        .execute(
            "INSERT INTO canonical_divergence \
         (singleton_id,nonce,safe_input_index,kind,detected_at_ms) VALUES (0,0,0,'foreign',0)",
            [],
        )
        .unwrap();
    assert!(matches!(
        storage.history_info(None, None),
        Err(HistoricalReadError::Checkpoint(
            FinalizedSelectionError::CanonicalDivergence
        ))
    ));
}

#[test]
#[should_panic(expected = "history has no rollback-safe snapshot")]
fn missing_baseline_is_not_reported_as_an_absent_accepted_checkpoint() {
    let (_db, mut storage) = fixture("historical-missing-baseline", 20, 41, 7);
    storage.conn.execute("DELETE FROM snapshots", []).unwrap();
    let _ = storage.history_info(None, None);
}

#[test]
fn compatibility_requires_an_era_and_rejects_future_generations_after_era_validation() {
    let (_db, mut storage) = fixture("historical-generation-query", 0, 0, 0);
    let current = era(&storage);
    assert!(matches!(
        storage.history_info(None, Some(RecoveryGeneration::new(0))),
        Err(HistoricalReadError::BadRequest(_))
    ));
    for from in [1, u64::MAX] {
        assert!(matches!(
            storage.history_info(Some(current), Some(RecoveryGeneration::new(from))),
            Err(HistoricalReadError::BadRequest(_))
        ));
    }
    let other = "00112233-4455-4677-8899-aabbccddeeff".parse().unwrap();
    assert!(matches!(
        storage.history_info(Some(other), Some(RecoveryGeneration::new(u64::MAX))),
        Err(HistoricalReadError::Policy(
            HistoryPolicyError::EraChanged { .. }
        ))
    ));
    let info = storage
        .history_info(Some(current), Some(RecoveryGeneration::new(0)))
        .unwrap();
    assert_eq!(
        info.compatibility,
        Some(HistoryCompatibility {
            from_generation: RecoveryGeneration::new(0),
            preserved_input_count: L2TxCount::ZERO,
        })
    );
}

#[test]
fn full_recovery_preserves_nonzero_baseline_before_replacement_directs() {
    let (_db, mut storage) = fixture("historical-generation-baseline", 100, 41, 7);
    let mut head = storage
        .initialize_open_state(100, SafeInputRange::empty_at(0))
        .unwrap();
    let now = crate::clock::unix_now_ms();
    let protocol = default_protocol_timing();
    storage
        .append_ingested_safe_inputs_with_timestamp(
            1400,
            now / 1000,
            &[input(110, SENDER_B, vec![1])],
            SENDER_A,
            &protocol,
            FrontierMode::Populate,
        )
        .unwrap();
    storage
        .close_frame_only_with_executions(
            &mut head,
            110,
            SafeInputRange::new(0, 1),
            TEST_MAX_DIRECT_PAYLOAD_BYTES,
            &[DirectInputExecution {
                safe_input_index: 0,
                l2_tx_index: L2TxCount::new(41),
            }],
        )
        .unwrap();
    let current = era(&storage);
    let before = storage
        .history_info(Some(current), Some(RecoveryGeneration::new(0)))
        .unwrap();
    assert_eq!(before.history.head, L2TxCount::new(42));
    assert_eq!(
        before.compatibility.unwrap().preserved_input_count,
        before.history.head
    );
    assert_eq!(
        storage
            .recover_aging_tip_for_recovery(
                head.batch_index,
                &protocol,
                now,
                TEST_MAX_DIRECT_PAYLOAD_BYTES,
            )
            .unwrap(),
        [0]
    );

    let after = storage
        .history_info(Some(current), Some(RecoveryGeneration::new(0)))
        .unwrap();
    assert_eq!(after.history.available_from, L2TxCount::new(41));
    assert_eq!(after.history.head, L2TxCount::new(42));
    assert_eq!(after.history.version.recovery_generation.get(), 1);
    assert_eq!(
        after.compatibility.unwrap().preserved_input_count,
        L2TxCount::new(41)
    );
    let latest = storage
        .history_info(Some(current), Some(RecoveryGeneration::new(1)))
        .unwrap();
    assert_eq!(
        latest.compatibility.unwrap().preserved_input_count,
        latest.history.head
    );
}
