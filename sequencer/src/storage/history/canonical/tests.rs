// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::SystemTime;

use alloy_primitives::{B256, Signature};
use sequencer_core::user_op::{SignedUserOp, UserOp};
use tokio::sync::oneshot;

use super::*;
use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp};
use crate::storage::L2TxContext;
use crate::storage::history::insert_era_in;
use crate::storage::test_helpers::{
    SENDER_A, SENDER_B, default_protocol_timing, local_batch_payload, set_up_storage,
};
use crate::storage::{
    DirectInputExecution, IngestedSafeInput, LifecycleCommand, NewEra, SafeInputRange,
};
use crate::test_support::{pin_test_deployment_identity, temp_db};
use sequencer_core::l2_tx::L2Tx;
use sequencer_core::scheduler::Scheduled;

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
        l2_tx_index: L2TxCount::new(offset),
    }
}

fn claim(bounds: HistoryBounds, next: u64) -> HistoryClaim {
    HistoryClaim {
        version: bounds.version,
        next_input: L2TxCount::new(next),
    }
}

fn seed_aging_tip(storage: &mut Storage) {
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    storage
        .append_included_user_ops(&mut head, &[included(0, 0, 0xaa)])
        .unwrap();
    storage.close_frame_and_batch(&mut head).unwrap();
    storage
        .append_included_user_ops(&mut head, &[included(1, 1, 0xbb)])
        .unwrap();
    storage
        .append_safe_inputs(1_500, &[], SENDER_A, &default_protocol_timing())
        .unwrap();
}

#[test]
fn canonical_pages_are_inclusive_and_preserve_context_without_batch_envelopes() {
    let db = temp_db("canonical-page-context");
    let mut storage = set_up_storage(&db.path, SENDER_A);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    let first_fee = head.frame_fee();
    storage
        .append_included_user_ops(&mut head, &[included(7, 0, 0x11)])
        .unwrap();
    storage.close_frame_and_batch(&mut head).unwrap();
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
            &default_protocol_timing(),
        )
        .unwrap();
    storage
        .close_frame_only_with_executions(
            &mut head,
            10,
            SafeInputRange::new(0, 2),
            &[DirectInputExecution {
                safe_input_index: 0,
                l2_tx_index: L2TxCount::new(1),
            }],
        )
        .unwrap();
    storage
        .append_included_user_ops(&mut head, &[included(8, 2, 0x33)])
        .unwrap();

    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.available_from, L2TxCount::ZERO);
    assert_eq!(bounds.head, L2TxCount::new(3));
    let first = storage.canonical_history_page(claim(bounds, 0), 2).unwrap();
    assert_eq!(first.bounds, bounds);
    assert_eq!(first.next_claim(), claim(bounds, 2));
    assert_eq!(first.rows.len(), 2);
    assert_eq!(first.rows[0].offset, L2TxCount::ZERO);
    match &first.rows[0].context {
        L2TxContext::UserOp { tx, batch_nonce } => {
            assert_eq!(tx.sender, SENDER_B);
            assert_eq!(tx.data, vec![0x11]);
            assert_eq!(tx.fee, first_fee);
            assert_eq!((tx.nonce, tx.block, *batch_nonce), (7, 0, 0));
        }
        other => panic!("expected user op, got {other:?}"),
    }
    assert_eq!(first.rows[1].offset, L2TxCount::new(1));
    match &first.rows[1].context {
        L2TxContext::Direct {
            tx,
            batch_nonce,
            transaction_hash: actual_hash,
        } => {
            assert_eq!(tx.sender, SENDER_B);
            assert_eq!(tx.payload, vec![0x22]);
            assert_eq!(tx.block, 10);
            assert_eq!((tx.input_index, *batch_nonce, tx.timestamp), (0, 1, 100));
            assert_eq!(*actual_hash, transaction_hash);
        }
        other => panic!("expected direct input, got {other:?}"),
    }
    let last = storage
        .canonical_history_page(first.next_claim(), 2)
        .unwrap();
    assert_eq!(last.rows.len(), 1);
    assert_eq!(last.rows[0].offset, L2TxCount::new(2));
    match &last.rows[0].context {
        L2TxContext::UserOp { tx, batch_nonce } => {
            assert_eq!(tx.data, vec![0x33]);
            assert_eq!(tx.fee, head.frame_fee());
            assert_eq!((tx.nonce, tx.block, *batch_nonce), (8, 10, 1));
        }
        other => panic!("expected user op, got {other:?}"),
    }
    assert_eq!(last.next_claim(), claim(bounds, 3));
    let tail = storage
        .canonical_history_page(last.next_claim(), 2)
        .unwrap();
    assert!(tail.rows.is_empty());
    assert_eq!(tail.next_claim(), last.next_claim());
    let zero = storage.canonical_history_page(claim(bounds, 1), 0).unwrap();
    assert!(zero.rows.is_empty());
    assert_eq!(zero.next_claim(), claim(bounds, 1));
}

