// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `Storage` struct definition plus connection-open and migration entry points.
//!
//! Method clusters live in sibling files (`ingress`, `egress`, `l1_inputs`,
//! `l1_submission`, `recovery`, `admin`) — each adds its own `impl Storage`.

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
    /// The path this connection was opened from. Carried so a lease guard can
    /// re-open a brief writer connection to release on drop (see
    /// `snapshot_dumps::LeaseGuard`); the egress snapshot handlers open
    /// per-op, so the guard can't borrow this `Storage`.
    pub(super) path: String,
}

impl Storage {
    /// Production open: runs migrations, uses the canonical synchronous pragma.
    ///
    /// Refuses a path with no database file (production builds): every
    /// database is created by an owning command through
    /// [`Storage::initialize_for_command`], so a missing file here is a
    /// deployment mistake (mistyped `--data-dir`, wrong mount). Creating one
    /// on the fly would create an ownerless schema with no creating command —
    /// database absence means uninitialized, never create-and-proceed.
    /// Crate tests keep create-on-open as their fixture idiom; the
    /// command-less baseline in [`baseline_migration`] exists for them.
    pub(crate) fn open(path: &str) -> Result<Self, StorageOpenError> {
        #[cfg(not(test))]
        if !std::path::Path::new(path).exists() {
            return Err(StorageOpenError::NeverInitialized {
                path: path.to_string(),
            });
        }
        let mut conn = open_writer_connection(path)?;
        run_migrations(&mut conn, None)?;
        Ok(Self {
            conn,
            path: path.to_string(),
        })
    }

    /// Create the schema and record its owning command in one migration
    /// transaction. The complete history baseline is published later. On
    /// an already-migrated database the hook does not run; callers must
    /// inspect the existing facts.
    pub(crate) fn initialize_for_command(
        path: &str,
        command: LifecycleCommand,
    ) -> Result<Self, StorageOpenError> {
        assert!(
            matches!(command, LifecycleCommand::Setup | LifecycleCommand::Rebuild),
            "an uninitialized lifecycle may begin only with setup or rebuild"
        );
        let mut conn = open_writer_connection(path)?;
        run_migrations(&mut conn, Some(command))?;
        Ok(Self {
            conn,
            path: path.to_string(),
        })
    }

    /// Read-only handle. Uses a 50ms `busy_timeout` (vs. 5s for writers) so
    /// readers fail fast under write pressure and don't block on hot paths.
    pub fn open_read_only(path: &str) -> Result<Self, StorageOpenError> {
        let conn = open_reader_connection(path)?;
        Ok(Self {
            conn,
            path: path.to_string(),
        })
    }

    /// Read-write handle that does NOT run migrations — for components that
    /// open the DB *after* startup has already migrated it (e.g. egress
    /// snapshot handlers doing brief lease writes, and tests that pre-seed
    /// the schema). Same pragmas as [`Storage::open`] (WAL, `foreign_keys`,
    /// 5s `busy_timeout`); running migrations is the caller's responsibility
    /// (the runtime does it once via [`Storage::open`] at startup).
    pub fn open_writer(path: &str) -> Result<Self, StorageOpenError> {
        let conn = open_writer_connection(path)?;
        Ok(Self {
            conn,
            path: path.to_string(),
        })
    }

    /// Test-only: return a raw `Connection` with the same pragmas as
    /// [`Storage::open`]. Used by tests that need to reach past the typed API
    /// (e.g., rewinding `synced_at_ms`, installing failure triggers).
    #[cfg(test)]
    pub fn open_connection(path: &str) -> std::result::Result<Connection, StorageOpenError> {
        open_writer_connection(path)
    }

    /// Run `f` inside a Deferred transaction, commit on success. For pure reads.
    ///
    /// Using Deferred rather than Immediate matches SQLite's default — readers
    /// don't hold a write lock and don't block writers. If `f` returns `Err`
    /// the transaction is dropped unsent (auto-rollback); on success the
    /// commit is issued before returning `Ok`.
    pub fn read<T, F>(&mut self, f: F) -> Result<T>
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
    pub fn write<T, F>(&mut self, f: F) -> Result<T>
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
fn open_writer_connection(path: &str) -> Result<Connection, StorageOpenError> {
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
/// production history insert, without maintaining a shadow migration.
///
/// `initial_command: None` is the crate-test fixture path (create-on-open
/// with genesis history bases); production cannot reach it because
/// [`Storage::open`] refuses paths with no database file.
fn baseline_migration(
    initial_command: Option<LifecycleCommand>,
    post_initial_metadata: Option<PostInitialMetadataHook>,
) -> M<'static> {
    M::up_with_hook(MIGRATION_0001_SCHEMA, move |tx: &Transaction<'_>| {
        if initial_command.is_none() {
            super::history::initialize_history_in(
                tx,
                sequencer_core::history::ExecutedInputCount::ZERO,
                0,
            )?;
        }
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
            "SELECT COUNT(*) FROM history_state \
             WHERE singleton_id = 0 AND recovery_generation = 0 \
               AND base_executed_input_count = 0 \
               AND base_safe_block = 0",
            [],
            |row| row.get(0),
        )?;
        if history_count != 1 {
            return Err(rusqlite_migration::HookError::Hook(
                "production initial history insert was not observed".to_string(),
            ));
        }
        Err(rusqlite_migration::HookError::Hook(
            "injected failure after initial history insert".to_string(),
        ))
    }

    #[test]
    fn failing_initial_hook_rolls_back_schema_and_history_together() {
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
    fn setup_registers_history_only_with_complete_baseline() {
        let setup_dir = tempfile::tempdir().unwrap();
        let setup_path = setup_dir.path().join("setup.sqlite");
        let mut setup =
            Storage::initialize_for_command(setup_path.to_str().unwrap(), LifecycleCommand::Setup)
                .unwrap();
        assert!(matches!(
            setup.history_state(),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        setup
            .write(|tx| {
                super::super::history::initialize_history_in(
                    tx,
                    sequencer_core::history::ExecutedInputCount::ZERO,
                    0,
                )
            })
            .unwrap();
        let a = setup.history_state().unwrap();
        let rebuild_path = setup_dir.path().join("rebuild.sqlite");
        let mut rebuild = Storage::initialize_for_command(
            rebuild_path.to_str().unwrap(),
            LifecycleCommand::Rebuild,
        )
        .unwrap();
        assert!(matches!(
            rebuild.history_state(),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        rebuild
            .write(|tx| {
                super::super::history::initialize_history_in(
                    tx,
                    sequencer_core::history::ExecutedInputCount::new(8),
                    100,
                )
            })
            .unwrap();
        let b = rebuild.history_state().unwrap();
        assert_ne!(a.version.era_id, b.version.era_id);
        assert_eq!(b.base_executed_input_count, 8);
        assert_eq!(b.base_safe_block, 100);
    }
}
