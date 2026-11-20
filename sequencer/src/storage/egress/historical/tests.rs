// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::Path;

use alloy_primitives::{Address, B256};
use sequencer_core::history::L2TxCount;

use super::*;
use crate::storage::history::insert_era_in;
use crate::storage::test_helpers::{SENDER_A, SENDER_B, era as era_record, local_batch_payload};
use crate::storage::{IngestedSafeInput, LifecycleCommand, NewEra, SafeInputRange};
use crate::test_support::{TestDb, pin_test_deployment_identity, temp_db};

fn fixture(name: &str, stop: u64, count: u64, nonce: u64) -> (TestDb, Storage) {
    let db = temp_db(name);
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    pin_test_deployment_identity(&mut storage, SENDER_A);
    let record = NewEra {
        anchor_batch_nonce: nonce,
        ..era_record(count, count, stop)
    };
    storage.write(|tx| insert_era_in(tx, &record)).unwrap();
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
        .append_ingested_safe_inputs_with_timestamp(safe_block, safe_block * 12, inputs)
        .unwrap();
}

fn era(storage: &mut Storage) -> EraId {
    storage.era().unwrap().id
}

#[test]
fn genesis_metadata_is_unmoved_by_live_inputs() {
    let (_db, mut storage) = fixture("historical-genesis", 0, 0, 0);
    append(&mut storage, 20, &[input(10, SENDER_B, vec![1])]);
    let info = storage.history_info(None).unwrap();
    assert_eq!(info.history.available_from, L2TxCount::ZERO);
    assert_eq!(info.history.head, L2TxCount::ZERO);
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
fn a_rebuilt_eras_baseline_survives_new_inputs() {
    let (_db, mut storage) = fixture("historical-rebuilt", 20, 41, 7);
    let inputs = [
        input(5, SENDER_B, vec![0xaa, 0xbb]),
        input(12, SENDER_A, vec![0xff]), // Deliberately malformed batch bytes.
        input(20, SENDER_B, vec![]),
        input(21, SENDER_B, vec![0xcc]),
    ];
    append(&mut storage, 25, &inputs);
    let info = storage.history_info(None).unwrap();
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
    let updated = storage.history_info(Some(info.history.era_id)).unwrap();
    assert_eq!(updated.baseline, info.baseline);
}

#[test]
fn metadata_refuses_another_era() {
    let (_db, mut storage) = fixture("historical-claim", 0, 0, 0);
    let current = era(&mut storage);
    let other: EraId = "00112233-4455-4677-8899-aabbccddeeff".parse().unwrap();
    assert_ne!(other, current);
    assert!(matches!(
        storage.history_info(Some(other)),
        Err(HistoricalReadError::Policy(HistoryPolicyError::EraChanged { era_id }))
            if era_id == current
    ));
    assert_eq!(
        storage.history_info(Some(current)).unwrap().history.era_id,
        current
    );
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
        )
        .unwrap();
    storage
        .collect_snapshots(&crate::storage::SnapshotLeases::default())
        .unwrap();
    let info = storage.history_info(None).unwrap();
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
        .execute("DELETE FROM snapshots WHERE batch_nonce=8", [])
        .unwrap();
    assert!(matches!(
        storage.history_info(None),
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
        storage.history_info(None),
        Err(HistoricalReadError::Checkpoint(
            FinalizedSelectionError::CanonicalDivergence
        ))
    ));
}

#[test]
#[should_panic(expected = "history has no accepted snapshot")]
fn missing_baseline_is_not_reported_as_an_absent_accepted_checkpoint() {
    let (_db, mut storage) = fixture("historical-missing-baseline", 20, 41, 7);
    storage.conn.execute("DELETE FROM snapshots", []).unwrap();
    let _ = storage.history_info(None);
}
