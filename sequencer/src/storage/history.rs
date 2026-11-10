// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The era (one immutable row) and the preserved prefix at each recovery
//! generation; the generation is the greatest cut.

use rusqlite::{Connection, OptionalExtension, Result, Transaction, params, types::Type};
use sequencer_core::history::{EraId, HistoryVersion, L2TxCount, RecoveryGeneration};

use super::convert::{i64_to_u64, now_unix_ms, u64_to_i64};

mod canonical;
#[cfg(test)]
pub(crate) use canonical::ApplicationInputRow;
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
    /// The nonce the batch tree's parentless root carries (I16): the
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

impl NewEra {
    /// An era at genesis that rebuilt nothing, with stop block 0 and anchor 0:
    /// what the crate-test open path and seeded harnesses publish.
    pub const EMPTY_GENESIS: Self = Self {
        base: L2TxCount::ZERO,
        rebuilt_end: L2TxCount::ZERO,
        stop_block: 0,
        first_queued_input: 0,
        anchor_batch_nonce: 0,
    };
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

/// The era id and recovery generation, read in one statement. The generation
/// is the greatest cut (0 with none): no other counter exists.
pub(super) fn history_version_in(conn: &Connection) -> Result<HistoryVersion> {
    conn.query_row(
        "SELECT era_id,
                (SELECT COALESCE(MAX(recovery_generation), 0) FROM history_generation_cuts)
           FROM era WHERE singleton_id = 0",
        [],
        |row| {
            Ok(HistoryVersion {
                era_id: decode_era_id(row, 0)?,
                recovery_generation: RecoveryGeneration::new(i64_to_u64(row.get(1)?)),
            })
        },
    )
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

/// Record the next recovery generation's cut. Called after suffix deletion
/// and before the replacement Tip attributes directs; the cut is the new
/// generation, since the generation is the greatest cut.
pub(super) fn advance_recovery_generation_in(tx: &Transaction<'_>) -> Result<RecoveryGeneration> {
    let current = history_version_in(tx)?.recovery_generation.get();
    let next = current
        .checked_add(1)
        .expect("recovery generation exhausted");
    let preserved = next_l2_tx_count_in(tx)?;
    tx.execute(
        "INSERT INTO history_generation_cuts (recovery_generation, preserved_l2_tx_count) \
         VALUES (?1, ?2)",
        params![u64_to_i64(next), u64_to_i64(preserved.get())],
    )?;
    Ok(RecoveryGeneration::new(next))
}

pub(super) fn preserved_input_count_in(
    conn: &Connection,
    from: RecoveryGeneration,
    current: RecoveryGeneration,
    head: L2TxCount,
) -> Result<L2TxCount> {
    assert!(
        from <= current,
        "compatibility starts after the current generation"
    );
    if from == current {
        return Ok(head);
    }
    let (count, minimum): (i64, Option<i64>) = conn.query_row(
        "SELECT COUNT(*), MIN(preserved_l2_tx_count) FROM history_generation_cuts \
         WHERE recovery_generation > ?1 AND recovery_generation <= ?2",
        params![u64_to_i64(from.get()), u64_to_i64(current.get())],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    // Unique integer generations plus the exact interval length prove that
    // every intervening recovery contributed its cut, including empty batches.
    assert_eq!(
        i64_to_u64(count),
        current.get() - from.get(),
        "history generation lineage is incomplete"
    );
    let minimum = minimum.expect("a nonempty complete generation interval has a minimum");
    Ok(head.min(L2TxCount::new(i64_to_u64(minimum))))
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

    #[test]
    fn the_generation_is_the_greatest_cut_and_advances_one_at_a_time() {
        let db = temp_db("history-generation");
        let mut storage = Storage::open(&db.path).unwrap();
        let original = storage.history_version().unwrap();
        assert_eq!(original.recovery_generation.get(), 0);
        let result: Result<()> = storage.write(|tx| {
            advance_recovery_generation_in(tx)?;
            Err(rusqlite::Error::InvalidQuery)
        });
        assert!(result.is_err());
        assert_eq!(storage.history_version().unwrap(), original);
        for expected in [1, 2] {
            assert_eq!(
                storage.write(advance_recovery_generation_in).unwrap(),
                RecoveryGeneration::new(expected)
            );
            let version = storage.history_version().unwrap();
            assert_eq!(version.era_id, original.era_id);
            assert_eq!(version.recovery_generation.get(), expected);
        }
        for sql in [
            "INSERT INTO history_generation_cuts VALUES (4, 0)",
            "INSERT INTO history_generation_cuts VALUES (2, 0)",
        ] {
            let error = storage.conn.execute(sql, []).expect_err(sql);
            assert_eq!(
                error.to_string(),
                "a cut advances the recovery generation by exactly one, within an era",
                "{sql}"
            );
        }
    }

    #[test]
    fn a_cut_without_an_era_aborts() {
        let db = temp_db("history-generation-no-era");
        let storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
        let error = storage
            .conn
            .execute("INSERT INTO history_generation_cuts VALUES (1, 0)", [])
            .expect_err("no era");
        assert_eq!(
            error.to_string(),
            "a cut advances the recovery generation by exactly one, within an era"
        );
    }
}

#[cfg(test)]
mod generation_tests;