#[test]
fn recovery_refuses_old_claims_and_reuses_canonical_offsets_after_suffix_replacement() {
    let db = temp_db("canonical-page-recovery");
    let mut storage = set_up_storage(&db.path, SENDER_A);
    seed_aging_tip(&mut storage);
    let before = storage.history_bounds().unwrap();
    assert_eq!(before.head, L2TxCount::new(2));

    assert_eq!(
        storage.recover_aging_tip(1_200).unwrap(),
        crate::storage::test_helpers::batch_ids(&[1])
    );
    let recovered = storage.history_bounds().unwrap();
    assert_eq!(recovered.version.era_id, before.version.era_id);
    assert_eq!(recovered.version.recovery_generation.get(), 1);
    assert_eq!(recovered.head, L2TxCount::new(1));
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
        .append_included_user_ops(&mut head, &[included(1, 1, 0xcc)])
        .unwrap();
    let page = storage
        .canonical_history_page(claim(recovered, 1), 2)
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].offset, L2TxCount::new(1));
    assert_eq!(page.next_input, before.head);
    match &page.rows[0].context {
        L2TxContext::UserOp { tx, .. } => assert_eq!(tx.data, vec![0xcc]),
        other => panic!("expected replacement user op, got {other:?}"),
    }
    let audit_rows: i64 = storage
        .conn
        .query_row("SELECT COUNT(*) FROM user_ops", [], |row| row.get(0))
        .unwrap();
    assert_eq!(audit_rows, 3);
    assert_eq!(
        storage
            .canonical_history_page(claim(recovered, 0), 10)
            .unwrap()
            .rows
            .len(),
        2
    );
}

fn scheduled_direct(input_index: u64, batch_nonce: u64) -> Scheduled {
    Scheduled {
        batch_nonce,
        tx: L2Tx::Direct(Direct {
            input_index,
            sender: SENDER_B,
            block: 0,
            timestamp: 0,
            payload: vec![],
        }),
    }
}

fn scheduled_op(nonce: u32, batch_nonce: u64) -> Scheduled {
    Scheduled {
        batch_nonce,
        tx: L2Tx::UserOp(UserOpTx {
            sender: SENDER_B,
            nonce,
            fee: 9,
            block: 7,
            data: vec![0xd0, nonce as u8],
        }),
    }
}

