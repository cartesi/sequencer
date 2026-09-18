// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use super::*;
use crate::storage::test_helpers::temp_db;

fn ledger(storage: &Storage) -> Vec<(i64, i64)> {
    storage
        .conn
        .prepare("SELECT recovery_generation, preserved_input_count FROM history_generation_cuts ORDER BY recovery_generation")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_>>()
        .unwrap()
}

// Synthetic cuts exercise the query's full interval contract independently
// of today's recovery pivot policy. Actual cascades are tested separately.
fn seed_cuts(storage: &mut Storage, cuts: &[u64]) {
    storage
        .write(|tx| {
            for (index, cut) in cuts.iter().enumerate() {
                let generation = i64::try_from(index + 1).unwrap();
                tx.execute(
                    "INSERT INTO history_generation_cuts (recovery_generation,preserved_input_count) VALUES (?1,?2)",
                    params![generation, u64_to_i64(*cut)],
                )?;
                tx.execute(
                    "UPDATE history_state SET recovery_generation=?1 WHERE singleton_id=0",
                    [generation],
                )?;
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn generation_and_cut_are_atomic_and_immutable() {
    let db = temp_db("generation-cut-atomic");
    let mut storage = Storage::open(&db.path).unwrap();
    let result: Result<()> = storage.write(|tx| {
        advance_recovery_generation_in(tx)?;
        Err(rusqlite::Error::InvalidQuery)
    });
    assert!(result.is_err());
    assert!(ledger(&storage).is_empty());
    assert_eq!(
        storage
            .history_state()
            .unwrap()
            .version
            .recovery_generation
            .get(),
        0
    );
    assert!(
        storage
            .conn
            .execute("UPDATE history_state SET recovery_generation=1", [])
            .is_err()
    );

    storage.write(advance_recovery_generation_in).unwrap();
    assert_eq!(ledger(&storage), [(1, 0)]);
    for sql in [
        "UPDATE history_generation_cuts SET preserved_input_count=1",
        "UPDATE history_generation_cuts SET recovery_generation=2",
        "DELETE FROM history_generation_cuts",
        "INSERT INTO history_generation_cuts VALUES (1,1)",
        "INSERT INTO history_generation_cuts VALUES (0,0)",
        "INSERT INTO history_generation_cuts VALUES (2,-1)",
    ] {
        assert!(storage.conn.execute(sql, []).is_err(), "{sql}");
    }
    drop(storage);
    let reopened = Storage::open(&db.path).unwrap();
    assert_eq!(ledger(&reopened), [(1, 0)]);
    assert_eq!(
        reopened
            .history_state()
            .unwrap()
            .version
            .recovery_generation
            .get(),
        1
    );
}

#[test]
fn compatibility_uses_every_intervening_cut_and_the_current_head() {
    let db = temp_db("generation-cut-minimum");
    let mut storage = Storage::open(&db.path).unwrap();
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
                ExecutedInputCount::new(head)
            )
            .unwrap(),
            ExecutedInputCount::new(expected),
            "from={from}, current={current}, head={head}",
        );
    }
}

#[test]
#[should_panic(expected = "history generation lineage is incomplete")]
fn missing_intermediate_cut_fails_instead_of_certifying_a_partial_minimum() {
    let db = temp_db("generation-cut-gap");
    let mut storage = Storage::open(&db.path).unwrap();
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
        ExecutedInputCount::new(9),
    );
}

#[test]
fn current_head_may_be_the_boundary_after_the_maximum_sqlite_offset() {
    let db = temp_db("generation-cut-max-head");
    let mut storage = Storage::open(&db.path).unwrap();
    let max = i64::MAX as u64;
    seed_cuts(&mut storage, &[max]);
    let head = ExecutedInputCount::new(max + 1);
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
        ExecutedInputCount::new(max),
    );
}
