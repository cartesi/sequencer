// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `Storage` struct definition plus connection-open and migration entry points.
//!
//! Method clusters live in sibling files (`ingress`, `egress`, `l1_inputs`,
//! `l1_submission`, `recovery`) — each adds its own `impl Storage`.

use rusqlite::{Connection, OpenFlags, Result, Transaction, TransactionBehavior};
use rusqlite_migration::{HookResult, M, Migrations};

use super::{LifecycleCommand, StorageOpenError};

const MIGRATION_0001_SCHEMA: &str = include_str!("migrations/0001_schema.sql");

/// SQLite `synchronous` pragma used by every production writer connection.
/// `FULL` under WAL fsyncs on every commit, so commits survive power loss /
/// OS crash — not just process crash. Load-bearing: the sequencer
/// externalizes effects on commits (acks `POST /tx` after the
/// chunk commit; the submitter broadcasts sealed batches), and a rewound
/// commit after externalization is silent divergence — e.g. a re-sealed
/// batch at the same nonce with different content than the one the
/// scheduler executed. The dump side already pays the same cost
/// (`create_dump` fsyncs); this closes the DB half. Also a precondition
/// for the wallet-nonce watermark's write-before-broadcast guarantee.
/// Setup publishes its complete baseline and completion together after the
/// artifact is durable; FULL makes that boundary survive power loss too.
///
/// Do not relax to NORMAL without revisiting all three (externalized
/// commits, the write-before-broadcast watermark, and the setup-completion
/// linearization in `commands/setup/` + `storage/migrations/0001_schema.sql`).
const SYNCHRONOUS_PRAGMA: &str = "FULL";

/// Sequencer storage backed by a single SQLite database.
///
/// All methods take `&mut self` to enforce exclusive access at the Rust level,
/// matching SQLite's single-writer model. Read-only access uses a separate
/// `Storage` instance opened via [`Storage::open_read_only`].
pub struct Storage {
    pub(super) conn: Connection,
}

impl Storage {
    /// Production open: runs migrations, uses the canonical synchronous pragma.
    /// `run` calls it once, before startup recovery; every later open of the
    /// run is [`Storage::open_writer`] or [`Storage::open_read_only`].
    ///
    /// Refuses a path with no database file (production builds): every
    /// database is created by an owning command through
    /// [`Storage::initialize_for_command`], so a missing file here is a
    /// deployment mistake (mistyped `--data-dir`, wrong mount). Creating one
    /// on the fly would create an ownerless schema with no creating command —
    /// database absence means uninitialized, never create-and-proceed.
    /// On an existing file that was never migrated it creates the schema
    /// with no era, so the database is not set up. Crate tests keep
    /// create-on-open as their fixture idiom, with the genesis era that
    /// [`baseline_migration`] inserts only in test builds.
    pub(crate) fn open(path: &str) -> Result<Self, StorageOpenError> {
        #[cfg(not(test))]
        if !std::path::Path::new(path).exists() {
            return Err(StorageOpenError::NeverInitialized {
                path: path.to_string(),
            });
        }
        let mut conn = open_writer_connection(path)?;
        run_migrations(&mut conn, None)?;
        Ok(Self { conn })
    }

    /// Create the schema for setup in one migration transaction. The era is
    /// published later, and its row is setup's completion. On
    /// an already-migrated database the hook does not run; callers must
    /// inspect the existing facts.
    pub(crate) fn initialize_for_command(
        path: &str,
        command: LifecycleCommand,
    ) -> Result<Self, StorageOpenError> {
        assert!(
            matches!(command, LifecycleCommand::Setup),
            "an uninitialized lifecycle may begin only with setup"
        );
        let mut conn = open_writer_connection(path)?;
        run_migrations(&mut conn, Some(command))?;
        Ok(Self { conn })
    }

    /// A private in-memory database with setup's schema, for questions the
    /// schema answers before setup may create its database file.
    pub(crate) fn schema_only() -> Result<Self, StorageOpenError> {
        let mut conn = Connection::open_in_memory()?;
        run_migrations(&mut conn, Some(LifecycleCommand::Setup))?;
        Ok(Self { conn })
    }

    /// Read-only handle. Uses a 50ms `busy_timeout` (vs. 5s for writers) so
    /// readers fail fast under write pressure and don't block on hot paths.
    pub fn open_read_only(path: &str) -> Result<Self, StorageOpenError> {
        let conn = open_reader_connection(path)?;
        Ok(Self { conn })
    }

    /// Read-write handle that does NOT run migrations — for components that
    /// open the DB *after* `run` has migrated it (recovery, the lane, the
    /// input reader, and tests that pre-seed the schema). Same pragmas as
    /// [`Storage::open`] (WAL, `foreign_keys`, 5s `busy_timeout`).
    pub fn open_writer(path: &str) -> Result<Self, StorageOpenError> {
        let conn = open_writer_connection(path)?;
        Ok(Self { conn })
    }

