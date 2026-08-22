// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Current-era history metadata persisted as one SQLite singleton.

use rusqlite::{Connection, Result, types::Type};
use sequencer_core::history::{EraId, ExecutedInputCount, HistoryVersion, RecoveryGeneration};

use super::Storage;
use super::convert::{i64_to_u64, u64_to_i64};

/// Durable metadata for the history served by this database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryState {
    pub version: HistoryVersion,
    pub era_created_at_ms: u64,
    /// `None` only while an admitted cockroach rebuild has not yet registered
    /// its recovered finalized application state.
    pub base_executed_input_count: Option<u64>,
    /// Exclusive `safe_inputs` cursor below which this era must never drain.
    /// `None` has the same narrow pre-fill rebuild meaning as the application
    /// base above; the two fields bind atomically.
    pub base_safe_input_index: Option<u64>,
}

/// One sparse attribution from SQLite's physical replay log to the canonical
/// application-history coordinate consumed by that row.
///
/// Physical rows that do not execute in the application (our own submitted
/// batches and cockroach-root padding) deliberately have no mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExecutedInputMapping {
    pub sequenced_l2_tx_offset: u64,
    pub executed_input_offset: ExecutedInputCount,
}

/// One safe input from a drained physical range that actually executed in the
/// application. The range may also contain intentionally-unmapped rows, so the
/// caller supplies only these sparse attributions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectInputExecution {
    pub safe_input_index: u64,
    pub executed_input_offset: ExecutedInputCount,
}

impl Storage {
    /// Read the current era, recovery generation, and locally available base.
    pub fn history_state(&self) -> Result<HistoryState> {
        query_history_state(&self.conn)
    }

    /// Boundary before the next canonical application input executes.
    ///
    /// This is derived, never independently advanced: the maximum of the era's
    /// recovered base and one past the greatest valid execution attribution.
    /// Invalidating a suffix therefore rolls the value back automatically.
    pub fn next_executed_input_count(&mut self) -> Result<ExecutedInputCount> {
        self.read(|tx| next_executed_input_count_in(tx))
    }
}

pub(super) fn query_history_state(conn: &Connection) -> Result<HistoryState> {
    conn.query_row(
        "SELECT era_id, era_created_at_ms, recovery_generation, \
                base_executed_input_count, base_safe_input_index \
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
                base_executed_input_count: row.get::<_, Option<i64>>(3)?.map(i64_to_u64),
                base_safe_input_index: row.get::<_, Option<i64>>(4)?.map(i64_to_u64),
            })
        },
    )
}

/// Derive the next canonical application coordinate from durable facts.
pub(super) fn next_executed_input_count_in(conn: &Connection) -> Result<ExecutedInputCount> {
    let base = query_history_state(conn)?
        .base_executed_input_count
        .expect("application history base is unbound outside rebuild fill");
    let greatest_valid: Option<i64> = conn.query_row(
        "SELECT MAX(executed_input_offset) FROM executed_inputs",
        [],
        |row| row.get(0),
    )?;
    let after_valid = greatest_valid.map_or(0, |offset| {
        i64_to_u64(offset)
            .checked_add(1)
            .expect("executed input offset overflow: contract-impossible")
    });
    Ok(ExecutedInputCount::new(base.max(after_valid)))
}

/// Attach a sequence of explicit application offsets inside the physical-row
/// creation transaction. Each offset must equal the currently-derived next
/// boundary; the baseline schema independently enforces the same rule over the
/// valid projection, including offset reuse after suffix invalidation.
pub(super) fn attach_executed_inputs_in(
    tx: &rusqlite::Transaction<'_>,
    mappings: &[ExecutedInputMapping],
) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }

    let mut expected = next_executed_input_count_in(tx)?;
    let mut stmt = tx.prepare_cached(
        "INSERT INTO executed_inputs \
         (sequenced_l2_tx_offset, executed_input_offset) VALUES (?1, ?2)",
    )?;
    for mapping in mappings {
        assert_eq!(
            mapping.executed_input_offset, expected,
            "executed input attribution does not match canonical next count"
        );
        stmt.execute(rusqlite::params![
            u64_to_i64(mapping.sequenced_l2_tx_offset),
            u64_to_i64(mapping.executed_input_offset.get()),
        ])?;
        expected = expected
            .checked_next()
            .expect("executed input count overflow: contract-impossible");
    }
    Ok(())
}

