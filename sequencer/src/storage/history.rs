// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The era (one immutable row): the history's whole identity, and where its
//! history starts and its live rows begin.

use rusqlite::{Connection, OptionalExtension, Result, Transaction, params, types::Type};
use sequencer_core::history::{EraId, L2TxCount};

use super::convert::{i64_to_u64, now_unix_ms, u64_to_i64};

mod canonical;
#[cfg(test)]
pub(crate) use canonical::ApplicationInputRow;
pub(super) use canonical::history_bounds_from;
pub(crate) use canonical::{HistoryReadError, L2TxContext};

/// The published era, as read. Immutable for the database's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Era {
    pub id: EraId,
    pub published_at_ms: u64,
    /// Where the era's history starts.
    pub base: L2TxCount,
    /// Where the live rows start: the baseline's count.
    pub rebuilt_end: L2TxCount,
    /// The L1 prefix the era's baseline accounts for.
    pub stop_block: u64,
    /// The oldest direct still queued at the stop block, or the input count
    /// through it when none is: where the era's first frame drains from.
    pub first_queued_input: u64,
    /// The nonce the era's first batch carries (I16): the
    /// scheduler's next batch nonce at the stop block.
    pub anchor_batch_nonce: u64,
}

/// What setup publishes: an [`Era`] without the id and time storage assigns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewEra {
    pub base: L2TxCount,
    pub rebuilt_end: L2TxCount,
    pub stop_block: u64,
    pub first_queued_input: u64,
    pub anchor_batch_nonce: u64,
}

/// One direct a drained range executed, at its history offset. A drained
/// range may hold inputs without one: the batch submitter's own inputs and
/// directs over the application's payload cap never execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectInputExecution {
    pub safe_input_index: u64,
    pub l2_tx_index: L2TxCount,
}

const ERA_SQL: &str = "SELECT era_id, published_at_ms, base_l2_tx_count, rebuilt_history_end,
            stop_block, first_queued_input, anchor_batch_nonce
       FROM era WHERE singleton_id = 0";

fn decode_era_id(row: &rusqlite::Row<'_>, index: usize) -> Result<EraId> {
    let blob = row.get::<_, Vec<u8>>(index)?;
    EraId::try_from(blob.as_slice()).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Blob, Box::new(error))
    })
}

fn decode_era(row: &rusqlite::Row<'_>) -> Result<Era> {
    Ok(Era {
        id: decode_era_id(row, 0)?,
        published_at_ms: i64_to_u64(row.get(1)?),
        base: L2TxCount::new(i64_to_u64(row.get(2)?)),
        rebuilt_end: L2TxCount::new(i64_to_u64(row.get(3)?)),
        stop_block: i64_to_u64(row.get(4)?),
        first_queued_input: i64_to_u64(row.get(5)?),
        anchor_batch_nonce: i64_to_u64(row.get(6)?),
    })
}

/// The published era. Fails with `QueryReturnedNoRows` before setup
/// publishes: every caller runs inside a set-up database.
pub(super) fn era_in(conn: &Connection) -> Result<Era> {
    conn.query_row(ERA_SQL, [], decode_era)
}

/// The published era, or `None` before setup publishes it. Only for setup's
/// own appends, to which "no era yet" is a real state.
pub(super) fn find_era_in(conn: &Connection) -> Result<Option<Era>> {
    conn.query_row(ERA_SQL, [], decode_era).optional()
}

/// Insert the era under a fresh v4 era id, stamped now. The schema checks
/// the counts against the executed rebuilt rows, seals them, and refuses a
/// second era. Used by `publish_era` and the test fixture `open_with_era`.
pub(super) fn insert_era_in(tx: &Transaction<'_>, era: &NewEra) -> Result<()> {
    let mut bytes: [u8; EraId::BYTE_LEN] =
        tx.query_row("SELECT randomblob(16)", [], |row| row.get(0))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let era_id = EraId::from_bytes(bytes).expect("UUID bits were set above");
    tx.execute(
        "INSERT INTO era
         (singleton_id, era_id, published_at_ms, base_l2_tx_count, rebuilt_history_end,
          stop_block, first_queued_input, anchor_batch_nonce)
         VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            era_id.as_bytes().as_slice(),
            now_unix_ms(),
            u64_to_i64(era.base.get()),
            u64_to_i64(era.rebuilt_end.get()),
            u64_to_i64(era.stop_block),
            u64_to_i64(era.first_queued_input),
            u64_to_i64(era.anchor_batch_nonce),
        ],
    )?;
    Ok(())
}

