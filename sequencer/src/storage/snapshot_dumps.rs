// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Storage half of the snapshot dump lifecycle: pending and finalized
//! snapshots plus lease management. See `migrations/0001_schema.sql` for
//! the schema rationale.
//!
//! This module exposes SQLite operations only; filesystem cleanup of the
//! actual dump artifacts is the caller's responsibility (`gc_dump_rows`
//! returns the prefixes to remove, then the caller invokes
//! `delete_dump_row` after the filesystem delete succeeds). The lane
//! drives the lifecycle from `inclusion_lane/` in a subsequent commit.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, Result, ToSql, Transaction, params};

use super::Storage;
use super::convert::{i64_to_u64, u64_to_i64};

/// A row in `dumps`: SQLite primary key plus the on-disk directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpRow {
    pub id: i64,
    pub prefix: PathBuf,
}

/// One row of `pending_snapshots` joined with the underlying `dumps` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDump {
    pub nonce: u64,
    pub dump: DumpRow,
    pub l2_tx_index: u64,
}

/// The singleton `finalized_snapshot` row joined with the underlying
/// `dumps` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedDump {
    pub dump: DumpRow,
    pub inclusion_block: u64,
    pub l2_tx_index: u64,
}

impl Storage {
    /// Insert a new dump row plus its pending-snapshot row in one
    /// transaction. Caller has already created the dump on disk at
    /// `prefix`. Returns the newly assigned `dump_id`.
    ///
    /// Fails with a UNIQUE constraint violation if `prefix` already
    /// exists in `dumps`; this is intentional — the caller is expected
    /// to pass fresh, unique prefixes per call, and reuse is a bug
    /// worth surfacing loudly.
    pub fn insert_pending_dump(
        &mut self,
        prefix: &Path,
        nonce: u64,
        l2_tx_index: u64,
    ) -> Result<i64> {
        self.write(|tx| insert_pending_dump_in(tx, prefix, nonce, l2_tx_index))
    }

    /// Atomically promote a non-empty set of pending nonces into the
    /// single `finalized_snapshot` row. The nonce with the largest
    /// value becomes the new finalized snapshot; its `l2_tx_index` is
    /// carried over from `pending_snapshots`, and the promoted nonces
    /// are removed from `pending_snapshots` in the same transaction.
    ///
    /// All passed nonces must currently exist in `pending_snapshots`;
    /// a missing nonce surfaces as a `QueryReturnedNoRows` error.
    /// Empty `nonces` slice is a no-op.
    pub fn promote_pending_dumps(&mut self, nonces: &[u64], inclusion_block: u64) -> Result<()> {
        self.write(|tx| promote_pending_dumps_in(tx, nonces, inclusion_block))
    }

    /// Increment a dump's lease count. Called by HTTP handlers at the
    /// start of a stream to prevent GC from removing the dump while in
    /// use.
    pub fn acquire_dump_lease(&mut self, dump_id: i64) -> Result<()> {
        self.write(|tx| acquire_dump_lease_in(tx, dump_id))
    }

    /// Decrement a dump's lease count. Called by HTTP handlers when a
    /// stream completes (or aborts).
    pub fn release_dump_lease(&mut self, dump_id: i64) -> Result<()> {
        self.write(|tx| release_dump_lease_in(tx, dump_id))
    }

    /// Reset every dump's lease count to zero. Called at process
    /// startup to clear stale leases from a crashed previous run.
    pub fn reset_dump_leases(&mut self) -> Result<usize> {
        self.write(reset_dump_leases_in)
    }

    /// Return dumps eligible for garbage collection: `lease_count = 0`
    /// AND not referenced by `pending_snapshots` or
    /// `finalized_snapshot`. The caller is responsible for filesystem
    /// cleanup; once that succeeds, the caller invokes
    /// [`Storage::delete_dump_row`] for each id.
    pub fn gc_dump_rows(&mut self) -> Result<Vec<DumpRow>> {
        self.read(gc_dump_rows_in)
    }