/// Bind the era's locally-available application base and durable safe-input
/// drain floor to the snapshot that establishes them. Genesis is already
/// initialized to zero by the baseline migration; rebuild starts with both
/// `NULL` and reaches this function with the folded application's absolute
/// count plus the recovery root's exclusive safe-input cursor.
pub(super) fn bind_history_base_in(
    tx: &rusqlite::Transaction<'_>,
    base_executed_input_count: u64,
    base_safe_input_index: u64,
) -> Result<()> {
    let current = query_history_state(tx)?;
    match (
        current.base_executed_input_count,
        current.base_safe_input_index,
    ) {
        (Some(current_count), Some(current_safe_input_index)) => {
            assert_eq!(
                current_count, base_executed_input_count,
                "history base differs from the initial finalized application state"
            );
            assert_eq!(
                current_safe_input_index, base_safe_input_index,
                "safe-input floor differs from the initial finalized application state"
            );
            return Ok(());
        }
        (None, None) => {}
        _ => unreachable!("history base pair cannot be partially bound"),
    }

    let changed = tx.execute(
        "UPDATE history_state \
         SET base_executed_input_count = ?1, base_safe_input_index = ?2 \
         WHERE singleton_id = 0 \
           AND base_executed_input_count IS NULL \
           AND base_safe_input_index IS NULL",
        rusqlite::params![
            u64_to_i64(base_executed_input_count),
            u64_to_i64(base_safe_input_index)
        ],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

/// Durable lower bound for safe-input draining. A NULL floor is usable as zero
/// only while a rebuild's setup has not completed: that is the one interval
/// in which the recovery root must be populated before its cursor can be
/// bound. Everywhere else NULL is a storage invariant violation and fails
/// loud.
pub(super) fn safe_input_floor_in(conn: &Connection) -> Result<u64> {
    let state = query_history_state(conn)?;
    match state.base_safe_input_index {
        Some(floor) => Ok(floor),
        None => {
            assert!(
                state.base_executed_input_count.is_none(),
                "history base pair cannot be partially bound"
            );
            // A NULL floor exists only during a pre-completion rebuild
            // fill: plain setup binds base 0 in its baseline transaction, and
            // completion refuses while the base is NULL — so the completion
            // fact alone decides legality (the journal is never read for
            // decisions; L2).
            let setup_complete: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM setup_complete WHERE singleton_id = 0)",
                [],
                |row| row.get(0),
            )?;
            assert!(
                !setup_complete,
                "NULL safe-input floor outside pre-completion rebuild fill"
            );
            Ok(0)
        }
    }
}