/// An era rebuilt from a machine at count 41: inputs 0 and 2 are directs,
/// input 1 an accepted batch with three ops, the second skipped. Counts
/// 41..45 are rebuilt; the root frame then serves live rows from 45.
fn seed_two_table_era(storage: &mut Storage) {
    pin_test_deployment_identity(storage, SENDER_A);
    let input = |block: u64, payload: u8| IngestedSafeInput {
        sender: SENDER_B,
        payload: vec![payload],
        block_number: block,
        block_timestamp: 1_000 + block,
        transaction_hash: B256::repeat_byte(payload),
    };
    storage
        .append_ingested_safe_inputs_with_timestamp(
            10,
            1_010,
            &[input(5, 0xa0), input(6, 0xa1), input(10, 0xa2)],
            &default_protocol_timing(),
        )
        .unwrap();
    // The op at nonce 5 is skipped, so it has no row.
    storage
        .append_rebuilt_history(&[
            (L2TxCount::new(41), scheduled_direct(0, 3)),
            (L2TxCount::new(42), scheduled_op(0, 3)),
        ])
        .unwrap();
    storage
        .append_rebuilt_history(&[
            (L2TxCount::new(43), scheduled_op(1, 3)),
            (L2TxCount::new(44), scheduled_direct(2, 4)),
        ])
        .unwrap();
    storage
        .publish_era(
            std::path::Path::new("/snapshot/era"),
            &NewEra {
                base: L2TxCount::new(41),
                rebuilt_end: L2TxCount::new(45),
                stop_block: 10,
                first_queued_input: 3,
                anchor_batch_nonce: 5,
            },
        )
        .unwrap();
    storage.ensure_open_tip().unwrap();
}

#[test]
fn a_page_spans_the_rebuilt_history_and_the_live_rows_without_gaps() {
    let db = temp_db("canonical-page-two-tables");
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    seed_two_table_era(&mut storage);
    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.available_from, L2TxCount::new(41));
    assert_eq!(bounds.head, L2TxCount::new(45));
    assert!(
        storage
            .canonical_history_page(claim(bounds, 45), 10)
            .unwrap()
            .rows
            .is_empty()
    );
    let mut head = storage.open_state().unwrap().unwrap();
    // The first live row must sit at the rebuilt history's end.
    assert!(
        storage
            .append_included_user_ops(&mut head, &[included(2, 44, 0xcc)])
            .is_err()
    );
    let mut head = storage.open_state().unwrap().unwrap();
    storage
        .append_included_user_ops(&mut head, &[included(2, 45, 0xcc), included(3, 46, 0xdd)])
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.head, L2TxCount::new(47));

    let whole = storage
        .canonical_history_page(claim(bounds, 41), 100)
        .unwrap();
    assert_eq!(
        whole
            .rows
            .iter()
            .map(|row| row.offset.get())
            .collect::<Vec<_>>(),
        (41..47).collect::<Vec<_>>()
    );
    assert_eq!(whole.next_claim(), claim(bounds, 47));
    match &whole.rows[0].context {
        L2TxContext::Direct {
            tx,
            batch_nonce,
            transaction_hash,
        } => {
            assert_eq!(
                (tx.input_index, tx.sender, tx.block, tx.timestamp),
                (0, SENDER_B, 5, 1_005)
            );
            assert_eq!(tx.payload, vec![0xa0]);
            assert_eq!(*batch_nonce, 3);
            assert_eq!(*transaction_hash, B256::repeat_byte(0xa0));
        }
        other => panic!("expected the rebuilt direct, got {other:?}"),
    }
    // The skipped op (nonce 5) has no row, so the feed leaves it out.
    let rebuilt_ops: Vec<_> = whole.rows[1..3]
        .iter()
        .map(|row| match &row.context {
            L2TxContext::UserOp { tx, batch_nonce } => {
                assert_eq!((tx.sender, tx.fee, tx.block), (SENDER_B, 9, 7));
                (tx.nonce, *batch_nonce)
            }
            other => panic!("expected a rebuilt user op, got {other:?}"),
        })
        .collect();
    assert_eq!(rebuilt_ops, vec![(0, 3), (1, 3)]);
    match &whole.rows[3].context {
        L2TxContext::Direct {
            tx, batch_nonce, ..
        } => assert_eq!((tx.input_index, tx.timestamp, *batch_nonce), (2, 1_010, 4)),
        other => panic!("expected the second rebuilt direct, got {other:?}"),
    }
    match &whole.rows[4].context {
        L2TxContext::UserOp { tx, batch_nonce } => {
            assert_eq!((tx.data.clone(), *batch_nonce), (vec![0xcc], 5));
        }
        other => panic!("expected the first live op, got {other:?}"),
    }

    // Pages that split at and around the seam keep one contiguous cursor.
    for (from, limit) in [(43, 3), (44, 1), (44, 2), (45, 5), (41, 4)] {
        let page = storage
            .canonical_history_page(claim(bounds, from), limit)
            .unwrap();
        let end = (from + limit as u64).min(47);
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.offset.get())
                .collect::<Vec<_>>(),
            (from..end).collect::<Vec<_>>(),
            "from {from}, limit {limit}"
        );
        assert_eq!(page.next_claim(), claim(bounds, end));
    }
    assert!(matches!(
        storage.canonical_history_page(claim(bounds, 40), 10),
        Err(HistoryReadError::Policy(HistoryPolicyError::HistoryUnavailable { available_from }))
            if available_from == L2TxCount::new(41)
    ));
}

