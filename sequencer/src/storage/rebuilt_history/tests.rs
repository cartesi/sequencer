// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::B256;

use super::*;
use crate::storage::test_helpers::{SENDER_A, SENDER_B, default_protocol_timing, era};
use crate::storage::{EraRecord, FrontierMode, IngestedSafeInput, LifecycleCommand};
use crate::test_support::{TestDb, pin_test_deployment_identity, temp_db};

fn key(l1_input_index: u64, position: u64) -> RebuiltKey {
    RebuiltKey {
        l1_input_index,
        position,
    }
}

fn count(value: u64) -> ExecutedInputCount {
    ExecutedInputCount::new(value)
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
            SENDER_A,
            &default_protocol_timing(),
            FrontierMode::DeferUntilAnchorSet,
        )
        .unwrap();
    (db, storage)
}

#[test]
fn rows_append_in_order_and_read_back_in_chunks_for_execution() {
    let (_db, mut storage) = setup_storage("rebuilt-append");
    storage
        .append_rebuilt_history(&[(0, direct(0, 2)), (1, op(0, 2))])
        .unwrap();
    // One input's transactions may span calls; positions continue.
    storage
        .append_rebuilt_history(&[(1, op(1, 2)), (2, direct(2, 3))])
        .unwrap();
    storage.append_rebuilt_history(&[]).unwrap();

    let first = storage.rebuilt_history_after(None, 2).unwrap();
    assert_eq!(
        first.iter().map(|row| row.key).collect::<Vec<_>>(),
        vec![key(0, 0), key(1, 0)]
    );
    assert_eq!(
        first[0].tx,
        L2Tx::Direct(Direct {
            input_index: 0,
            sender: SENDER_B,
            block: 5,
            timestamp: 105,
            payload: vec![5],
        }),
        "a direct reads back from the input table"
    );
    assert_eq!(first[0].batch_nonce, 2);
    let rest = storage
        .rebuilt_history_after(Some(first[1].key), 10)
        .unwrap();
    assert_eq!(
        rest.iter().map(|row| row.key).collect::<Vec<_>>(),
        vec![key(1, 1), key(2, 0)]
    );
    assert_eq!(rest[0].tx, op(1, 2).tx);
    assert_eq!(rest[1].batch_nonce, 3);
    assert!(
        storage
            .rebuilt_history_after(Some(key(2, 0)), 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn rows_out_of_order_and_submitter_directs_are_refused() {
    let (_db, mut storage) = setup_storage("rebuilt-order");
    storage
        .append_rebuilt_history(&[(2, direct(2, 0))])
        .unwrap();
    let earlier = storage
        .append_rebuilt_history(&[(1, op(0, 0))])
        .expect_err("an earlier input after a later one");
    assert_eq!(
        earlier.to_string(),
        "rebuilt history rows are appended in order"
    );
    let skipped_position = storage
        .conn
        .execute(
            "INSERT INTO rebuilt_history (l1_input_index, position, kind, safe_input_index, \
             batch_nonce) VALUES (2, 5, 'direct', 0, 0)",
            [],
        )
        .expect_err("positions count up from 0");
    assert_eq!(
        skipped_position.to_string(),
        "rebuilt history rows are appended in order"
    );
    let submitter = storage
        .append_rebuilt_history(&[(3, direct(1, 0))])
        .expect_err("input 1 is the batch submitter's");
    assert_eq!(
        submitter.to_string(),
        "a batch submitter input never executes as a direct"
    );
    let drained_early = storage
        .append_rebuilt_history(&[(2, direct(4, 0))])
        .expect_err("a direct drains at or after its own input");
    assert!(
        drained_early.to_string().contains("CHECK"),
        "{drained_early}"
    );
}

#[test]
fn counts_are_set_once_in_row_order_and_without_gaps() {
    let (_db, mut storage) = setup_storage("rebuilt-counts");
    storage
        .append_rebuilt_history(&[
            (0, direct(0, 0)),
            (1, op(0, 0)),
            (1, op(9, 0)),
            (1, op(1, 0)),
        ])
        .unwrap();
    let order = "rebuilt counts follow the row order without gaps";
    storage
        .record_rebuilt_counts(&[(key(0, 0), count(7))])
        .unwrap();
    for (row, value, expected) in [
        (key(1, 0), 9, order),
        (key(1, 0), 7, order),
        (
            key(0, 0),
            8,
            "a rebuilt row only gains its count, once, before its era is published",
        ),
    ] {
        let error = storage
            .record_rebuilt_counts(&[(row, count(value))])
            .expect_err("refused count");
        assert_eq!(error.to_string(), expected, "{row:?} at {value}");
    }
    // Row (1, 1) is skipped: it keeps no count, and later rows continue.
    storage
        .record_rebuilt_counts(&[(key(1, 0), count(8)), (key(1, 2), count(9))])
        .unwrap();
    let late = storage
        .record_rebuilt_counts(&[(key(1, 1), count(10))])
        .expect_err("a count behind a counted row");
    assert_eq!(late.to_string(), order);
    // A chunk applies all its counts or none.
    let missing = storage
        .record_rebuilt_counts(&[(key(9, 0), count(10))])
        .expect_err("no such row");
    assert!(matches!(missing, rusqlite::Error::StatementChangedRows(0)));
    let counted: Vec<Option<i64>> = storage
        .conn
        .prepare("SELECT l2_tx_count FROM rebuilt_history ORDER BY l1_input_index, position")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(counted, vec![Some(7), Some(8), None, Some(9)]);
    for sql in [
        "UPDATE rebuilt_history SET batch_nonce = 1 WHERE l1_input_index = 1 AND position = 1",
        "UPDATE rebuilt_history SET l2_tx_count = NULL WHERE l2_tx_count = 9",
    ] {
        let error = storage.conn.execute(sql, []).expect_err(sql);
        assert_eq!(
            error.to_string(),
            "a rebuilt row only gains its count, once, before its era is published"
        );
    }
}

/// Recording a count reads the last one through the partial count index: a
/// full scan per row would make the execute pass quadratic in the history.
#[test]
fn recording_a_count_never_scans_the_table() {
    let (_db, mut storage) = setup_storage("rebuilt-count-index");
    let rows: Vec<_> = (0..64_u32).map(|nonce| (0, op(nonce, 0))).collect();
    storage.append_rebuilt_history(&rows).unwrap();
    let counts: Vec<_> = (0..63)
        .map(|position| (key(0, position), count(position)))
        .collect();
    storage.record_rebuilt_counts(&counts).unwrap();

    let mut update = storage
        .conn
        .prepare(
            "UPDATE rebuilt_history SET l2_tx_count = 63 \
             WHERE l1_input_index = 0 AND position = 63",
        )
        .unwrap();
    assert_eq!(update.execute([]).unwrap(), 1);
    let scanned = update.get_status(rusqlite::StatementStatus::FullscanStep);
    assert!(scanned < 8, "{scanned} full-scan steps for one count");
}

#[test]
fn the_published_era_seals_its_rebuilt_rows() {
    let (_db, mut storage) = setup_storage("rebuilt-sealed");
    // The trailing op is skipped, so it keeps no count.
    storage
        .append_rebuilt_history(&[(0, direct(0, 0)), (2, op(0, 1)), (2, op(1, 1))])
        .unwrap();
    storage
        .record_rebuilt_counts(&[(key(0, 0), count(41)), (key(2, 0), count(42))])
        .unwrap();
    let prefix = std::path::Path::new("/snapshot/era");
    // The era must span exactly the executed rows: base 41, end 43.
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
            &EraRecord {
                first_queued_input: 3,
                next_batch_nonce: 2,
                ..era(41, 43, 9)
            },
        )
        .unwrap();
    assert_eq!(storage.history_state().unwrap().rebuilt_history_end, 43);

    let sealed = "rebuilt history is sealed once its era is published";
    let error = storage
        .append_rebuilt_history(&[(2, op(2, 1))])
        .expect_err("no row after the era is published");
    assert_eq!(error.to_string(), sealed);
    let error = storage
        .record_rebuilt_counts(&[(key(2, 1), count(43))])
        .expect_err("no count after the era is published");
    assert_eq!(
        error.to_string(),
        "a rebuilt row only gains its count, once, before its era is published"
    );
    let error = storage
        .conn
        .execute("DELETE FROM rebuilt_history", [])
        .expect_err("rows are never deleted");
    assert_eq!(
        error.to_string(),
        "rebuilt history is retained for the database lifetime"
    );
    assert_eq!(storage.rebuilt_history_after(None, 10).unwrap().len(), 3);
}

/// An era whose rebuilt direct has no count would drop it from the feed.
#[test]
fn an_era_with_an_uncounted_direct_is_refused() {
    let (_db, mut storage) = setup_storage("rebuilt-uncounted-direct");
    storage
        .append_rebuilt_history(&[(0, direct(0, 0)), (2, direct(2, 1))])
        .unwrap();
    storage
        .record_rebuilt_counts(&[(key(0, 0), count(41))])
        .unwrap();
    let error = storage
        .publish_era(
            std::path::Path::new("/snapshot/era"),
            &EraRecord {
                first_queued_input: 3,
                ..era(41, 42, 9)
            },
        )
        .expect_err("a rebuilt direct always executes");
    assert!(
        error
            .to_string()
            .contains("a rebuilt direct always executes"),
        "{error}"
    );
    assert!(!storage.is_setup_complete().unwrap());
}

/// An era that still queues a direct the rebuild executed would make run's
/// root frame execute it again.
#[test]
fn an_era_queueing_a_rebuilt_direct_is_refused() {
    let (_db, mut storage) = setup_storage("rebuilt-direct-queued");
    storage
        .append_rebuilt_history(&[(0, direct(0, 0)), (2, direct(2, 1))])
        .unwrap();
    storage
        .record_rebuilt_counts(&[(key(0, 0), count(41)), (key(2, 0), count(42))])
        .unwrap();
    let prefix = std::path::Path::new("/snapshot/era");
    let error = storage
        .publish_era(
            prefix,
            &EraRecord {
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
            &EraRecord {
                first_queued_input: 3,
                ..era(41, 43, 9)
            },
        )
        .unwrap();
}
