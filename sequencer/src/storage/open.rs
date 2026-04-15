// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `Storage` struct definition plus connection-open and migration entry points.
//!
//! Method clusters live in sibling files (`ingress`, `egress`, `l1_inputs`,
//! `l1_submission`, `recovery`, `admin`) — each adds its own `impl Storage`.

use rusqlite::{Connection, OpenFlags};
use rusqlite_migration::{M, Migrations};

use super::StorageOpenError;

const MIGRATION_0001_SCHEMA: &str = include_str!("migrations/0001_schema.sql");

/// Sequencer storage backed by a single SQLite database.
///
/// All methods take `&mut self` to enforce exclusive access at the Rust level,
/// matching SQLite's single-writer model. Read-only access uses a separate
/// `Storage` instance opened via [`Storage::open_read_only`].
pub struct Storage {
    pub(super) conn: Connection,
}

impl Storage {
    pub fn open(path: &str, synchronous: &str) -> Result<Self, StorageOpenError> {
        let conn = Self::open_connection_with_migrations(path, synchronous)?;
        Ok(Self { conn })
    }

    /// Open without running migrations. Used by tests that need to inspect or
    /// pre-seed the schema before letting the migration runner touch it.
    pub fn open_without_migrations(
        path: &str,
        synchronous: &str,
    ) -> Result<Self, StorageOpenError> {
        let conn = Self::open_connection(path, synchronous)?;
        Ok(Self { conn })
    }

    /// Read-only handle. Uses a 50ms `busy_timeout` (vs. 5s for writers) so
    /// readers fail fast under write pressure and don't block on hot paths.
    pub fn open_read_only(path: &str) -> Result<Self, StorageOpenError> {
        let conn = Self::open_connection_read_only(path)?;
        Ok(Self { conn })
    }

    pub fn open_connection(path: &str, synchronous: &str) -> Result<Connection, StorageOpenError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", synchronous)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(conn)
    }

    pub fn open_connection_read_only(path: &str) -> Result<Connection, StorageOpenError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "query_only", "ON")?;
        // Readers should fail fast under write pressure to keep tail latency bounded.
        conn.pragma_update(None, "busy_timeout", 50)?;
        Ok(conn)
    }

    pub fn open_connection_with_migrations(
        path: &str,
        synchronous: &str,
    ) -> Result<Connection, StorageOpenError> {
        let mut conn = Self::open_connection(path, synchronous)?;
        Self::run_migrations(&mut conn)?;
        Ok(conn)
    }

    pub fn run_migrations(conn: &mut Connection) -> Result<(), StorageOpenError> {
        Migrations::from_slice(&[M::up(MIGRATION_0001_SCHEMA)]).to_latest(conn)?;
        Ok(())
    }
}
