// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::B256;
use sequencer_core::l2_tx::{Direct, UserOpTx};

use super::*;
use crate::storage::test_helpers::{SENDER_A, SENDER_B, era};
use crate::storage::{IngestedSafeInput, LifecycleCommand, NewEra};
use crate::test_support::{TestDb, pin_test_deployment_identity, temp_db};

fn count(value: u64) -> L2TxCount {
    L2TxCount::new(value)
}

fn direct(input_index: u64, batch_nonce: u64) -> Scheduled {
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

fn op(nonce: u32, batch_nonce: u64) -> Scheduled {
    Scheduled {
        batch_nonce,
        tx: L2Tx::UserOp(UserOpTx {
            sender: SENDER_B,
            nonce,
            fee: 3,
            block: 4,
            data: vec![nonce as u8],
        }),
    }
}

/// A setup directory with three safe inputs: two directs around a batch
/// submitter input.
fn setup_storage(name: &str) -> (TestDb, Storage) {
    let db = temp_db(name);
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    pin_test_deployment_identity(&mut storage, SENDER_A);
    let input = |sender, block: u64| IngestedSafeInput {
        sender,
        payload: vec![block as u8],
        block_number: block,
        block_timestamp: 100 + block,
        transaction_hash: B256::repeat_byte(block as u8),
    };
    storage
        .append_ingested_safe_inputs_with_timestamp(
            9,
            109,
            &[input(SENDER_B, 5), input(SENDER_A, 6), input(SENDER_B, 9)],
        )
        .unwrap();
    (db, storage)
}

/// Every stored row as `(count, batch nonce, input index, sender, nonce)`,
/// in count order.
type StoredRow = (i64, i64, Option<i64>, Option<Vec<u8>>, Option<i64>);

fn stored(storage: &mut Storage) -> Vec<StoredRow> {
    storage
        .conn
        .prepare(
            "SELECT l2_tx_count, batch_nonce, safe_input_index, sender, nonce \
             FROM rebuilt_history ORDER BY l2_tx_count",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn rows_are_stored_once_with_their_counts() {
    let (_db, mut storage) = setup_storage("rebuilt-append");
    // The first row may carry any count; the era ties it to the base.
    storage
        .append_rebuilt_history(&[(count(41), direct(0, 2)), (count(42), op(0, 2))])
        .unwrap();
    storage
        .append_rebuilt_history(&[(count(43), op(1, 2)), (count(44), direct(2, 3))])
        .unwrap();
    storage.append_rebuilt_history(&[]).unwrap();
    let sender = Some(SENDER_B.to_vec());
    assert_eq!(
        stored(&mut storage),
        vec![
            (41, 2, Some(0), None, None),
            (42, 2, None, sender.clone(), Some(0)),
            (43, 2, None, sender, Some(1)),
            (44, 3, Some(2), None, None),
        ]
    );
}

#[test]
fn a_row_without_a_count_is_refused() {
    let (_db, mut storage) = setup_storage("rebuilt-no-count");
    // The key is a rowid alias: without the trigger SQLite would number it.
    let error = storage
        .conn
        .execute(
            "INSERT INTO rebuilt_history (batch_nonce, safe_input_index) VALUES (0, 0)",
            [],
        )
        .expect_err("a row needs its count");
    assert_eq!(error.to_string(), "a rebuilt row carries its L2 tx count");
    let error = storage
        .conn
        .execute(
            "INSERT INTO rebuilt_history (l2_tx_count, batch_nonce, safe_input_index) \
             VALUES (NULL, 0, 0)",
            [],
        )
        .expect_err("a NULL count is numbered by SQLite");
    assert_eq!(error.to_string(), "a rebuilt row carries its L2 tx count");
    assert!(stored(&mut storage).is_empty());
}

#[test]
fn counts_follow_one_another_without_gaps() {
    let (_db, mut storage) = setup_storage("rebuilt-gaps");
    storage
        .append_rebuilt_history(&[(count(7), direct(0, 0))])
        .unwrap();
    for value in [9, 7, 6] {
        let error = storage
            .append_rebuilt_history(&[(count(value), op(0, 0))])
            .expect_err("a count other than the next");
        assert_eq!(
            error.to_string(),
            "rebuilt counts follow one another without gaps",
            "{value}"
        );
    }
    // A page applies all its rows or none.
    let error = storage
        .append_rebuilt_history(&[(count(8), op(0, 0)), (count(10), op(1, 0))])
        .expect_err("the second row leaves a gap");
    assert_eq!(
        error.to_string(),
        "rebuilt counts follow one another without gaps"
    );
    assert_eq!(stored(&mut storage).len(), 1);
}

#[test]
fn a_submitter_input_is_refused_as_a_direct() {
    let (_db, mut storage) = setup_storage("rebuilt-submitter");
    let error = storage
        .append_rebuilt_history(&[(count(0), direct(1, 0))])
        .expect_err("input 1 is the batch submitter's");
    assert_eq!(
        error.to_string(),
        "a batch submitter input never executes as a direct"
    );
}

#[test]
fn directs_drain_in_input_order() {
    let (_db, mut storage) = setup_storage("rebuilt-direct-order");
    storage
        .append_rebuilt_history(&[(count(0), direct(2, 0)), (count(1), op(0, 0))])
        .unwrap();
    for input_index in [0, 2] {
        let error = storage
            .append_rebuilt_history(&[(count(2), direct(input_index, 0))])
            .expect_err("a direct at or before an earlier one");
        assert_eq!(
            error.to_string(),
            "rebuilt directs drain in input order",
            "{input_index}"
        );
    }
}

/// Appending a row reads the last count and the last direct through their
/// keys: a full scan per row would make the rebuild quadratic in the history.
#[test]
fn appending_a_row_never_scans_the_table() {
    let (_db, mut storage) = setup_storage("rebuilt-append-plan");
    let rows: Vec<_> = (0..64_u32)
        .map(|nonce| (count(u64::from(nonce)), op(nonce, 0)))
        .collect();
    storage.append_rebuilt_history(&rows).unwrap();

    let mut insert = storage
        .conn
        .prepare(
            "INSERT INTO rebuilt_history (l2_tx_count, batch_nonce, safe_input_index) \
             VALUES (64, 0, 0)",
        )
        .unwrap();
    assert_eq!(insert.execute([]).unwrap(), 1);
    let scanned = insert.get_status(rusqlite::StatementStatus::FullscanStep);
    assert!(scanned < 8, "{scanned} full-scan steps for one row");
}

#[test]
fn rows_are_immutable_and_never_deleted() {
    let (_db, mut storage) = setup_storage("rebuilt-immutable");
    storage
        .append_rebuilt_history(&[(count(0), direct(0, 0)), (count(1), op(0, 0))])
        .unwrap();
    for sql in [
        "UPDATE rebuilt_history SET batch_nonce = 1 WHERE l2_tx_count = 1",
        "UPDATE rebuilt_history SET l2_tx_count = 5 WHERE l2_tx_count = 1",
    ] {
        let error = storage.conn.execute(sql, []).expect_err(sql);
        assert_eq!(error.to_string(), "rebuilt history rows are immutable");
    }
    let error = storage
        .conn
        .execute("DELETE FROM rebuilt_history WHERE l2_tx_count = 1", [])
        .expect_err("rows are never deleted");
    assert_eq!(
        error.to_string(),
        "rebuilt history is retained for the database lifetime"
    );
    assert_eq!(stored(&mut storage).len(), 2);
}

#[test]
fn the_published_era_seals_its_rebuilt_rows() {
    let (_db, mut storage) = setup_storage("rebuilt-sealed");
    storage
        .append_rebuilt_history(&[(count(41), direct(0, 0)), (count(42), op(0, 1))])
        .unwrap();
    let prefix = std::path::Path::new("/snapshot/era");
    // The era must span exactly the rows: base 41, end 43.
    for (base, end) in [(41, 42), (40, 43), (41, 44)] {
        let error = storage
            .publish_era(prefix, &era(base, end, 9))
            .expect_err("the counts do not fit");
        assert!(
            error
                .to_string()
                .contains("the era must span exactly its executed rebuilt rows"),
            "{error}"
        );
    }
    storage
        .publish_era(
            prefix,
            &NewEra {
                first_queued_input: 3,
                anchor_batch_nonce: 2,
                ..era(41, 43, 9)
            },
        )
        .unwrap();
    assert_eq!(storage.era().unwrap().rebuilt_end.get(), 43);

    let error = storage
        .append_rebuilt_history(&[(count(43), op(1, 1))])
        .expect_err("no row after the era is published");
    assert_eq!(
        error.to_string(),
        "rebuilt history is sealed once its era is published"
    );
    assert_eq!(stored(&mut storage).len(), 2);
}

/// An era that still queues a direct the rebuild executed would make run's
/// root frame execute it again.
#[test]
fn an_era_queueing_a_rebuilt_direct_is_refused() {
    let (_db, mut storage) = setup_storage("rebuilt-direct-queued");
    storage
        .append_rebuilt_history(&[(count(41), direct(0, 0)), (count(42), direct(2, 1))])
        .unwrap();
    let prefix = std::path::Path::new("/snapshot/era");
    let error = storage
        .publish_era(
            prefix,
            &NewEra {
                first_queued_input: 2,
                ..era(41, 43, 9)
            },
        )
        .expect_err("input 2 was drained by the rebuild");
    assert!(
        error
            .to_string()
            .contains("a rebuilt direct is never still queued"),
        "{error}"
    );
    storage
        .publish_era(
            prefix,
            &NewEra {
                first_queued_input: 3,
                ..era(41, 43, 9)
            },
        )
        .unwrap();
}
