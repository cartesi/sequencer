// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Command-admission facts and the `terminal_faults` black box: an append-only,
//! best-effort record of terminal causes for operators that nothing branches on.
//! Admission design: authority-boundary ADR, mechanism 2.

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use super::Storage;
use super::convert::{i64_to_u64, now_unix_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleCommand {
    Setup,
    Run,
    MaintenanceFlush,
}

impl LifecycleCommand {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Run => "run",
            Self::MaintenanceFlush => "maintenance_flush",
        }
    }

    fn parse(value: &str) -> Result<Self, LifecycleError> {
        match value {
            "setup" => Ok(Self::Setup),
            "run" => Ok(Self::Run),
            "maintenance_flush" => Ok(Self::MaintenanceFlush),
            other => Err(LifecycleError::Malformed(format!(
                "unknown lifecycle command {other:?}"
            ))),
        }
    }
}

impl std::fmt::Display for LifecycleCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One black-box row: which command died terminal, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFault {
    pub command: LifecycleCommand,
    pub cause: String,
    pub recorded_at_ms: u64,
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("lifecycle storage failed: {0}")]
    Storage(#[from] rusqlite::Error),
    /// A lifecycle contract was violated: a malformed black-box row, an
    /// empty terminal cause, or setup completion attempted before its
    /// preconditions exist.
    #[error("lifecycle contract violated: {0}")]
    Malformed(String),
    #[error("{requested} is not admissible: {reason}")]
    NotAdmissible {
        requested: LifecycleCommand,
        reason: &'static str,
    },
    #[error(
        "canonical divergence at batch nonce {nonce}; only cockroach recovery (fresh-directory rebuild) can proceed"
    )]
    CanonicalDivergence { nonce: u64 },
}

impl Storage {
    /// Publish the new era with its durable baseline dump, in one transaction:
    /// the era record, the anchor, the baseline snapshot at the rebuilt
    /// history's end and the setup-complete marker. Setup opens no Tip;
    /// `run`'s startup roots the batch tree at the anchor.
    pub(crate) fn publish_era(
        &mut self,
        baseline_prefix: &std::path::Path,
        era: &super::EraRecord,
    ) -> Result<(), LifecycleError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        refuse_on_canonical_divergence(&tx)?;
        require_command_fits_completion(&tx, LifecycleCommand::Setup)?;
        let has_batch: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM batches)", [], |row| {
            row.get(0)
        })?;
        if has_batch {
            return Err(LifecycleError::Malformed(
                "an era is published before any batch exists".to_string(),
            ));
        }
        super::history::initialize_history_in(&tx, era)?;
        super::snapshot_dumps::insert_baseline_snapshot_in(
            &tx,
            baseline_prefix,
            era.rebuilt_history_end,
        )?;
        tx.execute(
            "INSERT INTO setup_complete (singleton_id, completed_at_ms) VALUES (0, ?1)",
            [now_unix_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The admission facts, checked read-only before a command does any
    /// preparatory work: divergence is absorbing, and the two-sided
    /// completion rule orders commands.
    pub(crate) fn preflight_lifecycle_command(
        &self,
        command: LifecycleCommand,
    ) -> Result<(), LifecycleError> {
        refuse_on_canonical_divergence(&self.conn)?;
        require_command_fits_completion(&self.conn, command)
    }

    /// Best-effort terminal-cause record, written by the command bracket at
    /// settlement. Deliberately gated on nothing — not even divergence —
    /// because a refused write must never be the reason a cause goes
    /// unrecorded. Restart policy is the exit-code contract, not this row.
    pub(crate) fn record_terminal_fault(
        &mut self,
        command: LifecycleCommand,
        cause: &str,
    ) -> Result<(), LifecycleError> {
        if cause.is_empty() {
            return Err(LifecycleError::Malformed(
                "terminal cause must not be empty".to_string(),
            ));
        }
        self.conn.execute(
            "INSERT INTO terminal_faults (command, cause, recorded_at_ms) \
             VALUES (?1, ?2, ?3)",
            params![command.as_str(), cause, now_unix_ms()],
        )?;
        Ok(())
    }

    /// The most recent black-box row, or `None` when no terminal fault was
    /// ever recorded. Read once at `run` startup for a warning line and by
    /// operators for postmortems; nothing branches on it.
    pub fn latest_terminal_fault(&self) -> Result<Option<TerminalFault>, LifecycleError> {
        let row = self
            .conn
            .query_row(
                "SELECT command, cause, recorded_at_ms FROM terminal_faults \
                 ORDER BY fault_id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((command, cause, recorded_at_ms)) = row else {
            return Ok(None);
        };
        if cause.is_empty() {
            return Err(LifecycleError::Malformed(
                "terminal-fault row without cause".to_string(),
            ));
        }
        Ok(Some(TerminalFault {
            command: LifecycleCommand::parse(&command)?,
            cause,
            recorded_at_ms: u64::try_from(recorded_at_ms).map_err(|_| {
                LifecycleError::Malformed(format!(
                    "terminal-fault recorded_at_ms {recorded_at_ms} is negative"
                ))
            })?,
        }))
    }
}

/// The two-sided completion rule — the one command-ordering fact: setup
/// never restarts over a completed setup (completion is once per database),
/// run and maintenance never start before one exists. Run and maintenance
/// reach it through `preflight_lifecycle_command`; setup inside
/// `publish_era`'s transaction.
fn require_command_fits_completion(
    conn: &rusqlite::Connection,
    command: LifecycleCommand,
) -> Result<(), LifecycleError> {
    let setup_complete = setup_complete_exists(conn)?;
    let (fits, reason) = match command {
        LifecycleCommand::Setup => (
            !setup_complete,
            "setup is already complete for this data directory",
        ),
        LifecycleCommand::Run | LifecycleCommand::MaintenanceFlush => (
            setup_complete,
            "setup has not completed for this data directory",
        ),
    };
    if !fits {
        return Err(LifecycleError::NotAdmissible {
            requested: command,
            reason,
        });
    }
    Ok(())
}

fn setup_complete_exists(conn: &rusqlite::Connection) -> Result<bool, LifecycleError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM setup_complete WHERE singleton_id = 0)",
        [],
        |row| row.get::<_, bool>(0),
    )?)
}

