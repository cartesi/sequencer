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
    Rebuild,
    Run,
    MaintenanceFlush,
}

impl LifecycleCommand {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Rebuild => "rebuild",
            Self::Run => "run",
            Self::MaintenanceFlush => "maintenance_flush",
        }
    }

    fn parse(value: &str) -> Result<Self, LifecycleError> {
        match value {
            "setup" => Ok(Self::Setup),
            "rebuild" => Ok(Self::Rebuild),
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
    /// Publish a durable setup dump and its complete baseline atomically.
    pub(crate) fn complete_baseline_setup(
        &mut self,
        prefix: &std::path::Path,
        count: sequencer_core::history::ExecutedInputCount,
        safe_block: u64,
        next_batch_nonce: u64,
        recovery: bool,
    ) -> Result<(), LifecycleError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        refuse_on_canonical_divergence(&tx)?;
        require_command_fits_completion(
            &tx,
            if recovery {
                LifecycleCommand::Rebuild
            } else {
                LifecycleCommand::Setup
            },
        )?;
        super::history::initialize_history_in(&tx, count, safe_block)?;
        super::mutations::set_batch_tree_anchor_in(&tx, next_batch_nonce)?;
        if recovery {
            super::ingress::open_recovery_tip_in_tx(&tx, safe_block)?;
        }
        super::snapshot_dumps::insert_baseline_snapshot_in(&tx, prefix, count)?;
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

    /// When setup completed, or `None` before it has.
    pub fn setup_completed_at_ms(&self) -> Result<Option<u64>, LifecycleError> {
        let completed: Option<i64> = self
            .conn
            .query_row(
                "SELECT completed_at_ms FROM setup_complete WHERE singleton_id = 0",
                [],
                |row| row.get(0),
            )
            .optional()?;
        completed
            .map(|ms| {
                u64::try_from(ms).map_err(|_| {
                    LifecycleError::Malformed(format!(
                        "setup_complete completed_at_ms {ms} is negative"
                    ))
                })
            })
            .transpose()
    }
}

/// The two-sided completion rule — the one command-ordering fact:
/// setup/rebuild never restart over a completed setup (completion is
/// once-per-database), run and maintenance never start before one exists.
/// Run and maintenance reach it through `preflight_lifecycle_command`;
/// setup and rebuild inside `complete_baseline_setup`'s transaction, after
/// `crate::commands::setup` has checked the same fact up front.
fn require_command_fits_completion(
    conn: &rusqlite::Connection,
    command: LifecycleCommand,
) -> Result<(), LifecycleError> {
    let setup_complete = setup_complete_exists(conn)?;
    let (fits, reason) = match command {
        LifecycleCommand::Setup | LifecycleCommand::Rebuild => (
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
            .complete_baseline_setup(
                std::path::Path::new("/tmp/facts-genesis"),
                sequencer_core::history::ExecutedInputCount::ZERO,
                0,
                0,
                false,
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
    fn setup_completed_at_ms_reads_the_completion_fact() {
        let (_db, mut storage) = seeded(LifecycleCommand::Setup);
        assert_eq!(storage.setup_completed_at_ms().expect("read"), None);
        complete_seeded_setup(&mut storage);
        assert!(
            storage
                .setup_completed_at_ms()
                .expect("read")
                .is_some_and(|ms| ms > 0)
        );
    }

    #[test]
    fn completion_rule_is_two_sided() {
        let (_db, mut storage) = seeded(LifecycleCommand::Rebuild);
        // Before completion: run and maintenance refuse; rebuild may retry.
        assert!(matches!(
            storage.preflight_lifecycle_command(LifecycleCommand::Run),
            Err(LifecycleError::NotAdmissible { .. })
        ));
        assert!(matches!(
            storage.preflight_lifecycle_command(LifecycleCommand::MaintenanceFlush),
            Err(LifecycleError::NotAdmissible { .. })
        ));
        storage
            .preflight_lifecycle_command(LifecycleCommand::Rebuild)
            .expect("rebuild retry over its incomplete database");

        complete_seeded_setup(&mut storage);
        // After completion: setup/rebuild refuse; run and maintenance pass.
        assert!(matches!(
            storage.preflight_lifecycle_command(LifecycleCommand::Setup),
            Err(LifecycleError::NotAdmissible { .. })
        ));
        assert!(matches!(
            storage.preflight_lifecycle_command(LifecycleCommand::Rebuild),
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
            LifecycleCommand::Rebuild,
            LifecycleCommand::Run,
            LifecycleCommand::MaintenanceFlush,
        ] {
            assert!(matches!(
                storage.preflight_lifecycle_command(command),
                Err(LifecycleError::CanonicalDivergence { nonce: 7 })
            ));
        }
        assert!(matches!(
            storage.complete_baseline_setup(
                std::path::Path::new("/tmp/facts-genesis"),
                sequencer_core::history::ExecutedInputCount::ZERO,
                0,
                0,
                false,
            ),
            Err(LifecycleError::CanonicalDivergence { nonce: 7 })
        ));
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