/// The next history offset: one past the last live row, or the rebuilt
/// history's end before the era has any.
pub(super) fn next_l2_tx_count_in(conn: &Connection) -> Result<L2TxCount> {
    next_l2_tx_count_of(conn, &era_in(conn)?)
}

/// [`next_l2_tx_count_in`] for a caller that already read the era.
pub(super) fn next_l2_tx_count_of(conn: &Connection, era: &Era) -> Result<L2TxCount> {
    let rebuilt_end = era.rebuilt_end;
    let greatest: Option<i64> = conn.query_row(
        "SELECT MAX(l2_tx_count) FROM application_inputs",
        [],
        |row| row.get(0),
    )?;
    Ok(greatest.map_or(rebuilt_end, |offset| {
        L2TxCount::new(
            i64_to_u64(offset)
                .checked_add(1)
                .expect("application input count overflow"),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::era;
    use crate::storage::{LifecycleCommand, Storage};
    use crate::test_support::temp_db;

    #[test]
    fn the_era_is_one_immutable_row_that_survives_restart() {
        let db = temp_db("history-baseline");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
        assert!(matches!(
            storage.era(),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        assert_eq!(storage.read(|tx| find_era_in(tx)).unwrap(), None);
        let mismatched = storage
            .write(|tx| insert_era_in(tx, &era(41, 42, 70)))
            .expect_err("an empty rebuild cannot end past its base");
        assert_eq!(
            mismatched.to_string(),
            "the era must span exactly its executed rebuilt rows"
        );
        let past_stop = storage
            .write(|tx| {
                insert_era_in(
                    tx,
                    &NewEra {
                        first_queued_input: 1,
                        ..era(41, 41, 70)
                    },
                )
            })
            .expect_err("no input exists through the stop block");
        assert_eq!(
            past_stop.to_string(),
            "the first queued input lies past the stop block"
        );
        storage
            .write(|tx| {
                insert_era_in(
                    tx,
                    &NewEra {
                        anchor_batch_nonce: 9,
                        ..era(41, 41, 70)
                    },
                )
            })
            .unwrap();
        let published = storage.era().unwrap();
        assert_eq!(
            (
                published.base,
                published.rebuilt_end,
                published.stop_block,
                published.first_queued_input,
                published.anchor_batch_nonce
            ),
            (L2TxCount::new(41), L2TxCount::new(41), 70, 0, 9)
        );
        assert_eq!(storage.read(|tx| find_era_in(tx)).unwrap(), Some(published));
        let duplicate = storage
            .write(|tx| insert_era_in(tx, &era(41, 41, 70)))
            .expect_err("even identical values cannot publish another era");
        assert_eq!(
            duplicate.to_string(),
            "the era is inserted once per database"
        );
        for (sql, expected) in [
            (
                "INSERT OR REPLACE INTO era (singleton_id, era_id, published_at_ms, \
                 base_l2_tx_count, rebuilt_history_end, stop_block, first_queued_input, \
                 anchor_batch_nonce) \
                 VALUES (0, X'00000000000040008000000000000001', 0, 41, 41, 70, 0, 9)",
                "the era is inserted once per database",
            ),
            ("UPDATE era SET singleton_id = 0", "the era is immutable"),
            (
                "UPDATE era SET era_id = X'00000000000040008000000000000001'",
                "the era is immutable",
            ),
            ("UPDATE era SET published_at_ms = 1", "the era is immutable"),
            (
                "UPDATE era SET base_l2_tx_count = 42",
                "the era is immutable",
            ),
            (
                "UPDATE era SET rebuilt_history_end = 42",
                "the era is immutable",
            ),
            ("UPDATE era SET stop_block = 71", "the era is immutable"),
            (
                "UPDATE era SET first_queued_input = 1",
                "the era is immutable",
            ),
            (
                "UPDATE era SET anchor_batch_nonce = 10",
                "the era is immutable",
            ),
            (
                "DELETE FROM era",
                "the era is retained for the database lifetime",
            ),
        ] {
            let error = storage.conn.execute(sql, []).expect_err(sql);
            assert_eq!(error.to_string(), expected, "{sql}");
        }
        drop(storage);
        let mut reopened = Storage::open(&db.path).unwrap();
        assert_eq!(reopened.era().unwrap(), published);
        assert_eq!(reopened.next_l2_tx_count().unwrap().get(), 41);
    }
}