    /// Delete a dump row from `dumps` by id. Errors if the row is
    /// still FK-referenced; the caller must clear pending/finalized
    /// rows referencing it first (or use [`Storage::gc_dump_rows`] to
    /// only delete unreferenced rows).
    pub fn delete_dump_row(&mut self, dump_id: i64) -> Result<()> {
        self.write(|tx| delete_dump_row_in(tx, dump_id))
    }

    /// Read the pending snapshot with the highest nonce, if any. Used
    /// by catch-up to load the freshest available state on startup.
    pub fn latest_pending_dump(&mut self) -> Result<Option<PendingDump>> {
        self.read(latest_pending_dump_in)
    }

    /// Read the singleton finalized snapshot, if any. Used by the
    /// watchdog endpoint and by catch-up's fallback when no pending
    /// snapshot exists.
    pub fn finalized_dump(&mut self) -> Result<Option<FinalizedDump>> {
        self.read(finalized_dump_in)
    }

    /// Return every row in `dumps`. Used at startup to reconcile
    /// SQLite-tracked prefixes against what actually exists on disk
    /// (paths on disk not in `dumps` are removed; rows in `dumps`
    /// whose paths are missing are dropped on the next GC pass).
    pub fn list_dump_rows(&mut self) -> Result<Vec<DumpRow>> {
        self.read(list_dump_rows_in)
    }

    /// Delete every row from `pending_snapshots`. Used by danger-zone
    /// recovery: the underlying batches are cascade-invalidated and
    /// their snapshots represent states the canonical stream will never
    /// reach. The dumps themselves become GC-eligible immediately.
    pub fn clear_pending_dumps(&mut self) -> Result<usize> {
        self.write(clear_pending_dumps_in)
    }

    /// Look up a batch's nonce from `batches`. Errors with
    /// `QueryReturnedNoRows` if the batch doesn't exist — the lane
    /// calls this immediately after `close_frame_and_batch` so the row
    /// should always be there.
    pub fn batch_nonce(&mut self, batch_index: u64) -> Result<u64> {
        self.read(|tx| {
            let nonce: i64 = tx.query_row(
                "SELECT nonce FROM batches WHERE batch_index = ?1",
                params![u64_to_i64(batch_index)],
                |row| row.get(0),
            )?;
            Ok(i64_to_u64(nonce))
        })
    }

    /// Return the highest `offset` in `sequenced_l2_txs` for the given
    /// batch, or `None` if the batch has no sequenced txs. Used at
    /// batch close to record the l2_tx_index of the pending dump so
    /// downstream consumers can subscribe to the WS feed at the right
    /// depth.
    pub fn max_l2_tx_offset_for_batch(&mut self, batch_index: u64) -> Result<Option<u64>> {
        self.read(|tx| {
            // MAX(offset) returns one row with NULL when no rows match
            // the filter; wrap in Option<i64> to surface the empty case.
            let offset: Option<i64> = tx.query_row(
                "SELECT MAX(offset) FROM sequenced_l2_txs WHERE batch_index = ?1",
                params![u64_to_i64(batch_index)],
                |row| row.get(0),
            )?;
            Ok(offset.map(i64_to_u64))
        })
    }

    /// Look up the nonce of a previously-accepted batch by its safe
    /// input index. Returns `None` if the safe input is either a
    /// third-party direct input (not our batch) or one of our batches
    /// that the scheduler did not accept (stale-nonce drop). Used when
    /// processing safe inputs to decide whether to promote a pending
    /// snapshot.
    pub fn accepted_batch_nonce_at(&mut self, safe_input_index: u64) -> Result<Option<u64>> {
        self.read(|tx| {
            tx.query_row(
                "SELECT nonce FROM safe_accepted_batches WHERE safe_input_index = ?1",
                params![u64_to_i64(safe_input_index)],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|opt| opt.map(i64_to_u64))
        })
    }
}

// ── transaction-scoped helpers ────────────────────────────────────────────

fn insert_pending_dump_in(
    tx: &Transaction<'_>,
    prefix: &Path,
    nonce: u64,
    l2_tx_index: u64,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO dumps (prefix) VALUES (?1)",
        params![path_to_text(prefix)],
    )?;
    let dump_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO pending_snapshots (nonce, dump_id, l2_tx_index) \
         VALUES (?1, ?2, ?3)",
        params![u64_to_i64(nonce), dump_id, u64_to_i64(l2_tx_index)],
    )?;
    Ok(dump_id)
}

