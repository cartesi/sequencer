// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::SystemTime;

use alloy_primitives::{B256, Signature};
use sequencer_core::user_op::{SignedUserOp, UserOp};
use tokio::sync::oneshot;

use super::*;
use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp};
use crate::storage::test_helpers::{
    SENDER_A, SENDER_B, default_protocol_timing, local_batch_payload, pin_test_deployment_identity,
    temp_db,
};
use crate::storage::{
    DirectInputExecution, FrontierMode, IngestedSafeInput, LifecycleCommand, SafeInputRange,
    StoredSafeInput,
};

fn included(nonce: u32, offset: u64, payload: u8) -> IncludedUserOp {
    let (respond_to, _response) = oneshot::channel();
    IncludedUserOp {
        pending: PendingUserOp {
            signed: SignedUserOp {
                sender: SENDER_B,
                signature: Signature::test_signature(),
                user_op: UserOp {
                    nonce,
                    max_fee: u16::MAX,
                    data: vec![payload].into(),
                },
            },
            respond_to,
            received_at: SystemTime::now(),
        },
        executed_input_offset: ExecutedInputCount::new(offset),
    }
}

fn claim(bounds: HistoryBounds, next: u64) -> HistoryClaim {
    HistoryClaim {
        version: bounds.version,
        next_input: ExecutedInputCount::new(next),
    }
}

fn seed_aging_tip(storage: &mut Storage) {
    pin_test_deployment_identity(storage, SENDER_A);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    storage
        .append_executed_user_ops_chunk(&mut head, &[included(0, 0, 0xaa)])
        .unwrap();
    storage.close_frame_and_batch(&mut head, 0).unwrap();
    storage
        .append_executed_user_ops_chunk(&mut head, &[included(1, 1, 0xbb)])
        .unwrap();
    storage
        .append_safe_inputs(1_500, &[], SENDER_A, &default_protocol_timing())
        .unwrap();
}

#[test]
fn canonical_pages_are_inclusive_and_preserve_context_without_batch_envelopes() {
    let db = temp_db("canonical-page-context");
    let mut storage = Storage::open(&db.path).unwrap();
    pin_test_deployment_identity(&mut storage, SENDER_A);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    let first_fee = head.frame_fee;
    storage
        .append_executed_user_ops_chunk(&mut head, &[included(7, 0, 0x11)])
        .unwrap();
    storage.close_frame_and_batch(&mut head, 0).unwrap();
    let envelope = local_batch_payload(&mut storage, 0);
    let transaction_hash = B256::repeat_byte(0x42);
    storage
        .append_ingested_safe_inputs_with_timestamp(
            10,
            100,
            &[
                IngestedSafeInput {
                    sender: SENDER_B,
                    payload: vec![0x22],
                    block_number: 10,
                    block_timestamp: 100,
                    transaction_hash,
                },
                IngestedSafeInput {
                    sender: SENDER_A,
                    payload: envelope,
                    block_number: 10,
                    block_timestamp: 100,
                    transaction_hash: B256::repeat_byte(0x43),
                },
            ],
            SENDER_A,
            &default_protocol_timing(),
            FrontierMode::Populate,
        )
        .unwrap();
    storage
        .close_frame_only_with_executions(
            &mut head,
            10,
            SafeInputRange::new(0, 2),
            &[DirectInputExecution {
                safe_input_index: 0,
                executed_input_offset: ExecutedInputCount::new(1),
            }],
            None,
        )
        .unwrap();
    storage
        .append_executed_user_ops_chunk(&mut head, &[included(8, 2, 0x33)])
        .unwrap();

    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.available_from, ExecutedInputCount::ZERO);
    assert_eq!(bounds.head, ExecutedInputCount::new(3));
    assert_eq!(storage.ordered_l2_txs_page_from(0, 10).unwrap().len(), 4);
    let first = storage.canonical_history_page(claim(bounds, 0), 2).unwrap();
    assert_eq!(first.bounds, bounds);
    assert_eq!(first.next, claim(bounds, 2));
    assert_eq!(first.rows.len(), 2);
    assert_eq!(first.rows[0].offset, ExecutedInputCount::ZERO);
    match &first.rows[0].context {
        L2TxContext::UserOp {
            tx,
            nonce,
            safe_block,
            batch_nonce,
        } => {
            assert_eq!(tx.sender, SENDER_B);
            assert_eq!(tx.data, vec![0x11]);
            assert_eq!(tx.fee, first_fee);
            assert_eq!((*nonce, *safe_block, *batch_nonce), (7, 0, 0));
        }
        other => panic!("expected user op, got {other:?}"),
    }
    assert_eq!(first.rows[1].offset, ExecutedInputCount::new(1));
    match &first.rows[1].context {
        L2TxContext::DirectInput {
            tx,
            input_index,
            safe_block,
            batch_nonce,
            block_timestamp,
            transaction_hash: actual_hash,
        } => {
            assert_eq!(tx.sender, SENDER_B);
            assert_eq!(tx.payload, vec![0x22]);
            assert_eq!(tx.block_number, 10);
            assert_eq!(
                (*input_index, *safe_block, *batch_nonce, *block_timestamp),
                (0, 10, 1, 100)
            );
            assert_eq!(*actual_hash, transaction_hash);
        }
        other => panic!("expected direct input, got {other:?}"),
    }
    let last = storage.canonical_history_page(first.next, 2).unwrap();
    assert_eq!(last.rows.len(), 1);
    assert_eq!(last.rows[0].offset, ExecutedInputCount::new(2));
    match &last.rows[0].context {
        L2TxContext::UserOp {
            tx,
            nonce,
            safe_block,
            batch_nonce,
        } => {
            assert_eq!(tx.data, vec![0x33]);
            assert_eq!(tx.fee, head.frame_fee);
            assert_eq!((*nonce, *safe_block, *batch_nonce), (8, 10, 1));
        }
        other => panic!("expected user op, got {other:?}"),
    }
    assert_eq!(last.next, claim(bounds, 3));
    let tail = storage.canonical_history_page(last.next, 2).unwrap();
    assert!(tail.rows.is_empty());
    assert_eq!(tail.next, last.next);
    let zero = storage.canonical_history_page(claim(bounds, 1), 0).unwrap();
    assert!(zero.rows.is_empty());
    assert_eq!(zero.next, claim(bounds, 1));
}

