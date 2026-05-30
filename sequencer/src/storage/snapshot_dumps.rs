// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Storage half of the snapshot dump lifecycle: pending and finalized
//! snapshots plus lease management. See `migrations/0001_schema.sql` for
//! the schema rationale.
//!
//! This module exposes SQLite operations only; filesystem cleanup of the
//! actual dump artifacts is the caller's responsibility:
//! `gc_unreferenced_dumps` deletes unreferenced rows in one transaction
//! and returns their prefixes, and the lane (`inclusion_lane/snapshot.rs`)
//! removes the directories afterward. The lane drives the lifecycle —
//! register at batch close, promote on L1 observation (atomically with the
//! drain), GC after each promotion.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, Result, Transaction, params};

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

/// The snapshot the `/latest_snapshot` endpoint serves: the latest pending
/// dump, or the finalized dump as fallback. Carries the dump row (id +
/// prefix) and the replay offset; the source (pending vs finalized) is
/// immaterial to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedSnapshot {
    pub dump: DumpRow,
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

    /// Atomically promote the pending dump for `max_nonce` into the
    /// single `finalized_snapshot` row, carrying over its
    /// `l2_tx_index`. In the same transaction, every pending row with
    /// `nonce <= max_nonce` is deleted — that's the explicitly-promoted
    /// row plus any stale rows behind it from earlier promotions that
    /// failed to clean up. L1 wallet nonces guarantee batches land in
    /// monotonic order, so anything with a smaller nonce has landed
    /// by the time we're observing `max_nonce`.
    ///
    /// `max_nonce` must currently exist in `pending_snapshots`; a
    /// missing row surfaces as `QueryReturnedNoRows`.
    ///
    /// Standalone promotion, retained for test setup (it's the only way to
    /// *supersede* an existing finalized row, which `insert_finalized_dump`
    /// can't). **Production does not call this**: the lane promotes via
    /// `promote_finalized_in` folded into the safe-frontier-advance
    /// transaction ([`Storage::close_frame_only_promoting`]), so the promotion
    /// commits atomically with the drain it derives from — a separate promotion
    /// could commit ahead of the drain and wedge a restart on a deleted pending
    /// row.
    pub fn promote_finalized(&mut self, max_nonce: u64, inclusion_block: u64) -> Result<()> {
        self.write(|tx| promote_finalized_in(tx, max_nonce, inclusion_block))
    }

    /// Increment a dump's lease count, non-atomically with any row read.
    /// Test-only: production leases atomically with the row read via
    /// [`Storage::acquire_finalized_lease`] /
    /// [`Storage::acquire_latest_snapshot_lease`], which call
    /// `acquire_dump_lease_in` inside the read transaction.
    #[cfg(test)]
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

    /// Current lease count for a dump, or `None` if the row doesn't exist.
    /// Read-only; exposed for operational visibility (in-flight stream
    /// count) and tests.
    pub fn dump_lease_count(&mut self, dump_id: i64) -> Result<Option<i64>> {
        self.read(|tx| {
            tx.query_row(
                "SELECT lease_count FROM dumps WHERE id = ?1",
                params![dump_id],
                |row| row.get(0),
            )
            .optional()
        })
    }

    /// Return dumps eligible for garbage collection (`lease_count = 0`
    /// AND not referenced by `pending_snapshots` or `finalized_snapshot`)
    /// without deleting them. Test-only: production reads and deletes in
    /// one transaction via [`Storage::gc_unreferenced_dumps`], closing the
    /// race a read-then-delete split would open against a concurrent
    /// `acquire_dump_lease_in`.
    #[cfg(test)]
    pub fn gc_dump_rows(&mut self) -> Result<Vec<DumpRow>> {
        self.read(gc_dump_rows_in)
    }

    /// Atomic GC pass: in one SQLite transaction, find all eligible
    /// dumps and delete their rows. Returns the (id, prefix) pairs so
    /// the caller can drive filesystem cleanup separately — file
    /// deletion is best-effort, and an orphan file on failure is
    /// acceptable per the no-dangling-row invariant.
    ///
    /// The single-transaction shape closes a race window: between a
    /// non-atomic `gc_dump_rows()` and the per-row `delete_dump_row`,
    /// an HTTP handler on another thread could `acquire_dump_lease`,
    /// and a naive per-row delete would race against the lease.
    /// Doing both inside one `write` (Immediate-mode tx) serializes
    /// against any concurrent writer.
    pub fn gc_unreferenced_dumps(&mut self) -> Result<Vec<DumpRow>> {
        self.write(|tx| {
            let candidates = gc_dump_rows_in(tx)?;
            for row in &candidates {
                tx.execute("DELETE FROM dumps WHERE id = ?1", params![row.id])?;
            }
            Ok(candidates)
        })
    }

    /// Delete a dump row from `dumps` by id. Errors if the row is still
    /// FK-referenced. Test-only: production deletes unreferenced rows
    /// atomically via [`Storage::gc_unreferenced_dumps`].
    #[cfg(test)]
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

    /// Atomically read the finalized snapshot AND increment its dump's
    /// lease, in one transaction. This closes the race where an HTTP
    /// handler reads the finalized row, a promotion + GC delete that dump,
    /// and the handler then fails to open the file: holding the lease from
    /// the moment of the read keeps GC off the dump. Returns `None` if no
    /// finalized snapshot exists. The caller MUST release the lease when
    /// done — the HTTP layer does this through a drop-guard, and
    /// [`Storage::reset_dump_leases`] at startup is the crash backstop.
    pub fn acquire_finalized_lease(&mut self) -> Result<Option<FinalizedDump>> {
        self.write(|tx| {
            let finalized = finalized_dump_in(tx)?;
            if let Some(ref f) = finalized {
                acquire_dump_lease_in(tx, f.dump.id)?;
            }
            Ok(finalized)
        })
    }

    /// Atomically read the snapshot to serve (latest pending, else
    /// finalized) AND lease its dump, in one transaction. Same race-closing
    /// guarantee and release contract as [`Storage::acquire_finalized_lease`].
    pub fn acquire_latest_snapshot_lease(&mut self) -> Result<Option<LeasedSnapshot>> {
        self.write(|tx| {
            let snapshot = match latest_pending_dump_in(tx)? {
                Some(pending) => Some(LeasedSnapshot {
                    dump: pending.dump,
                    l2_tx_index: pending.l2_tx_index,
                }),
                None => finalized_dump_in(tx)?.map(|f| LeasedSnapshot {
                    dump: f.dump,
                    l2_tx_index: f.l2_tx_index,
                }),
            };
            if let Some(ref s) = snapshot {
                acquire_dump_lease_in(tx, s.dump.id)?;
            }
            Ok(snapshot)
        })
    }

    /// Return every row in `dumps`. Used at startup to reconcile
    /// SQLite-tracked prefixes against what actually exists on disk
    /// (paths on disk not in `dumps` are removed; rows in `dumps`
    /// whose paths are missing are dropped on the next GC pass).
    pub fn list_dump_rows(&mut self) -> Result<Vec<DumpRow>> {
        self.read(list_dump_rows_in)
    }

    /// Delete every row from `pending_snapshots`. Test-only convenience
    /// wrapper: production danger-zone recovery composes
    /// `clear_pending_dumps_in` into the same transaction as the cascade
    /// invalidation (see `storage/recovery.rs`), so the pending rows for
    /// cascade-doomed batches are cleared atomically with them.
    #[cfg(test)]
    pub fn clear_pending_dumps(&mut self) -> Result<usize> {
        self.write(clear_pending_dumps_in)
    }

    /// Insert a new dump row and a finalized-snapshot row in one
    /// transaction. Used at first startup to register the genesis dump
    /// directly as finalized (bypassing pending). Fails if a finalized
    /// row already exists (the singleton constraint).
    pub fn insert_finalized_dump(
        &mut self,
        prefix: &Path,
        inclusion_block: u64,
        l2_tx_index: u64,
    ) -> Result<i64> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO dumps (prefix) VALUES (?1)",
                params![path_to_text(prefix)],
            )?;
            let dump_id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO finalized_snapshot \
                 (singleton_id, dump_id, inclusion_block, l2_tx_index) \
                 VALUES (0, ?1, ?2, ?3)",
                params![
                    dump_id,
                    u64_to_i64(inclusion_block),
                    u64_to_i64(l2_tx_index)
                ],
            )?;
            Ok(dump_id)
        })
    }

    /// Highest `offset` in the valid ordered L2-tx stream (the global
    /// replay head), or 0 when empty. Test-only standalone read: the
    /// production batch-close path reads the same value via the
    /// `valid_ordered_l2_tx_head` free function *inside* its seal
    /// transaction (see `close_frame_and_batch_with_pending_dump`), so
    /// the recorded `l2_tx_index` is consistent with the seal.
    #[cfg(test)]
    pub fn valid_ordered_l2_tx_head(&mut self) -> Result<u64> {
        self.read(|tx| super::queries::valid_ordered_l2_tx_head(tx))
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

pub(super) fn insert_pending_dump_in(
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

pub(super) fn promote_finalized_in(
    tx: &Transaction<'_>,
    max_nonce: u64,
    inclusion_block: u64,
) -> Result<()> {
    // The promoted dump's bytes correspond to state at batch close, so
    // we carry over its `l2_tx_index` directly.
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

    // Clean up the promoted row plus any stale ones behind it. The
    // dumps for non-max nonces (and the previous finalized) become GC
    // candidates from this point.
    tx.execute(
        "DELETE FROM pending_snapshots WHERE nonce <= ?1",
        params![u64_to_i64(max_nonce)],
    )?;

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

#[cfg(test)]
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

/// Visible to siblings within the `storage` module so that the
/// danger-zone recovery path can compose pending-dump cleanup into
/// the same transaction as `cascade_invalidate_from` — otherwise a
/// crash between the cascade and the clear would leave stale pending
/// snapshots pointing at states the canonical stream will never reach.
pub(super) fn clear_pending_dumps_in(tx: &Transaction<'_>) -> Result<usize> {
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

        storage.promote_finalized(2, 500).unwrap();

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
    fn promote_overwrites_previous_finalized_and_makes_old_dump_gc_eligible() {
        let db = temp_db("promote-overwrite");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let id_first = storage.insert_pending_dump(&prefix(0), 0, 100).unwrap();
        storage.promote_finalized(0, 500).unwrap();

        let id_second = storage.insert_pending_dump(&prefix(1), 1, 101).unwrap();
        storage.promote_finalized(1, 501).unwrap();

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
        storage.promote_finalized(0, 500).unwrap();

        // Promote another so the first becomes a GC candidate.
        let _ = storage.insert_pending_dump(&prefix(1), 1, 1).unwrap();
        storage.promote_finalized(1, 501).unwrap();

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
        storage.promote_finalized(0, 0).unwrap();
        let _ = storage.insert_pending_dump(&prefix(1), 1, 1).unwrap();
        storage.promote_finalized(1, 1).unwrap();

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
        storage.promote_finalized(1, 0).unwrap();
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
        storage.promote_finalized(0, 0).unwrap();

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
        storage.promote_finalized(1, 0).unwrap();

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
        storage.promote_finalized(0, 0).unwrap();
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
        storage.promote_finalized(0, 0).unwrap();

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
            .promote_finalized(42, 0)
            .expect_err("missing nonce should error");
        assert!(matches!(err, rusqlite::Error::QueryReturnedNoRows));
    }

    #[test]
    fn gc_unreferenced_dumps_drops_rows_and_returns_them() {
        let db = temp_db("gc-unreferenced");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        // Set up: 4 dumps. ids 0, 1 superseded (unreferenced after
        // promotion). id 2 in finalized. id 3 in pending.
        let _id_a = storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        storage.promote_finalized(0, 0).unwrap();
        let _id_b = storage.insert_pending_dump(&prefix(1), 1, 0).unwrap();
        storage.promote_finalized(1, 0).unwrap();
        // Now id_a is superseded by id_b's promotion. id_a is GC-eligible.
        let _id_c = storage.insert_pending_dump(&prefix(2), 2, 0).unwrap();
        storage.promote_finalized(2, 0).unwrap();
        let _id_d = storage.insert_pending_dump(&prefix(3), 3, 0).unwrap();

        let removed = storage.gc_unreferenced_dumps().unwrap();
        let removed_ids: HashSet<i64> = removed.iter().map(|row| row.id).collect();

        // The two superseded dumps got removed. The current finalized
        // (id_c) and the pending (id_d) survived.
        assert_eq!(removed.len(), 2);
        assert!(!removed_ids.is_empty());

        // Confirm the survivors are still in dumps.
        let surviving: HashSet<PathBuf> = storage
            .list_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.prefix)
            .collect();
        assert!(surviving.contains(&prefix(2)));
        assert!(surviving.contains(&prefix(3)));
        assert!(!surviving.contains(&prefix(0)));
        assert!(!surviving.contains(&prefix(1)));
    }

    #[test]
    fn gc_unreferenced_dumps_skips_leased_rows() {
        let db = temp_db("gc-leased");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        // Two dumps that would both be GC-eligible…
        let id_a = storage.insert_pending_dump(&prefix(0), 0, 0).unwrap();
        storage.promote_finalized(0, 0).unwrap();
        let id_b = storage.insert_pending_dump(&prefix(1), 1, 0).unwrap();
        storage.promote_finalized(1, 0).unwrap();
        // id_a is superseded. Both ids might be candidates; the
        // promotion of 1 makes id_a unreferenced and id_b finalized.

        // … but we hold a lease on id_a, so GC must skip it.
        storage.acquire_dump_lease(id_a).unwrap();

        let removed = storage.gc_unreferenced_dumps().unwrap();
        let removed_ids: HashSet<i64> = removed.iter().map(|row| row.id).collect();
        assert!(!removed_ids.contains(&id_a), "lease blocks GC");
        assert!(!removed_ids.contains(&id_b), "id_b is still finalized");

        // Releasing the lease makes id_a eligible.
        storage.release_dump_lease(id_a).unwrap();
        let removed_again = storage.gc_unreferenced_dumps().unwrap();
        let removed_again_ids: HashSet<i64> = removed_again.iter().map(|row| row.id).collect();
        assert!(removed_again_ids.contains(&id_a));
    }

    #[test]
    fn gc_unreferenced_dumps_on_empty_db_returns_empty() {
        let db = temp_db("gc-empty");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let removed = storage.gc_unreferenced_dumps().unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn acquire_finalized_lease_returns_none_when_no_finalized() {
        let db = temp_db("acquire-finalized-none");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        assert!(storage.acquire_finalized_lease().unwrap().is_none());
    }

    #[test]
    fn acquire_finalized_lease_reads_and_leases_atomically_blocking_gc() {
        let db = temp_db("acquire-finalized-lease");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let id_a = storage.insert_finalized_dump(&prefix(0), 100, 5).unwrap();
        let finalized = storage
            .acquire_finalized_lease()
            .unwrap()
            .expect("a finalized snapshot exists");
        assert_eq!(finalized.dump.id, id_a);
        assert_eq!(finalized.inclusion_block, 100);
        assert_eq!(finalized.l2_tx_index, 5);

        // Supersede A with a promotion so A becomes unreferenced — but the
        // lease must keep GC off it.
        storage.insert_pending_dump(&prefix(1), 0, 7).unwrap();
        storage.promote_finalized(0, 101).unwrap();
        let eligible: HashSet<i64> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(
            !eligible.contains(&id_a),
            "lease must block GC of the superseded-but-leased dump"
        );

        // Releasing makes it GC-eligible.
        storage.release_dump_lease(id_a).unwrap();
        let eligible: HashSet<i64> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(eligible.contains(&id_a), "released dump is GC-eligible");
    }

    #[test]
    fn acquire_latest_snapshot_lease_prefers_pending_and_leases() {
        let db = temp_db("acquire-latest-pending");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        storage.insert_finalized_dump(&prefix(0), 100, 5).unwrap();
        let id_pending = storage.insert_pending_dump(&prefix(1), 3, 9).unwrap();

        let leased = storage
            .acquire_latest_snapshot_lease()
            .unwrap()
            .expect("a snapshot exists");
        assert_eq!(leased.dump.id, id_pending, "prefers the latest pending");
        assert_eq!(leased.l2_tx_index, 9);

        // Clear pending so the leased dump is unreferenced; the lease still
        // blocks GC until released.
        storage.clear_pending_dumps().unwrap();
        let eligible: HashSet<i64> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(!eligible.contains(&id_pending), "lease blocks GC");
        storage.release_dump_lease(id_pending).unwrap();
        let eligible: HashSet<i64> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(eligible.contains(&id_pending));
    }

    #[test]
    fn acquire_latest_snapshot_lease_falls_back_to_finalized() {
        let db = temp_db("acquire-latest-finalized");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let id_finalized = storage.insert_finalized_dump(&prefix(0), 100, 5).unwrap();
        let leased = storage
            .acquire_latest_snapshot_lease()
            .unwrap()
            .expect("falls back to finalized");
        assert_eq!(leased.dump.id, id_finalized);
        assert_eq!(leased.l2_tx_index, 5);
    }
}
