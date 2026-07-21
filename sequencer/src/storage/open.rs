// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `Storage` struct definition plus connection-open and migration entry points.
//!
//! Method clusters live in sibling files (`ingress`, `egress`, `l1_inputs`,
//! `l1_submission`, `recovery`, `admin`) — each adds its own `impl Storage`.

use rusqlite::{Connection, OpenFlags, Result, Transaction, TransactionBehavior};
use rusqlite_migration::{M, Migrations};

use super::StorageOpenError;

const MIGRATION_0001_SCHEMA: &str = include_str!("migrations/0001_schema.sql");
const MIGRATION_0002_SAFE_INPUT_PROVENANCE: &str =
    include_str!("migrations/0002_safe_input_provenance.sql");

/// SQLite `synchronous` pragma used by every production writer connection.
/// `FULL` under WAL fsyncs on every commit, so commits survive power loss /
/// OS crash — not just process crash. Load-bearing (review R3/F3): the
/// sequencer externalizes effects on commits (acks `POST /tx` after the
/// chunk commit; the submitter broadcasts sealed batches), and a rewound
/// commit after externalization is silent divergence — e.g. a re-sealed
/// batch at the same nonce with different content than the one the
/// scheduler executed. The dump side already pays the same cost
/// (`create_dump` fsyncs); this closes the DB half. Also a precondition
/// for the wallet-nonce watermark's write-before-broadcast guarantee
/// (review R1a). And it is what makes the `setup_complete` marker a valid
/// linearization point: the marker is committed in its own transaction
/// after the genesis-snapshot row's transaction, so "marker durable ⇒
/// snapshot row durable ⇒ dump dir durable" only holds because FULL fsyncs
/// every commit — under NORMAL the marker's WAL frame could survive while
/// the snapshot row's frames are lost, and `run` would boot a half-set-up
/// DB. Benchmarked at the flip: round-trip/ack deltas were noise-level on
/// NVMe (see review ledger WP1).
///
/// Do not relax to NORMAL without revisiting all three (R3/F3 externalized
/// commits, R1a watermark, the setup-marker linearization in
/// `runtime/setup.rs` + `storage/migrations/0001_schema.sql`).
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
    pub fn open(path: &str) -> Result<Self, StorageOpenError> {
        let mut conn = open_writer_connection(path)?;
        run_migrations(&mut conn)?;
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
pub(super) fn run_migrations(conn: &mut Connection) -> Result<(), StorageOpenError> {
    Migrations::from_slice(&[
        M::up(MIGRATION_0001_SCHEMA),
        M::up(MIGRATION_0002_SAFE_INPUT_PROVENANCE),
    ])
    .to_latest(conn)?;
    Ok(())
}