#[test]
fn recovery_refuses_old_claims_and_reuses_canonical_offsets_across_physical_holes() {
    let db = temp_db("canonical-page-recovery");
    let mut storage = Storage::open(&db.path).unwrap();
    seed_aging_tip(&mut storage);
    let before = storage.history_bounds().unwrap();
    assert_eq!(before.head, ExecutedInputCount::new(2));

    assert_eq!(storage.recover_aging_tip(1_200).unwrap(), vec![1]);
    let recovered = storage.history_bounds().unwrap();
    assert_eq!(recovered.version.era_id, before.version.era_id);
    assert_eq!(recovered.version.recovery_generation.get(), 1);
    assert_eq!(recovered.head, ExecutedInputCount::new(1));
    for limit in [0, 2] {
        assert!(matches!(
            storage.canonical_history_page(claim(before, 2), limit),
            Err(HistoryReadError::Policy(HistoryPolicyError::StaleGeneration { current }))
                if current == recovered.version
        ));
    }
    assert!(
        storage
            .canonical_history_page(claim(recovered, 1), 2)
            .unwrap()
            .rows
            .is_empty()
    );
    let mut head = storage.open_state().unwrap().unwrap();
    storage
        .append_executed_user_ops_chunk(&mut head, &[included(1, 1, 0xcc)])
        .unwrap();
    let page = storage
        .canonical_history_page(claim(recovered, 1), 2)
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].offset, ExecutedInputCount::new(1));
    assert_eq!(page.next.next_input, before.head);
    match &page.rows[0].context {
        L2TxContext::UserOp { tx, .. } => assert_eq!(tx.data, vec![0xcc]),
        other => panic!("expected replacement user op, got {other:?}"),
    }
    let physical = storage.ordered_l2_txs_page_from(0, 10).unwrap();
    assert_eq!(
        physical.iter().map(|row| row.db_offset).collect::<Vec<_>>(),
        vec![1, 3]
    );
    let audit_rows: i64 = storage
        .conn
        .query_row("SELECT COUNT(*) FROM sequenced_l2_txs", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(audit_rows, 3);
}

#[test]
fn rebuilt_history_starts_at_its_absolute_base_and_excludes_padding() {
    let db = temp_db("canonical-page-rebuild");
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
    storage
        .append_safe_inputs(
            10,
            &[
                StoredSafeInput {
                    sender: SENDER_B,
                    payload: vec![0xaa],
                    block_number: 10,
                },
                StoredSafeInput {
                    sender: SENDER_B,
                    payload: vec![0xbb],
                    block_number: 10,
                },
            ],
            SENDER_A,
            &default_protocol_timing(),
        )
        .unwrap();
    storage.open_recovery_tip(10).unwrap();
    let physical_head = storage.valid_ordered_l2_tx_head().unwrap();
    storage
        .insert_initial_finalized_dump(&db._dir.path().join("recovered"), 10, physical_head, 41, 2)
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.available_from, ExecutedInputCount::new(41));
    assert_eq!(bounds.head, bounds.available_from);
    assert!(
        storage
            .canonical_history_page(claim(bounds, 41), 10)
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(
        matches!(storage.canonical_history_page(claim(bounds, 40), 10),
        Err(HistoryReadError::Policy(HistoryPolicyError::HistoryUnavailable { available_from }))
            if available_from == bounds.available_from)
    );
    assert!(
        matches!(storage.canonical_history_page(claim(bounds, 42), 10),
        Err(HistoryReadError::Policy(HistoryPolicyError::AheadOfHead { head }))
            if head == bounds.head)
    );

    let mut head = storage.open_state().unwrap().unwrap();
    storage
        .append_executed_user_ops_chunk(&mut head, &[included(0, 41, 0xcc)])
        .unwrap();
    let page = storage
        .canonical_history_page(claim(bounds, 41), 10)
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].offset, ExecutedInputCount::new(41));
    assert_eq!(page.next.next_input, ExecutedInputCount::new(42));
    let physical = storage.ordered_l2_txs_page_from(0, 10).unwrap();
    assert_eq!(physical.len(), 3);
    assert!(
        physical[..2]
            .iter()
            .all(|row| row.executed_input_offset.is_none())
    );
}

