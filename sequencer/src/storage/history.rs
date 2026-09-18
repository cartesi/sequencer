// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Immutable era baseline and the preserved prefix at each recovery generation.

#[cfg(test)]
use rusqlite::OptionalExtension;
use rusqlite::{Connection, Result, Transaction, params, types::Type};
use sequencer_core::history::{EraId, ExecutedInputCount, HistoryVersion, RecoveryGeneration};

use super::Storage;
use super::convert::{i64_to_u64, now_unix_ms, u64_to_i64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryState {
    pub version: HistoryVersion,
    pub era_created_at_ms: u64,
    pub base_executed_input_count: u64,
    /// L1 prefix already accounted for by the era's initial application state.
    pub base_safe_block: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectInputExecution {
    pub safe_input_index: u64,
    pub executed_input_offset: ExecutedInputCount,
}

impl Storage {
    pub fn history_state(&self) -> Result<HistoryState> {
        query_history_state(&self.conn)
    }

    pub fn next_executed_input_count(&mut self) -> Result<ExecutedInputCount> {
        self.read(|tx| next_executed_input_count_in(tx))
    }
}

pub(super) fn query_history_state(conn: &Connection) -> Result<HistoryState> {
    conn.query_row(
        "SELECT era_id, era_created_at_ms, recovery_generation,
                base_executed_input_count, base_safe_block
           FROM history_state WHERE singleton_id = 0",
        [],
        |row| {
            let era_blob = row.get::<_, Vec<u8>>(0)?;
            let era_id = EraId::try_from(era_blob.as_slice()).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(0, Type::Blob, Box::new(error))
            })?;
            Ok(HistoryState {
                version: HistoryVersion {
                    era_id,
                    recovery_generation: RecoveryGeneration::new(i64_to_u64(row.get(2)?)),
                },
                era_created_at_ms: i64_to_u64(row.get(1)?),
                base_executed_input_count: i64_to_u64(row.get(3)?),
                base_safe_block: i64_to_u64(row.get(4)?),
            })
        },
    )
}

pub(super) fn initialize_history_in(
    tx: &Transaction<'_>,
    base: ExecutedInputCount,
    base_safe_block: u64,
) -> Result<()> {
    #[cfg(test)]
    if let Some(existing) = query_history_state(tx).optional()? {
        // Test fixtures initialize genesis when opening their schema. Production
        // creates this row only with the complete durable baseline.
        assert_eq!(
            existing.base_executed_input_count,
            base.get(),
            "history base differs"
        );
        assert_eq!(
            existing.base_safe_block, base_safe_block,
            "L1 prefix differs"
        );
        return Ok(());
    }
    let mut bytes: [u8; EraId::BYTE_LEN] =
        tx.query_row("SELECT randomblob(16)", [], |row| row.get(0))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let era = EraId::from_bytes(bytes).expect("UUID bits were set above");
    tx.execute(
        "INSERT INTO history_state
         (singleton_id, era_id, era_created_at_ms, recovery_generation,
          base_executed_input_count, base_safe_block) VALUES (0, ?1, ?2, 0, ?3, ?4)",
        params![
            era.as_bytes().as_slice(),
            now_unix_ms(),
            u64_to_i64(base.get()),
            u64_to_i64(base_safe_block)
        ],
    )?;
    Ok(())
}

pub(super) fn next_executed_input_count_in(conn: &Connection) -> Result<ExecutedInputCount> {
    let base = query_history_state(conn)?.base_executed_input_count;
    let greatest: Option<i64> =
        conn.query_row("SELECT MAX(offset) FROM application_inputs", [], |row| {
            row.get(0)
        })?;
    Ok(ExecutedInputCount::new(greatest.map_or(base, |offset| {
        i64_to_u64(offset)
            .checked_add(1)
            .expect("application input count overflow")
    })))
}

/// Called after suffix deletion and before the replacement Tip attributes directs.
pub(super) fn advance_recovery_generation_in(tx: &Transaction<'_>) -> Result<RecoveryGeneration> {
    let current = query_history_state(tx)?.version.recovery_generation.get();
    let next = current
        .checked_add(1)
        .expect("recovery generation exhausted");
    let preserved = next_executed_input_count_in(tx)?;
    tx.execute(
        "INSERT INTO history_generation_cuts (recovery_generation, preserved_input_count) \
         VALUES (?1, ?2)",
        params![u64_to_i64(next), u64_to_i64(preserved.get())],
    )?;
    let changed = tx.execute(
        "UPDATE history_state SET recovery_generation = ?1 WHERE singleton_id = 0",
        [u64_to_i64(next)],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(RecoveryGeneration::new(next))
}

pub(super) fn preserved_input_count_in(
    conn: &Connection,
    from: RecoveryGeneration,
    current: RecoveryGeneration,
    head: ExecutedInputCount,
) -> Result<ExecutedInputCount> {
    assert!(
        from <= current,
        "compatibility starts after the current generation"
    );
    if from == current {
        return Ok(head);
    }
    let (count, minimum): (i64, Option<i64>) = conn.query_row(
        "SELECT COUNT(*), MIN(preserved_input_count) FROM history_generation_cuts \
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
    Ok(head.min(ExecutedInputCount::new(i64_to_u64(minimum))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{LifecycleCommand, test_helpers::temp_db};

    #[test]
    fn complete_baseline_is_immutable_and_survives_restart() {
        let db = temp_db("history-baseline");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
        assert!(matches!(
            storage.history_state(),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        storage
            .write(|tx| initialize_history_in(tx, ExecutedInputCount::new(41), 70))
            .unwrap();
        let state = storage.history_state().unwrap();
        for sql in [
            "UPDATE history_state SET era_id = era_id",
            "UPDATE history_state SET base_executed_input_count = 42",
            "UPDATE history_state SET base_safe_block = 71",
            "DELETE FROM history_state",
        ] {
            assert!(storage.conn.execute(sql, []).is_err(), "{sql}");
        }
        drop(storage);
        let mut reopened = Storage::open(&db.path).unwrap();
        assert_eq!(reopened.history_state().unwrap(), state);
        assert_eq!(reopened.next_executed_input_count().unwrap().get(), 41);
    }

    #[test]
    fn generation_advances_exactly_once_and_transaction_rollback_preserves_it() {
        let db = temp_db("history-generation");
        let mut storage = Storage::open(&db.path).unwrap();
        let original = storage.history_state().unwrap();
        let result: Result<()> = storage.write(|tx| {
            advance_recovery_generation_in(tx)?;
            Err(rusqlite::Error::InvalidQuery)
        });
        assert!(result.is_err());
        assert_eq!(storage.history_state().unwrap(), original);
        storage.write(advance_recovery_generation_in).unwrap();
        assert_eq!(
            storage
                .history_state()
                .unwrap()
                .version
                .recovery_generation
                .get(),
            1
        );
        assert!(
            storage
                .conn
                .execute("UPDATE history_state SET recovery_generation = 3", [])
                .is_err()
        );
        assert!(
            storage
                .conn
                .execute("UPDATE history_state SET recovery_generation = 0", [])
                .is_err()
        );
    }
}

#[cfg(test)]
mod generation_tests;