#[test]
#[should_panic(expected = "canonical history page has an attribution gap")]
fn a_missing_rebuilt_row_below_the_end_fails_loud() {
    let db = temp_db("canonical-page-rebuilt-hole");
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    seed_two_table_era(&mut storage);
    storage
        .conn
        .execute_batch(
            "DROP TRIGGER trg_rebuilt_history_not_deletable; \
             DELETE FROM rebuilt_history WHERE l2_tx_count = 42;",
        )
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    let _ = storage.canonical_history_page(claim(bounds, 41), 10);
}

#[test]
fn a_deep_backlog_can_be_read_in_small_bounded_pages() {
    let db = temp_db("canonical-page-deep-backlog");
    let mut storage = set_up_storage(&db.path, SENDER_A);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    let inputs: Vec<_> = (0..50_001)
        .map(|offset| included(offset, u64::from(offset), 0xaa))
        .collect();
    storage
        .append_included_user_ops(&mut head, &inputs)
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.head, L2TxCount::new(50_001));
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
        assert_eq!(page.next_claim(), claim(bounds, from + 2));
    }
}

#[test]
fn one_read_transaction_keeps_history_identity_and_rows_coherent_during_recovery() {
    let db = temp_db("canonical-page-read-snapshot");
    let mut writer = set_up_storage(&db.path, SENDER_A);
    seed_aging_tip(&mut writer);
    let mut reader = Storage::open_read_only(&db.path).unwrap();
    let tx = reader.conn.transaction().unwrap();
    let before = history_bounds_in(&tx).unwrap();

    writer.recover_aging_tip(1_200).unwrap();
    let mut head = writer.open_state().unwrap().unwrap();
    writer
        .append_included_user_ops(&mut head, &[included(1, 1, 0xcc)])
        .unwrap();
    let era = crate::storage::history::era_in(&tx).unwrap();
    let retained = canonical_page_in(&tx, &era, before, L2TxCount::new(1), 2).unwrap();
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
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    let last = i64::MAX as u64;
    storage
        .write(|tx| insert_era_in(tx, &crate::storage::test_helpers::era(last, last, 0)))
        .unwrap();
    storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    let mut head = storage.open_state().unwrap().unwrap();
    storage
        .append_included_user_ops(&mut head, &[included(0, last, 0xaa)])
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    assert_eq!(bounds.head, L2TxCount::new(last + 1));
    let page = storage
        .canonical_history_page(claim(bounds, last), 1)
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].offset, L2TxCount::new(last));
    let tail = storage
        .canonical_history_page(page.next_claim(), usize::MAX)
        .unwrap();
    assert!(tail.rows.is_empty());
    assert_eq!(tail.next_claim(), page.next_claim());
}

#[test]
#[should_panic(expected = "canonical history page has an attribution gap")]
fn an_interior_history_hole_fails_loud() {
    let db = temp_db("canonical-page-corrupt-attribution");
    let mut storage = set_up_storage(&db.path, SENDER_A);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    storage
        .append_included_user_ops(
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
            "DROP TRIGGER trg_protect_valid_application_input_delete;\n\
         DELETE FROM application_inputs WHERE l2_tx_count = 1;",
        )
        .unwrap();
    let bounds = storage.history_bounds().unwrap();
    storage.canonical_history_page(claim(bounds, 0), 3).unwrap();
}