/// Advance the current era's soft-history reality by exactly one. The schema
/// independently rejects skips and rewrites; the caller composes this helper
/// into the same transaction as suffix invalidation and Tip reopening.
pub(super) fn advance_recovery_generation_in(
    tx: &rusqlite::Transaction<'_>,
) -> Result<RecoveryGeneration> {
    let current: i64 = tx.query_row(
        "SELECT recovery_generation FROM history_state WHERE singleton_id = 0",
        [],
        |row| row.get(0),
    )?;
    let next = current
        .checked_add(1)
        .expect("recovery generation exhausted SQLite INTEGER: contract-impossible");
    let changed = tx.execute(
        "UPDATE history_state SET recovery_generation = ?1 WHERE singleton_id = 0",
        [next],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(RecoveryGeneration::new(i64_to_u64(next)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::temp_db;
    use crate::storage::{LifecycleCommand, Storage};

    #[test]
    fn history_schema_enforces_write_once_identity_and_base() {
        let db = temp_db("history-write-once");
        let storage = Storage::open(db.path.as_str()).expect("initialize generic history");
        let original = storage.history_state().expect("read history");
        drop(storage);

        let conn = Storage::open_connection(db.path.as_str()).expect("open raw connection");
        assert!(
            conn.execute(
                "UPDATE history_state SET era_id = era_id WHERE singleton_id = 0",
                [],
            )
            .is_err(),
            "era identity must reject even a same-value rewrite"
        );
        assert!(
            conn.execute(
                "UPDATE history_state SET base_executed_input_count = 1 \
                 WHERE singleton_id = 0",
                [],
            )
            .is_err(),
            "the initialized genesis base must be immutable"
        );
        assert!(
            conn.execute(
                "UPDATE history_state SET base_safe_input_index = 1 \
                 WHERE singleton_id = 0",
                [],
            )
            .is_err(),
            "the initialized genesis safe-input floor must be immutable"
        );
        assert!(
            conn.execute("DELETE FROM history_state WHERE singleton_id = 0", [])
                .is_err(),
            "the current era singleton must not be deletable"
        );

        let reopened = Storage::open_read_only(db.path.as_str()).expect("reopen");
        assert_eq!(reopened.history_state().expect("read history"), original);
    }

    #[test]
    fn rebuild_base_is_set_once_and_generation_advances_only_by_one() {
        let db = temp_db("history-rebuild-transitions");
        let storage = Storage::initialize_for_command(db.path.as_str(), LifecycleCommand::Rebuild)
            .expect("initialize rebuild");
        assert_eq!(
            storage
                .history_state()
                .expect("read pending rebuild")
                .base_executed_input_count,
            None
        );
        assert_eq!(
            storage
                .history_state()
                .expect("read pending rebuild")
                .base_safe_input_index,
            None
        );
        drop(storage);

        let conn = Storage::open_connection(db.path.as_str()).expect("open raw connection");
        assert!(
            conn.execute(
                "UPDATE history_state SET base_executed_input_count = 41 \
                 WHERE singleton_id = 0",
                [],
            )
            .is_err(),
            "the application base cannot bind without its safe-input floor"
        );
        conn.execute(
            "UPDATE history_state \
             SET base_executed_input_count = 41, base_safe_input_index = 7 \
             WHERE singleton_id = 0",
            [],
        )
        .expect("set rebuild base pair once");
        assert!(
            conn.execute(
                "UPDATE history_state SET base_executed_input_count = 42 \
                 WHERE singleton_id = 0",
                [],
            )
            .is_err(),
            "rebuild base must not be rewritten"
        );
        assert!(
            conn.execute(
                "UPDATE history_state SET base_safe_input_index = 8 \
                 WHERE singleton_id = 0",
                [],
            )
            .is_err(),
            "rebuild safe-input floor must not be rewritten"
        );

        conn.execute(
            "UPDATE history_state SET recovery_generation = 1 WHERE singleton_id = 0",
            [],
        )
        .expect("advance generation by one");
        assert!(
            conn.execute(
                "UPDATE history_state SET recovery_generation = 3 WHERE singleton_id = 0",
                [],
            )
            .is_err(),
            "generation must not skip"
        );
        conn.execute(
            "UPDATE history_state SET recovery_generation = 2 WHERE singleton_id = 0",
            [],
        )
        .expect("advance generation by one again");

        let reopened = Storage::open_read_only(db.path.as_str()).expect("reopen");
        let state = reopened.history_state().expect("read history");
        assert_eq!(state.base_executed_input_count, Some(41));
        assert_eq!(state.base_safe_input_index, Some(7));
        assert_eq!(
            state.version.recovery_generation,
            RecoveryGeneration::new(2)
        );
    }
}