#[test]
fn a_deep_backlog_can_be_read_in_small_bounded_pages() {
    let db = temp_db("canonical-page-deep-backlog");
    let mut storage = Storage::open(&db.path).unwrap();
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    let inputs: Vec<_> = (0..50_001)
        .map(|offset| included(offset, u64::from(offset), 0xaa))
        .collect();
    storage
        .append_executed_user_ops_chunk(&mut head, &inputs)
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.head, ExecutedInputCount::new(50_001));
    for from in [0, 25_000, 49_999] {
        let page = storage
            .canonical_history_page(claim(bounds, from), 2)
            .unwrap();
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.offset.get())
                .collect::<Vec<_>>(),
            vec![from, from + 1]
        );
        assert_eq!(page.next, claim(bounds, from + 2));
    }
}

#[test]
fn one_read_transaction_keeps_history_identity_and_rows_coherent_during_recovery() {
    let db = temp_db("canonical-page-read-snapshot");
    let mut writer = Storage::open(&db.path).unwrap();
    seed_aging_tip(&mut writer);
    let mut reader = Storage::open_read_only(&db.path).unwrap();
    let tx = reader.conn.transaction().unwrap();
    let before = history_bounds_in(&tx).unwrap();

    writer.recover_aging_tip(1_200).unwrap();
    let mut head = writer.open_state().unwrap().unwrap();
    writer
        .append_executed_user_ops_chunk(&mut head, &[included(1, 1, 0xcc)])
        .unwrap();
    let retained = canonical_page_in(&tx, before, ExecutedInputCount::new(1), 2).unwrap();
    assert_eq!(retained.bounds, before);
    assert_eq!(history_bounds_in(&tx).unwrap(), before);
    assert_eq!(retained.rows.len(), 1);
    match &retained.rows[0].context {
        L2TxContext::UserOp { tx, .. } => assert_eq!(tx.data, vec![0xbb]),
        other => panic!("expected old snapshot user op, got {other:?}"),
    }
    tx.commit().unwrap();

    let after = reader.history_bounds().unwrap();
    assert_eq!(after.head, before.head);
    assert_ne!(after.version, before.version);
    let fresh = reader.canonical_history_page(claim(after, 1), 2).unwrap();
    assert_eq!(fresh.bounds, after);
    match &fresh.rows[0].context {
        L2TxContext::UserOp { tx, .. } => assert_eq!(tx.data, vec![0xcc]),
        other => panic!("expected current user op, got {other:?}"),
    }
}

#[test]
fn tail_after_the_largest_sqlite_offset_is_empty_without_clamping() {
    let db = temp_db("canonical-page-sqlite-tail");
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
    storage.open_recovery_tip(0).unwrap();
    let last = i64::MAX as u64;
    storage
        .insert_initial_finalized_dump(&db._dir.path().join("recovered"), 0, 0, last, 0)
        .unwrap();
    let mut head = storage.open_state().unwrap().unwrap();
    storage
        .append_executed_user_ops_chunk(&mut head, &[included(0, last, 0xaa)])
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.head, ExecutedInputCount::new(last + 1));
    let page = storage
        .canonical_history_page(claim(bounds, last), 1)
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].offset, ExecutedInputCount::new(last));
    let tail = storage
        .canonical_history_page(page.next, usize::MAX)
        .unwrap();
    assert!(tail.rows.is_empty());
    assert_eq!(tail.next, page.next);
}

#[test]
#[should_panic(expected = "canonical history page has an attribution gap")]
fn an_interior_mapping_hole_fails_loud() {
    let db = temp_db("canonical-page-corrupt-attribution");
    let mut storage = Storage::open(&db.path).unwrap();
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    storage
        .append_executed_user_ops_chunk(
            &mut head,
            &[
                included(0, 0, 0xaa),
                included(1, 1, 0xbb),
                included(2, 2, 0xcc),
            ],
        )
        .unwrap();
    storage
        .conn
        .execute_batch(
            "DROP TRIGGER trg_protect_valid_executed_input_delete;\n\
         DELETE FROM executed_inputs WHERE executed_input_offset = 1;",
        )
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    storage.canonical_history_page(claim(bounds, 0), 3).unwrap();
}