    /// Run `f` inside a Deferred transaction, commit on success. For pure reads.
    ///
    /// Using Deferred rather than Immediate matches SQLite's default — readers
    /// don't hold a write lock and don't block writers. If `f` returns `Err`
    /// the transaction is dropped unsent (auto-rollback); on success the
    /// commit is issued before returning `Ok`.
    pub(super) fn read<T, F>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T>,
    {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Run `f` inside an Immediate transaction, commit on success. For any
    /// mutation.
    ///
    /// Using Immediate acquires the write lock upfront so contending writers
    /// see `SQLITE_BUSY` immediately rather than mid-transaction — this is
    /// the right cadence under WAL + single-writer discipline. Same commit /
    /// auto-rollback semantics as [`Storage::read`].
    pub(super) fn write<T, F>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T>,
    {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

/// Open a read-write connection with WAL + `FULL` sync (`SYNCHRONOUS_PRAGMA`) +
/// 5s busy timeout.
pub(super) fn open_writer_connection(path: &str) -> Result<Connection, StorageOpenError> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", SYNCHRONOUS_PRAGMA)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

/// Open a read-only connection with `query_only` + 50ms busy timeout.
fn open_reader_connection(path: &str) -> Result<Connection, StorageOpenError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.pragma_update(None, "query_only", "ON")?;
    conn.pragma_update(None, "busy_timeout", 50)?;
    Ok(conn)
}

/// Apply all migrations. Package-private — callers use [`Storage::open`]
/// which runs this automatically.
pub(super) fn run_migrations(
    conn: &mut Connection,
    initial_command: Option<LifecycleCommand>,
) -> Result<(), StorageOpenError> {
    let migration = baseline_migration(initial_command, None);
    Migrations::from_slice(&[migration]).to_latest(conn)?;
    Ok(())
}

type PostInitialMetadataHook = fn(&Transaction<'_>) -> HookResult;

/// Build the exact baseline migration used in production. The optional hook
/// exists only so the atomicity test can fail *after* observing the
/// fixture era insert, without maintaining a shadow migration.
///
/// `initial_command: None` is [`Storage::open`]'s path. In crate tests it
/// inserts the genesis era (create-on-open fixtures); production builds
/// compile no era insert here, so only setup's publish ever writes one.
fn baseline_migration(
    initial_command: Option<LifecycleCommand>,
    post_initial_metadata: Option<PostInitialMetadataHook>,
) -> M<'static> {
    M::up_with_hook(MIGRATION_0001_SCHEMA, move |tx: &Transaction<'_>| {
        #[cfg(test)]
        if initial_command.is_none() {
            super::history::insert_era_in(tx, &super::NewEra::EMPTY_GENESIS)?;
        }
        #[cfg(not(test))]
        let _ = initial_command;
        if let Some(hook) = post_initial_metadata {
            hook(tx)?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail_after_observing_initial_metadata(tx: &Transaction<'_>) -> HookResult {
        let history_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM era \
             WHERE singleton_id = 0 AND base_l2_tx_count = 0 \
               AND stop_block = 0 AND anchor_batch_nonce = 0",
            [],
            |row| row.get(0),
        )?;
        if history_count != 1 {
            return Err(rusqlite_migration::HookError::Hook(
                "fixture genesis era insert was not observed".to_string(),
            ));
        }
        Err(rusqlite_migration::HookError::Hook(
            "injected failure after the genesis era insert".to_string(),
        ))
    }

    #[test]
    fn failing_initial_hook_rolls_back_schema_and_era_together() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("atomic-init.sqlite");
        let mut conn = open_writer_connection(path.to_str().expect("utf8")).expect("open");
        let definitions = [baseline_migration(
            None,
            Some(fail_after_observing_initial_metadata),
        )];
        let migrations = Migrations::from_slice(&definitions);
        migrations
            .to_latest(&mut conn)
            .expect_err("the injected hook failure must abort migration");

        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .expect("inspect schema");
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user version");
        assert_eq!(table_count, 0);
        assert_eq!(version, 0);
    }

    #[test]
    fn setup_schema_has_no_era_until_one_is_inserted() {
        let setup_dir = tempfile::tempdir().unwrap();
        let mut eras = Vec::new();
        for (name, era) in [
            ("genesis.sqlite", super::super::NewEra::EMPTY_GENESIS),
            ("machine.sqlite", super::super::test_helpers::era(8, 8, 100)),
        ] {
            let path = setup_dir.path().join(name);
            let mut setup =
                Storage::initialize_for_command(path.to_str().unwrap(), LifecycleCommand::Setup)
                    .unwrap();
            assert!(matches!(
                setup.era(),
                Err(rusqlite::Error::QueryReturnedNoRows)
            ));
            assert!(!setup.is_setup_complete().unwrap());
            setup
                .write(|tx| super::super::history::insert_era_in(tx, &era))
                .unwrap();
            eras.push(setup.era().unwrap());
        }
        assert_ne!(eras[0].id, eras[1].id);
        assert_eq!(eras[1].base.get(), 8);
        assert_eq!(eras[1].stop_block, 100);
        assert_eq!(eras[1].rebuilt_end.get(), 8);
    }
}

#[cfg(test)]
mod schema_tests;
