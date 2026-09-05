// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `Storage` struct definition plus connection-open and migration entry points.
//!
//! Method clusters live in sibling files (`ingress`, `egress`, `l1_inputs`,
//! `l1_submission`, `recovery`, `admin`) — each adds its own `impl Storage`.

use rusqlite::{Connection, OpenFlags, Result, Transaction, TransactionBehavior, types::Type};
use rusqlite_migration::{HookResult, M, Migrations};

use super::{EraId, LifecycleCommand, StorageOpenError};

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
/// And it is what makes the setup completion transaction a valid
/// linearization point: it commits after the genesis-snapshot row's
/// transaction, so "completion durable ⇒
/// snapshot row durable ⇒ dump dir durable" only holds because FULL fsyncs
/// every commit — under NORMAL the completion WAL frame could survive while
/// the snapshot row's frames are lost, and `run` would boot a half-set-up
/// DB. Benchmarked at the flip: round-trip/ack deltas were noise-level on
/// NVMe.
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
    /// on the fly would mint an ownerless era with no creating command —
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

    /// Create the baseline schema and history era in one migration
    /// transaction, with the creating command deciding the history bases. On
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
        let recorded_at_ms = i64::try_from(crate::clock::unix_now_ms()).unwrap_or(i64::MAX);
        let era_id = mint_era_id(tx)?;
        let (base_executed_input_count, base_safe_input_index) = match initial_command {
            Some(LifecycleCommand::Rebuild) => (None, None),
            Some(LifecycleCommand::Setup) | None => (Some(0_i64), Some(0_i64)),
            Some(LifecycleCommand::Run | LifecycleCommand::MaintenanceFlush) => {
                unreachable!("baseline command was checked before migration")
            }
        };
        tx.execute(
            "INSERT INTO history_state \
             (singleton_id, era_id, era_created_at_ms, recovery_generation, \
              base_executed_input_count, base_safe_input_index) \
             VALUES (0, ?1, ?2, 0, ?3, ?4)",
            rusqlite::params![
                era_id.as_bytes().as_slice(),
                recorded_at_ms,
                base_executed_input_count,
                base_safe_input_index
            ],
        )?;
        if let Some(hook) = post_initial_metadata {
            hook(tx)?;
        }
        Ok(())
    })
}

fn mint_era_id(tx: &Transaction<'_>) -> Result<EraId> {
    let random = tx.query_row("SELECT randomblob(16)", [], |row| row.get::<_, Vec<u8>>(0))?;
    let mut bytes: [u8; EraId::BYTE_LEN] = random.try_into().map_err(|value: Vec<u8>| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "SQLite randomblob returned {} bytes, expected {}",
                    value.len(),
                    EraId::BYTE_LEN
                ),
            )),
        )
    })?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EraId::from_bytes(bytes)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail_after_observing_initial_metadata(tx: &Transaction<'_>) -> HookResult {
        let history_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM history_state \
             WHERE singleton_id = 0 AND recovery_generation = 0 \
               AND base_executed_input_count = 0 \
               AND base_safe_input_index = 0",
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
            Some(LifecycleCommand::Setup),
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
    fn baseline_mints_distinct_uuid_v4_eras_and_initializes_known_bases() {
        let setup_dir = tempfile::tempdir().expect("setup tempdir");
        let setup_path = setup_dir.path().join("sequencer.sqlite");
        let setup = Storage::initialize_for_command(
            setup_path.to_str().expect("utf8"),
            LifecycleCommand::Setup,
        )
        .expect("initialize setup");
        let setup_history = setup.history_state().expect("setup history");
        assert_eq!(setup_history.version.recovery_generation.get(), 0);
        assert_eq!(setup_history.base_executed_input_count, Some(0));
        assert_eq!(setup_history.base_safe_input_index, Some(0));

        let rebuild_dir = tempfile::tempdir().expect("rebuild tempdir");
        let rebuild_path = rebuild_dir.path().join("sequencer.sqlite");
        let rebuild = Storage::initialize_for_command(
            rebuild_path.to_str().expect("utf8"),
            LifecycleCommand::Rebuild,
        )
        .expect("initialize rebuild");
        let rebuild_history = rebuild.history_state().expect("rebuild history");
        assert_eq!(rebuild_history.version.recovery_generation.get(), 0);
        assert_eq!(rebuild_history.base_executed_input_count, None);
        assert_eq!(rebuild_history.base_safe_input_index, None);
        assert_ne!(setup_history.version.era_id, rebuild_history.version.era_id);

        let generic_dir = tempfile::tempdir().expect("generic tempdir");
        let generic_path = generic_dir.path().join("sequencer.sqlite");
        let generic =
            Storage::open(generic_path.to_str().expect("utf8")).expect("initialize generic schema");
        let generic_history = generic.history_state().expect("generic history");
        assert_eq!(generic_history.base_executed_input_count, Some(0));
        assert_eq!(generic_history.base_safe_input_index, Some(0));
    }
}
