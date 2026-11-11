// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use super::*;
use crate::storage::Storage;
use crate::storage::test_helpers::{SENDER_A, set_up_storage};
use crate::test_support::temp_db;

// Synthetic cuts exercise the query's full interval contract; no production
// path writes a cut.
fn seed_cuts(storage: &mut Storage, cuts: &[u64]) {
    storage
        .write(|tx| {
            for (index, cut) in cuts.iter().enumerate() {
                let generation = i64::try_from(index + 1).unwrap();
                tx.execute(
                    "INSERT INTO history_generation_cuts (recovery_generation,preserved_l2_tx_count) VALUES (?1,?2)",
                    params![generation, u64_to_i64(*cut)],
                )?;
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn compatibility_uses_every_intervening_cut_and_the_current_head() {
    let db = temp_db("generation-cut-minimum");
    let mut storage = set_up_storage(&db.path, SENDER_A);
    seed_cuts(&mut storage, &[3, 5, 2, 4]);
    for (from, current, head, expected) in [
        (0, 1, 9, 3),
        (0, 2, 9, 3),
        (1, 2, 9, 5),
        (0, 3, 9, 2),
        (1, 3, 9, 2),
        (2, 3, 9, 2),
        (0, 4, 9, 2),
        (3, 4, 9, 4),
        (3, 4, 1, 1),
        (4, 4, 9, 9),
    ] {
        assert_eq!(
            preserved_input_count_in(
                &storage.conn,
                RecoveryGeneration::new(from),
                RecoveryGeneration::new(current),
                L2TxCount::new(head)
            )
            .unwrap(),
            L2TxCount::new(expected),
            "from={from}, current={current}, head={head}",
        );
    }
}

#[test]
#[should_panic(expected = "history generation lineage is incomplete")]
fn missing_intermediate_cut_fails_instead_of_certifying_a_partial_minimum() {
    let db = temp_db("generation-cut-gap");
    let mut storage = set_up_storage(&db.path, SENDER_A);
    seed_cuts(&mut storage, &[3, 1, 5]);
    storage
        .conn
        .execute_batch(
            "DROP TRIGGER trg_history_generation_cuts_not_deletable; \
         DELETE FROM history_generation_cuts WHERE recovery_generation=2",
        )
        .unwrap();
    let _ = preserved_input_count_in(
        &storage.conn,
        RecoveryGeneration::new(0),
        RecoveryGeneration::new(3),
        L2TxCount::new(9),
    );
}

#[test]
fn current_head_may_be_the_boundary_after_the_maximum_sqlite_offset() {
    let db = temp_db("generation-cut-max-head");
    let mut storage = set_up_storage(&db.path, SENDER_A);
    let max = i64::MAX as u64;
    seed_cuts(&mut storage, &[max]);
    let head = L2TxCount::new(max + 1);
    assert_eq!(
        preserved_input_count_in(
            &storage.conn,
            RecoveryGeneration::new(1),
            RecoveryGeneration::new(1),
            head
        )
        .unwrap(),
        head,
    );
    assert_eq!(
        preserved_input_count_in(
            &storage.conn,
            RecoveryGeneration::new(0),
            RecoveryGeneration::new(1),
            head
        )
        .unwrap(),
        L2TxCount::new(max),
    );
}
