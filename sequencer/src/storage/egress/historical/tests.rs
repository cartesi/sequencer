// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::Path;

use super::*;
use crate::storage::history::{advance_recovery_generation_in, initialize_history_in};
use crate::storage::test_helpers::{
    SENDER_A, SENDER_B, TestDb, default_protocol_timing, local_batch_payload,
    pin_test_deployment_identity, temp_db,
};
use crate::storage::{
    DirectInputExecution, FrontierMode, IngestedSafeInput, LifecycleCommand, SafeInputRange,
};

fn fixture(name: &str, stop: u64, count: u64, nonce: u64) -> (TestDb, Storage) {
    let db = temp_db(name);
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
    pin_test_deployment_identity(&mut storage, SENDER_A);
    storage
        .write(|tx| initialize_history_in(tx, ExecutedInputCount::new(count), stop))
        .unwrap();
    storage.set_batch_tree_anchor(nonce).unwrap();
    storage
        .insert_baseline_snapshot(
            Path::new("/snapshot/baseline"),
            ExecutedInputCount::new(count),
        )
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

fn page(storage: &mut Storage, next: u64, limit: usize) -> HistoricalL1InputsPage {
    storage
        .historical_l1_inputs(
            era(storage),
            HistoricalL1InputStart::NextInputIndex(next),
            limit,
        )
        .unwrap()
}

#[test]
fn genesis_history_has_an_empty_raw_prefix_even_after_live_inputs_arrive() {
    let (_db, mut storage) = fixture("historical-genesis", 0, 0, 0);
    append(&mut storage, 20, &[input(10, SENDER_B, vec![1])]);
    let info = storage.history_info(None, None).unwrap();
    assert_eq!(info.history.available_from, ExecutedInputCount::ZERO);
    assert_eq!(info.history.head, ExecutedInputCount::ZERO);
    assert_eq!(info.compatibility, None);
    assert_eq!(
        info.baseline,
        HistoryBaseline {
            l1_stop_block: 0,
            l1_end_input_index: 0,
            next_batch_nonce: 0
        }
    );
    assert_eq!(
        info.accepted_checkpoint,
        Some(AcceptedCheckpoint {
            inclusion_block: 0,
            executed_input_count: ExecutedInputCount::ZERO,
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
    let raw = page(&mut storage, 0, 10);
    assert!(raw.items.is_empty());
    assert_eq!((raw.end_input_index, raw.next_input_index), (0, 0));
}

#[test]
fn rebuilt_history_preserves_raw_payloads_metadata_and_its_fixed_prefix() {
    let (_db, mut storage) = fixture("historical-rebuilt", 20, 41, 7);
    let inputs = [
        input(5, SENDER_B, vec![0xaa, 0xbb]),
        input(12, SENDER_A, vec![0xff]), // Deliberately malformed batch bytes.
        input(20, SENDER_B, vec![]),
        input(21, SENDER_B, vec![0xcc]),
    ];
    append(&mut storage, 25, &inputs);
    let info = storage.history_info(None, None).unwrap();
    assert_eq!(info.history.available_from, ExecutedInputCount::new(41));
    assert_eq!(info.history.head, ExecutedInputCount::new(41));
    assert_eq!(info.baseline.l1_stop_block, 20);
    assert_eq!(info.baseline.l1_end_input_index, 3);
    assert_eq!(info.baseline.next_batch_nonce, 7);
    assert_eq!(info.accepted_checkpoint, None);

    let first = page(&mut storage, 0, 2);
    assert_eq!((first.end_input_index, first.next_input_index), (3, 2));
    for (index, actual) in first.items.iter().enumerate() {
        let expected = &inputs[index];
        assert_eq!(actual.input_index, index as u64);
        assert_eq!(actual.sender, expected.sender);
        assert_eq!(actual.payload.as_ref(), expected.payload);
        assert_eq!(actual.block_number, expected.block_number);
        assert_eq!(actual.block_timestamp, expected.block_timestamp);
        assert_eq!(actual.transaction_hash, expected.transaction_hash);
    }
    let last = page(&mut storage, first.next_input_index, 2);
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.items[0].input_index, 2);
    assert!(last.items[0].payload.is_empty());
    assert_eq!((last.end_input_index, last.next_input_index), (3, 3));
    assert!(page(&mut storage, 3, 2).items.is_empty());

    append(&mut storage, 40, &[input(35, SENDER_B, vec![9])]);
    storage.write(advance_recovery_generation_in).unwrap();
    assert_eq!(page(&mut storage, 0, 2), first);
    let updated = storage
        .history_info(Some(info.history.version.era_id), None)
        .unwrap();
    assert_eq!(updated.baseline, info.baseline);
    assert_eq!(updated.history.version.recovery_generation.get(), 1);
}

#[test]
fn after_block_skips_the_whole_block_and_paging_keeps_its_remaining_rows() {
    let (_db, mut storage) = fixture("historical-block-seek", 30, 9, 2);
    append(
        &mut storage,
        40,
        &[
            input(5, SENDER_B, vec![0]),
            input(12, SENDER_A, vec![1]),
            input(12, SENDER_B, vec![2]),
            input(20, SENDER_B, vec![3]),
            input(20, SENDER_A, vec![4]),
            input(31, SENDER_B, vec![5]),
        ],
    );
    let current = era(&storage);
    let first = storage
        .historical_l1_inputs(current, HistoricalL1InputStart::AfterBlock(12), 1)
        .unwrap();
    assert_eq!(first.items[0].input_index, 3);
    assert_eq!(first.next_input_index, 4);
    assert_eq!(page(&mut storage, 4, 1).items[0].input_index, 4);
    for block in [20, 25, 30] {
        let eof = storage
            .historical_l1_inputs(current, HistoricalL1InputStart::AfterBlock(block), 1)
            .unwrap();
        assert!(eof.items.is_empty());
        assert_eq!((eof.end_input_index, eof.next_input_index), (5, 5));
    }
}

#[test]
fn era_validation_precedes_numeric_bounds_even_for_empty_history() {
    let (_db, mut storage) = fixture("historical-claim", 0, 0, 0);
    let current = storage.history_state().unwrap().version;
    let other: EraId = "00112233-4455-4677-8899-aabbccddeeff".parse().unwrap();
    assert_ne!(other, current.era_id);
    for start in [
        HistoricalL1InputStart::NextInputIndex(0),
        HistoricalL1InputStart::NextInputIndex(u64::MAX),
        HistoricalL1InputStart::AfterBlock(u64::MAX),
    ] {
        for limit in [0, 1, usize::MAX] {
            assert!(matches!(
                storage.historical_l1_inputs(other, start, limit),
                Err(HistoricalReadError::Policy(HistoryPolicyError::EraChanged { current: actual }))
                    if actual == current
            ));
        }
    }
    assert!(matches!(
        storage.history_info(Some(other), None),
        Err(HistoricalReadError::Policy(HistoryPolicyError::EraChanged { current: actual }))
            if actual == current
    ));
    for (start, limit) in [
        (HistoricalL1InputStart::NextInputIndex(0), 0),
        (
            HistoricalL1InputStart::NextInputIndex(0),
            HISTORICAL_INPUT_MAX_ITEMS + 1,
        ),
        (HistoricalL1InputStart::NextInputIndex(1), 1),
        (HistoricalL1InputStart::NextInputIndex(u64::MAX), 1),
        (HistoricalL1InputStart::AfterBlock(1), 1),
        (HistoricalL1InputStart::AfterBlock(u64::MAX), 1),
    ] {
        assert!(matches!(
            storage.historical_l1_inputs(current.era_id, start, limit),
            Err(HistoricalReadError::BadRequest(_))
        ));
    }
}

#[test]
fn byte_budget_makes_progress_through_oversized_inputs_without_skipping_them() {
    let (_db, mut storage) = fixture("historical-byte-budget", 20, 1, 1);
    let target = HISTORICAL_INPUT_PAYLOAD_TARGET_BYTES;
    append(
        &mut storage,
        20,
        &[
            input(1, SENDER_B, vec![1; target / 2]),
            input(2, SENDER_B, vec![2; target / 2 + 1]),
            input(3, SENDER_A, vec![3; target + 1]),
            input(4, SENDER_B, vec![4]),
        ],
    );
    for (start, expected_len) in [
        (0, target / 2),
        (1, target / 2 + 1),
        (2, target + 1),
        (3, 1),
    ] {
        let raw = page(&mut storage, start, HISTORICAL_INPUT_MAX_ITEMS);
        assert_eq!(raw.items.len(), 1);
        assert_eq!(raw.items[0].input_index, start);
        assert_eq!(raw.items[0].payload.len(), expected_len);
        assert_eq!(raw.next_input_index, start + 1);
        assert_eq!(raw.end_input_index, 4);
    }
}

#[test]
fn exact_byte_target_and_item_cap_have_independent_boundaries() {
    let (_db, mut storage) = fixture("historical-item-budget", 20, 1, 1);
    let mut inputs = vec![input(
        1,
        SENDER_B,
        vec![7; HISTORICAL_INPUT_PAYLOAD_TARGET_BYTES],
    )];
    inputs.extend((0..HISTORICAL_INPUT_MAX_ITEMS).map(|_| input(2, SENDER_B, vec![])));
    append(&mut storage, 20, &inputs);
    let first = page(&mut storage, 0, HISTORICAL_INPUT_MAX_ITEMS);
    assert_eq!(first.items.len(), HISTORICAL_INPUT_MAX_ITEMS);
    assert_eq!(first.next_input_index, HISTORICAL_INPUT_MAX_ITEMS as u64);
    assert_eq!(first.end_input_index, HISTORICAL_INPUT_MAX_ITEMS as u64 + 1);
    let last = page(
        &mut storage,
        first.next_input_index,
        HISTORICAL_INPUT_MAX_ITEMS,
    );
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.next_input_index, last.end_input_index);
}

#[test]
#[should_panic(expected = "historical L1 page has an input gap")]
fn missing_raw_input_is_an_invariant_fault() {
    let (_db, mut storage) = fixture("historical-gap", 20, 3, 1);
    append(
        &mut storage,
        20,
        &[
            input(1, SENDER_B, vec![1]),
            input(2, SENDER_B, vec![2]),
            input(3, SENDER_B, vec![3]),
        ],
    );
    storage
        .conn
        .execute("DELETE FROM safe_inputs WHERE safe_input_index=1", [])
        .unwrap();
    let _ = page(&mut storage, 0, 3);
}

#[test]
fn maximum_sqlite_index_has_an_unclamped_exclusive_end() {
    let (_db, mut storage) = fixture("historical-max-index", 20, 1, 1);
    storage.conn.execute(
        "INSERT INTO safe_inputs (safe_input_index,sender,payload,block_number,block_timestamp,transaction_hash) \
         VALUES (?1,?2,?3,20,240,?4)",
        params![i64::MAX, SENDER_B.as_slice(), &[1_u8][..], B256::ZERO.as_slice()],
    ).unwrap();
    let max = i64::MAX as u64;
    let last = page(&mut storage, max, 10);
    assert_eq!(last.items.len(), 1);
    assert_eq!(
        (last.end_input_index, last.next_input_index),
        (max + 1, max + 1)
    );
    assert!(page(&mut storage, max + 1, 10).items.is_empty());
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
                index,
                ExecutedInputCount::new(41),
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
            executed_input_count: ExecutedInputCount::new(41),
            next_batch_nonce: 9,
        })
    );
    storage
        .conn
        .execute("DELETE FROM snapshots WHERE batch_index=1", [])
        .unwrap();
    assert!(matches!(
        storage.history_info(None, None),
        Err(HistoricalReadError::Checkpoint(FinalizedSelectionError::Storage(
            rusqlite::Error::QueryReturnedNoRows
        )))
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
            preserved_input_count: ExecutedInputCount::ZERO,
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
            &[DirectInputExecution {
                safe_input_index: 0,
                executed_input_offset: ExecutedInputCount::new(41),
            }],
        )
        .unwrap();
    let current = era(&storage);
    let before = storage
        .history_info(Some(current), Some(RecoveryGeneration::new(0)))
        .unwrap();
    assert_eq!(before.history.head, ExecutedInputCount::new(42));
    assert_eq!(
        before.compatibility.unwrap().preserved_input_count,
        before.history.head
    );
    assert_eq!(
        storage
            .recover_aging_tip_for_recovery(head.batch_index, &protocol, now)
            .unwrap(),
        [0]
    );

    let after = storage
        .history_info(Some(current), Some(RecoveryGeneration::new(0)))
        .unwrap();
    assert_eq!(after.history.available_from, ExecutedInputCount::new(41));
    assert_eq!(after.history.head, ExecutedInputCount::new(42));
    assert_eq!(after.history.version.recovery_generation.get(), 1);
    assert_eq!(
        after.compatibility.unwrap().preserved_input_count,
        ExecutedInputCount::new(41)
    );
    let latest = storage
        .history_info(Some(current), Some(RecoveryGeneration::new(1)))
        .unwrap();
    assert_eq!(
        latest.compatibility.unwrap().preserved_input_count,
        latest.history.head
    );
}