fn promote_pending_dumps_in(
    tx: &Transaction<'_>,
    nonces: &[u64],
    inclusion_block: u64,
) -> Result<()> {
    if nonces.is_empty() {
        return Ok(());
    }
    // Promote the dump associated with the largest nonce: it represents
    // the latest state in the block. The l2_tx_index is carried over
    // from the pending row — the snapshot bytes correspond to state at
    // batch close, and that's the offset they reflect.
    let max_nonce = nonces.iter().copied().max().expect("non-empty checked");

    let (new_dump_id, l2_tx_index): (i64, i64) = tx.query_row(
        "SELECT dump_id, l2_tx_index FROM pending_snapshots WHERE nonce = ?1",
        params![u64_to_i64(max_nonce)],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    tx.execute(
        "INSERT OR REPLACE INTO finalized_snapshot \
         (singleton_id, dump_id, inclusion_block, l2_tx_index) \
         VALUES (0, ?1, ?2, ?3)",
        params![new_dump_id, u64_to_i64(inclusion_block), l2_tx_index],
    )?;

    // Remove the promoted nonces from pending. The dumps for the
    // non-max promoted nonces (and any previous finalized) become GC
    // candidates from this point.
    let placeholders = std::iter::repeat_n("?", nonces.len())
        .collect::<Vec<_>>()
        .join(",");
    let stmt = format!("DELETE FROM pending_snapshots WHERE nonce IN ({placeholders})");
    let params_i64: Vec<i64> = nonces.iter().copied().map(u64_to_i64).collect();
    let params_dyn: Vec<&dyn ToSql> = params_i64.iter().map(|n| n as &dyn ToSql).collect();
    tx.execute(&stmt, params_dyn.as_slice())?;

    Ok(())
}

fn acquire_dump_lease_in(tx: &Transaction<'_>, dump_id: i64) -> Result<()> {
    let changed = tx.execute(
        "UPDATE dumps SET lease_count = lease_count + 1 WHERE id = ?1",
        params![dump_id],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

fn release_dump_lease_in(tx: &Transaction<'_>, dump_id: i64) -> Result<()> {
    let changed = tx.execute(
        "UPDATE dumps SET lease_count = lease_count - 1 WHERE id = ?1",
        params![dump_id],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

fn reset_dump_leases_in(tx: &Transaction<'_>) -> Result<usize> {
    tx.execute("UPDATE dumps SET lease_count = 0", [])
}

fn gc_dump_rows_in(tx: &Transaction<'_>) -> Result<Vec<DumpRow>> {
    let mut stmt = tx.prepare(
        "SELECT id, prefix FROM dumps \
         WHERE lease_count = 0 \
           AND id NOT IN (SELECT dump_id FROM pending_snapshots) \
           AND id NOT IN (SELECT dump_id FROM finalized_snapshot) \
         ORDER BY id",
    )?;
    let rows: Result<Vec<_>> = stmt.query_map([], row_to_dump_row)?.collect();
    rows
}

fn delete_dump_row_in(tx: &Transaction<'_>, dump_id: i64) -> Result<()> {
    let changed = tx.execute("DELETE FROM dumps WHERE id = ?1", params![dump_id])?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

fn latest_pending_dump_in(tx: &Transaction<'_>) -> Result<Option<PendingDump>> {
    tx.query_row(
        "SELECT p.nonce, p.dump_id, d.prefix, p.l2_tx_index \
         FROM pending_snapshots p \
         JOIN dumps d ON d.id = p.dump_id \
         ORDER BY p.nonce DESC \
         LIMIT 1",
        [],
        |row| {
            let nonce: i64 = row.get(0)?;
            let dump_id: i64 = row.get(1)?;
            let prefix: String = row.get(2)?;
            let l2_tx_index: i64 = row.get(3)?;
            Ok(PendingDump {
                nonce: i64_to_u64(nonce),
                dump: DumpRow {
                    id: dump_id,
                    prefix: PathBuf::from(prefix),
                },
                l2_tx_index: i64_to_u64(l2_tx_index),
            })
        },
    )
    .optional()
}

fn finalized_dump_in(tx: &Transaction<'_>) -> Result<Option<FinalizedDump>> {
    tx.query_row(
        "SELECT f.dump_id, d.prefix, f.inclusion_block, f.l2_tx_index \
         FROM finalized_snapshot f \
         JOIN dumps d ON d.id = f.dump_id \
         WHERE f.singleton_id = 0",
        [],
        |row| {
            let dump_id: i64 = row.get(0)?;
            let prefix: String = row.get(1)?;
            let inclusion_block: i64 = row.get(2)?;
            let l2_tx_index: i64 = row.get(3)?;
            Ok(FinalizedDump {
                dump: DumpRow {
                    id: dump_id,
                    prefix: PathBuf::from(prefix),
                },
                inclusion_block: i64_to_u64(inclusion_block),
                l2_tx_index: i64_to_u64(l2_tx_index),
            })
        },
    )
    .optional()
}

fn list_dump_rows_in(tx: &Transaction<'_>) -> Result<Vec<DumpRow>> {
    let mut stmt = tx.prepare("SELECT id, prefix FROM dumps ORDER BY id")?;
    let rows: Result<Vec<_>> = stmt.query_map([], row_to_dump_row)?.collect();
    rows
}

fn clear_pending_dumps_in(tx: &Transaction<'_>) -> Result<usize> {
    tx.execute("DELETE FROM pending_snapshots", [])
}

fn row_to_dump_row(row: &rusqlite::Row<'_>) -> Result<DumpRow> {
    Ok(DumpRow {
        id: row.get(0)?,
        prefix: PathBuf::from(row.get::<_, String>(1)?),
    })
}

fn path_to_text(path: &Path) -> String {
    // Prefixes are produced by the lane and are valid UTF-8 by
    // construction (they're built from u64 ids and unicode-safe
    // configured data dirs). `to_string_lossy` is the well-trodden
    // path for crossing the Path → SQLite TEXT boundary; if the lane
    // ever produces a non-UTF8 prefix that's a separate bug we'd
    // catch via the round-trip read mismatch.
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use crate::storage::{Storage, test_helpers::temp_db};

    use super::{DumpRow, FinalizedDump, PendingDump};

    fn prefix(n: u64) -> PathBuf {
        PathBuf::from(format!("/data/dumps/{n}"))
    }

    #[test]
    fn insert_pending_creates_dump_and_pending_rows() {
        let db = temp_db("insert-pending");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let dump_id = storage
            .insert_pending_dump(&prefix(0), 0, 10)
            .expect("insert");

        let pending = storage.latest_pending_dump().expect("read").expect("some");
        assert_eq!(
            pending,
            PendingDump {
                nonce: 0,
                dump: DumpRow {
                    id: dump_id,
                    prefix: prefix(0),
                },
                l2_tx_index: 10,
            }
        );

        let rows = storage.list_dump_rows().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prefix, prefix(0));
    }

    #[test]
    fn insert_pending_rejects_duplicate_prefix() {
        let db = temp_db("dup-prefix");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        storage
            .insert_pending_dump(&prefix(0), 0, 0)
            .expect("first");
        let err = storage
            .insert_pending_dump(&prefix(0), 1, 0)
            .expect_err("second");
        assert!(
            err.to_string().contains("UNIQUE"),
            "expected UNIQUE failure, got: {err}"
        );
    }

    #[test]
    fn latest_pending_picks_highest_nonce() {
        let db = temp_db("latest-pending");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        storage.insert_pending_dump(&prefix(0), 0, 100).unwrap();
        storage.insert_pending_dump(&prefix(1), 2, 102).unwrap();
        storage.insert_pending_dump(&prefix(2), 1, 101).unwrap();

        let latest = storage.latest_pending_dump().unwrap().unwrap();
        assert_eq!(latest.nonce, 2);
        assert_eq!(latest.l2_tx_index, 102);
        assert_eq!(latest.dump.prefix, prefix(1));
    }

    #[test]
    fn promote_moves_max_nonce_to_finalized_and_clears_promoted_pending() {
        let db = temp_db("promote");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let id_a = storage.insert_pending_dump(&prefix(0), 0, 100).unwrap();
        let id_b = storage.insert_pending_dump(&prefix(1), 1, 101).unwrap();
        let id_c = storage.insert_pending_dump(&prefix(2), 2, 102).unwrap();
        let id_unrelated = storage.insert_pending_dump(&prefix(3), 3, 103).unwrap();

        storage.promote_pending_dumps(&[0, 1, 2], 500).unwrap();

        // Finalized points at the dump for nonce 2.
        let finalized = storage.finalized_dump().unwrap().unwrap();
        assert_eq!(
            finalized,
            FinalizedDump {
                dump: DumpRow {
                    id: id_c,
                    prefix: prefix(2),
                },
                inclusion_block: 500,
                l2_tx_index: 102,
            }
        );

        // Nonces 0, 1, 2 are gone from pending; 3 stays.
        let latest = storage.latest_pending_dump().unwrap().unwrap();
        assert_eq!(latest.nonce, 3);
        assert_eq!(latest.dump.id, id_unrelated);

        // Dumps 0 and 1 are now GC-eligible (unreferenced); 2 is in
        // finalized and 3 is in pending, so both still referenced.
        let gc: HashSet<i64> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(gc, HashSet::from([id_a, id_b]));
    }

    #[test]
    fn promote_empty_nonces_is_noop() {
        let db = temp_db("promote-empty");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        storage.promote_pending_dumps(&[], 0).unwrap();

        assert!(storage.finalized_dump().unwrap().is_none());
        assert!(storage.latest_pending_dump().unwrap().is_some());
    }

    #[test]
    fn promote_overwrites_previous_finalized_and_makes_old_dump_gc_eligible() {
        let db = temp_db("promote-overwrite");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let id_first = storage.insert_pending_dump(&prefix(0), 0, 100).unwrap();
        storage.promote_pending_dumps(&[0], 500).unwrap();

        let id_second = storage.insert_pending_dump(&prefix(1), 1, 101).unwrap();
        storage.promote_pending_dumps(&[1], 501).unwrap();

        let finalized = storage.finalized_dump().unwrap().unwrap();
        assert_eq!(finalized.dump.id, id_second);

        let gc: HashSet<i64> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(gc, HashSet::from([id_first]));
    }

    #[test]
    fn lease_acquire_release_round_trips_and_blocks_gc() {
        let db = temp_db("lease-roundtrip");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let dump_id = storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        storage.promote_pending_dumps(&[0], 500).unwrap();

        // Promote another so the first becomes a GC candidate.
        let _ = storage.insert_pending_dump(&prefix(1), 1, 1).unwrap();
        storage.promote_pending_dumps(&[1], 501).unwrap();

        // Without a lease the first dump is GC-eligible.
        assert!(
            storage
                .gc_dump_rows()
                .unwrap()
                .iter()
                .any(|row| row.id == dump_id)
        );

        // Acquire the lease and the dump is no longer eligible.
        storage.acquire_dump_lease(dump_id).unwrap();
        assert!(
            storage
                .gc_dump_rows()
                .unwrap()
                .iter()
                .all(|row| row.id != dump_id)
        );

        // Release and it's eligible again.
        storage.release_dump_lease(dump_id).unwrap();
        assert!(
            storage
                .gc_dump_rows()
                .unwrap()
                .iter()
                .any(|row| row.id == dump_id)
        );
    }

    #[test]
    fn lease_supports_multiple_holders() {
        let db = temp_db("lease-multi");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let dump_id = storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        storage.promote_pending_dumps(&[0], 0).unwrap();
        let _ = storage.insert_pending_dump(&prefix(1), 1, 1).unwrap();
        storage.promote_pending_dumps(&[1], 1).unwrap();

        // Two concurrent readers.
        storage.acquire_dump_lease(dump_id).unwrap();
        storage.acquire_dump_lease(dump_id).unwrap();

        // One releases — still leased.
        storage.release_dump_lease(dump_id).unwrap();
        assert!(
            storage
                .gc_dump_rows()
                .unwrap()
                .iter()
                .all(|row| row.id != dump_id)
        );

        // Second releases — now eligible.
        storage.release_dump_lease(dump_id).unwrap();
        assert!(
            storage
                .gc_dump_rows()
                .unwrap()
                .iter()
                .any(|row| row.id == dump_id)
        );
    }

    #[test]
    fn release_below_zero_is_rejected_by_check_constraint() {
        let db = temp_db("lease-underflow");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let dump_id = storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        let err = storage
            .release_dump_lease(dump_id)
            .expect_err("lease_count cannot go negative");
        assert!(
            err.to_string().contains("CHECK"),
            "expected CHECK constraint failure, got: {err}"
        );
    }

    #[test]
    fn reset_dump_leases_clears_every_lease() {
        let db = temp_db("lease-reset");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let id_a = storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        let id_b = storage.insert_pending_dump(&prefix(1), 1, 0).unwrap();
        storage.acquire_dump_lease(id_a).unwrap();
        storage.acquire_dump_lease(id_a).unwrap();
        storage.acquire_dump_lease(id_b).unwrap();

        let cleared = storage.reset_dump_leases().unwrap();
        assert_eq!(cleared, 2);

        // After reset, releasing would underflow if leases lingered;
        // since GC eligibility depends on lease_count == 0 AND no
        // references, both rows still have pending references and
        // aren't GC-eligible — but a follow-up promote would correctly
        // surface them.
        storage.promote_pending_dumps(&[0, 1], 0).unwrap();
        // id_a's dump is now superseded; id_b is finalized.
        let gc: Vec<i64> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(gc, vec![id_a]);
    }

    #[test]
    fn delete_dump_row_removes_from_dumps_table() {
        let db = temp_db("delete-row");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let dump_id = storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        storage.promote_pending_dumps(&[0], 0).unwrap();

        // While in finalized, delete should fail (FK restrict).
        let err = storage
            .delete_dump_row(dump_id)
            .expect_err("FK should prevent delete while finalized");
        assert!(
            err.to_string().contains("FOREIGN KEY"),
            "expected FK failure, got: {err}"
        );

        // Promote a successor so finalized no longer references the row.
        let _ = storage.insert_pending_dump(&prefix(1), 1, 0).unwrap();
        storage.promote_pending_dumps(&[1], 0).unwrap();

        storage.delete_dump_row(dump_id).expect("delete after gc");
        assert!(
            storage
                .list_dump_rows()
                .unwrap()
                .iter()
                .all(|row| row.id != dump_id)
        );
    }

    #[test]
    fn clear_pending_removes_all_pending_rows() {
        let db = temp_db("clear-pending");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let _id_a = storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        let id_b = storage.insert_pending_dump(&prefix(1), 1, 0).unwrap();
        // Move id_a into finalized.
        storage.promote_pending_dumps(&[0], 0).unwrap();
        // id_b stays in pending.

        let cleared = storage.clear_pending_dumps().unwrap();
        assert_eq!(cleared, 1);

        assert!(storage.latest_pending_dump().unwrap().is_none());

        // id_b is now unreferenced → GC eligible. id_a stays in finalized.
        let gc: Vec<i64> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(gc, vec![id_b]);
    }

    #[test]
    fn list_dump_rows_returns_everything() {
        let db = temp_db("list-rows");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        storage.insert_pending_dump(&prefix(1), 1, 0).unwrap();
        storage.insert_pending_dump(&prefix(2), 2, 0).unwrap();
        storage.promote_pending_dumps(&[0], 0).unwrap();

        let rows = storage.list_dump_rows().unwrap();
        assert_eq!(rows.len(), 3);
        let prefixes: HashSet<PathBuf> = rows.into_iter().map(|row| row.prefix).collect();
        assert_eq!(prefixes, HashSet::from([prefix(0), prefix(1), prefix(2)]));
    }

    #[test]
    fn promote_missing_nonce_errors() {
        let db = temp_db("promote-missing");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let err = storage
            .promote_pending_dumps(&[42], 0)
            .expect_err("missing nonce should error");
        assert!(matches!(err, rusqlite::Error::QueryReturnedNoRows));
    }
}