fn refuse_on_canonical_divergence(conn: &rusqlite::Connection) -> Result<(), LifecycleError> {
    let nonce = conn
        .query_row(
            "SELECT nonce FROM canonical_divergence WHERE singleton_id = 0",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(nonce) = nonce {
        return Err(LifecycleError::CanonicalDivergence {
            nonce: i64_to_u64(nonce),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_db;

    fn seeded(command: LifecycleCommand) -> (crate::test_support::TestDb, Storage) {
        let db = temp_db("lifecycle-facts");
        let storage =
            Storage::initialize_for_command(db.path.as_str(), command).expect("initialize");
        (db, storage)
    }

    fn complete_seeded_setup(storage: &mut Storage) {
        storage
            .publish_era(
                std::path::Path::new("/tmp/facts-genesis"),
                &crate::storage::EraRecord::EMPTY_GENESIS,
            )
            .expect("complete setup");
    }

    fn seed_divergence(storage: &Storage) {
        storage
            .conn
            .execute(
                "INSERT INTO canonical_divergence \
                 (singleton_id, nonce, safe_input_index, kind, detected_at_ms) \
                 VALUES (0, 7, 8, 'foreign', 9)",
                [],
            )
            .expect("seed divergence");
    }

    #[test]
    fn completion_rule_is_two_sided() {
        let (_db, mut storage) = seeded(LifecycleCommand::Setup);
        // Before completion: run and maintenance refuse; setup may proceed.
        assert!(matches!(
            storage.preflight_lifecycle_command(LifecycleCommand::Run),
            Err(LifecycleError::NotAdmissible { .. })
        ));
        assert!(matches!(
            storage.preflight_lifecycle_command(LifecycleCommand::MaintenanceFlush),
            Err(LifecycleError::NotAdmissible { .. })
        ));
        storage
            .preflight_lifecycle_command(LifecycleCommand::Setup)
            .expect("setup over its incomplete database");

        complete_seeded_setup(&mut storage);
        // After completion: setup refuses; run and maintenance pass.
        assert!(matches!(
            storage.preflight_lifecycle_command(LifecycleCommand::Setup),
            Err(LifecycleError::NotAdmissible { .. })
        ));
        storage
            .preflight_lifecycle_command(LifecycleCommand::Run)
            .expect("run passes over a completed setup");
        storage
            .preflight_lifecycle_command(LifecycleCommand::MaintenanceFlush)
            .expect("flush passes");
    }

    #[test]
    fn divergence_refuses_every_preflight_and_setup_completion() {
        let (_db, mut storage) = seeded(LifecycleCommand::Setup);

        seed_divergence(&storage);

        for command in [
            LifecycleCommand::Setup,
            LifecycleCommand::Run,
            LifecycleCommand::MaintenanceFlush,
        ] {
            assert!(matches!(
                storage.preflight_lifecycle_command(command),
                Err(LifecycleError::CanonicalDivergence { nonce: 7 })
            ));
        }
        assert!(matches!(
            storage.publish_era(
                std::path::Path::new("/tmp/facts-genesis"),
                &crate::storage::EraRecord::EMPTY_GENESIS,
            ),
            Err(LifecycleError::CanonicalDivergence { nonce: 7 })
        ));
    }

    #[test]
    fn publish_era_is_atomic_and_one_shot() {
        use crate::storage::EraRecord;
        use crate::storage::test_helpers::era;
        let (_db, mut storage) = seeded(LifecycleCommand::Setup);
        let prefix = std::path::Path::new("/tmp/era-baseline");
        let unspanned = EraRecord {
            next_batch_nonce: 5,
            ..era(3, 4, 0)
        };
        assert!(matches!(
            storage.publish_era(prefix, &unspanned),
            Err(LifecycleError::Storage(_))
        ));
        // Nothing of the refused era survives: no record, anchor, snapshot or marker.
        assert!(storage.history_state().is_err());
        assert_eq!(storage.batch_tree_anchor().unwrap(), 0);
        assert!(storage.list_dump_rows().unwrap().is_empty());
        assert!(!storage.is_setup_complete().unwrap());

        let published = EraRecord {
            next_batch_nonce: 5,
            ..era(3, 3, 0)
        };
        storage.publish_era(prefix, &published).expect("publish");
        let state = storage.history_state().unwrap();
        assert_eq!(
            (
                state.base_l2_tx_count,
                state.rebuilt_history_end,
                state.base_safe_block,
                state.first_queued_input
            ),
            (3, 3, 0, 0)
        );
        assert_eq!(storage.batch_tree_anchor().unwrap(), 5);
        assert_eq!(
            storage.latest_snapshot().unwrap().unwrap().l2_tx_count,
            sequencer_core::history::L2TxCount::new(3)
        );
        assert!(
            storage.open_state().unwrap().is_none(),
            "setup opens no Tip"
        );
        assert!(matches!(
            storage.publish_era(prefix, &published),
            Err(LifecycleError::NotAdmissible {
                requested: LifecycleCommand::Setup,
                ..
            })
        ));
        assert_eq!(storage.history_state().unwrap(), state);
    }

    #[test]
    fn publish_era_refuses_a_directory_with_a_batch() {
        let (_db, mut storage) = seeded(LifecycleCommand::Setup);
        storage
            .conn
            .execute(
                "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms) \
                 VALUES (0, NULL, 0, 0)",
                [],
            )
            .expect("seed a batch");
        assert!(matches!(
            storage.publish_era(
                std::path::Path::new("/tmp/era-baseline"),
                &crate::storage::EraRecord::EMPTY_GENESIS
            ),
            Err(LifecycleError::Malformed(_))
        ));
        assert!(!storage.is_setup_complete().unwrap());
    }

    #[test]
    fn terminal_cause_recording_is_gated_on_nothing() {
        let (_db, mut storage) = seeded(LifecycleCommand::Setup);
        seed_divergence(&storage);

        storage
            .record_terminal_fault(
                LifecycleCommand::Run,
                "persistent storage invariant violation",
            )
            .expect("the settlement write must never be refused");
        let fault = storage
            .latest_terminal_fault()
            .expect("read")
            .expect("recorded fault");
        assert_eq!(fault.command, LifecycleCommand::Run);
        assert_eq!(fault.cause, "persistent storage invariant violation");
        assert!(matches!(
            storage.record_terminal_fault(LifecycleCommand::Run, ""),
            Err(LifecycleError::Malformed(_))
        ));
    }

    #[test]
    fn black_box_is_append_only_at_the_engine_and_reads_newest_first() {
        let (_db, mut storage) = seeded(LifecycleCommand::Setup);
        assert_eq!(storage.latest_terminal_fault().expect("empty read"), None);
        storage
            .record_terminal_fault(LifecycleCommand::Setup, "first death")
            .expect("record first");
        storage
            .record_terminal_fault(LifecycleCommand::MaintenanceFlush, "second death")
            .expect("record second");
        let fault = storage
            .latest_terminal_fault()
            .expect("read")
            .expect("latest fault");
        assert_eq!(fault.command, LifecycleCommand::MaintenanceFlush);
        assert_eq!(fault.cause, "second death");

        assert!(
            storage
                .conn
                .execute("DELETE FROM terminal_faults", [])
                .is_err()
        );
        assert!(
            storage
                .conn
                .execute("UPDATE terminal_faults SET cause = 'x'", [])
                .is_err()
        );
    }
}
