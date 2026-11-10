// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Immutable batch-close and baseline snapshots, derived acceptance, and the
//! in-process download leases. Files are durable before registration; GC deletes
//! rows before their files, which callers delete: no filesystem work here (I13).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Result, Transaction, TransactionBehavior, params};
use sequencer_core::history::{HistoryVersion, L2TxCount};

use super::Storage;
use super::convert::{i64_to_u64, u64_to_i64};
use super::history::{era_in, history_version_in, next_l2_tx_count_in};

/// One `snapshots` row: the artifact directory and the count it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: i64,
    pub prefix: PathBuf,
    pub l2_tx_count: L2TxCount,
}

/// A batch-close snapshot whose complete predicted prefix was accepted on L1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedDump {
    pub snapshot: Snapshot,
    pub inclusion_block: u64,
    pub next_batch_nonce: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum FinalizedSelectionError {
    #[error("canonical divergence prevents accepted checkpoint selection")]
    CanonicalDivergence,
    #[error("selecting finalized checkpoint: {0}")]
    Storage(#[from] rusqlite::Error),
}

type LeaseCounters = HashMap<i64, Arc<AtomicU32>>;

/// Download leases, shared by the egress handlers and the lane's GC. A leased
/// snapshot's row and files are never collected. Process-local: only `run`
/// serves and collects, under the data directory's process lock.
///
/// Acquire locks the registry, runs its selection in one read transaction,
/// increments the snapshot's counter and unlocks, so its read postdates any GC
/// commit. GC begins its IMMEDIATE transaction, locks the registry, deletes
/// only unleased candidates and unlocks after its transaction ends, so no
/// acquire reads a row GC is deleting. Release is the guard's decrement: no
/// lock, no I/O. GC holds SQLite's writer lock, then the registry; acquire
/// holds the registry and only reads, which WAL never makes wait on the writer
/// lock, so the two cannot deadlock (docs/snapshots/lifecycle.md).
#[derive(Debug, Clone, Default)]
pub struct SnapshotLeases(Arc<Mutex<LeaseCounters>>);

/// One download lease. Dropping it releases the lease; a zero entry stays in
/// the registry until the next GC removes it.
#[derive(Debug)]
pub struct LeaseGuard(Arc<AtomicU32>);

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl SnapshotLeases {
    fn lock(&self) -> MutexGuard<'_, LeaseCounters> {
        self.0.lock().expect("snapshot lease registry poisoned")
    }

    /// The acquire half of the protocol: `select` returns the snapshot id to
    /// lease and the data read with it, in one read transaction under the lock.
    fn lease<T, E>(
        &self,
        storage: &mut Storage,
        select: impl FnOnce(&Transaction<'_>) -> std::result::Result<Option<(i64, T)>, E>,
    ) -> std::result::Result<Option<(T, LeaseGuard)>, E>
    where
        E: From<rusqlite::Error>,
    {
        // A fresh connection parses the schema and maps the WAL index on its
        // first statement: do that before the lock, so only the selection
        // runs under it.
        storage.read(|tx| tx.prepare("SELECT 1 FROM snapshots").map(drop))?;
        let mut registry = self.lock();
        let Some((id, selected)) = storage.read(|tx| Ok(select(tx)))?? else {
            return Ok(None);
        };
        let counter = registry.entry(id).or_default().clone();
        counter.fetch_add(1, Ordering::SeqCst);
        drop(registry);
        Ok(Some((selected, LeaseGuard(counter))))
    }

    #[cfg(test)]
    pub(crate) fn count(&self, snapshot_id: i64) -> Option<u32> {
        self.lock()
            .get(&snapshot_id)
            .map(|counter| counter.load(Ordering::SeqCst))
    }

    #[cfg(test)]
    fn entries(&self) -> usize {
        self.lock().len()
    }
}

/// Artifact and history metadata selected in the read that took the lease.
pub struct LeasedDump {
    pub prefix: PathBuf,
    pub l2_tx_count: L2TxCount,
    pub history_version: HistoryVersion,
    pub next_batch_nonce: u64,
    pub guard: LeaseGuard,
}

pub struct FinalizedLease {
    pub inclusion_block: u64,
    pub dump: LeasedDump,
}

impl Storage {
    /// GC keeps the accepted rollback checkpoint, every valid snapshot beyond
    /// it (an intermediate optimistic snapshot may become the next accepted
    /// head) and every leased snapshot. Returns the collected prefixes, whose
    /// directories the caller deletes after this commit (I13).
    pub fn collect_snapshots(&mut self, leases: &SnapshotLeases) -> Result<Vec<PathBuf>> {
        // SQLite's writer lock first, then the registry (`SnapshotLeases`).
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut registry = leases.lock();
        let collected = match collect_unleased_in(&tx, &mut registry) {
            Ok(collected) => collected,
            Err(error) => {
                drop(tx);
                drop(registry);
                return Err(error);
            }
        };
        let committed = tx.commit();
        // Only now may an acquire read: before the commit returns, a reader
        // still sees the rows this transaction deletes.
        drop(registry);
        committed?;
        Ok(collected)
    }

    /// The latest accepted batch must have its own snapshot. A missing artifact
    /// is corruption, never a request to use an older accepted snapshot.
    pub fn finalized_dump(
        &mut self,
    ) -> std::result::Result<Option<FinalizedDump>, FinalizedSelectionError> {
        self.read(|tx| Ok(finalized_dump_in(tx)))?
    }

    pub fn latest_snapshot(&mut self) -> Result<Option<Snapshot>> {
        self.read(|tx| latest_snapshot_in(tx))
    }

    pub fn rollback_snapshot(&mut self) -> Result<Option<Snapshot>> {
        self.read(|tx| rollback_snapshot_in(tx))
    }

    /// Lease the accepted checkpoint; egress calls it on a read-only handle.
    pub fn acquire_finalized_lease(
        &mut self,
        leases: &SnapshotLeases,
    ) -> std::result::Result<Option<FinalizedLease>, FinalizedSelectionError> {
        let leased = leases.lease::<_, FinalizedSelectionError>(self, |tx| {
            let Some(finalized) = finalized_dump_in(tx)? else {
                return Ok(None);
            };
            let history_version = history_version_in(tx)?;
            Ok(Some((finalized.snapshot.id, (finalized, history_version))))
        })?;
        Ok(
            leased.map(|((finalized, history_version), guard)| FinalizedLease {
                inclusion_block: finalized.inclusion_block,
                dump: LeasedDump {
                    prefix: finalized.snapshot.prefix,
                    l2_tx_count: finalized.snapshot.l2_tx_count,
                    next_batch_nonce: finalized.next_batch_nonce,
                    history_version,
                    guard,
                },
            }),
        )
    }

    /// Lease the newest valid snapshot; egress calls it on a read-only handle.
    pub fn acquire_latest_snapshot_lease(
        &mut self,
        leases: &SnapshotLeases,
    ) -> Result<Option<LeasedDump>> {
        let leased = leases.lease::<_, rusqlite::Error>(self, |tx| {
            let Some(snapshot) = latest_snapshot_in(tx)? else {
                return Ok(None);
            };
            let history_version = history_version_in(tx)?;
            let next_batch_nonce = snapshot_next_nonce_in(tx, snapshot.id)?;
            Ok(Some((
                snapshot.id,
                (snapshot, history_version, next_batch_nonce),
            )))
        })?;
        Ok(leased.map(
            |((snapshot, history_version, next_batch_nonce), guard)| LeasedDump {
                prefix: snapshot.prefix,
                l2_tx_count: snapshot.l2_tx_count,
                history_version,
                next_batch_nonce,
                guard,
            },
        ))
    }

    pub fn list_snapshots(&mut self) -> Result<Vec<Snapshot>> {
        self.read(|tx| {
            let mut stmt = tx.prepare(
                "SELECT snapshot_id, prefix, l2_tx_count FROM snapshots ORDER BY snapshot_id",
            )?;
            stmt.query_map([], row_to_snapshot)?.collect()
        })
    }

    pub fn batch_nonce(&mut self, batch_index: u64) -> Result<u64> {
        self.read(|tx| batch_nonce_in(tx, batch_index))
    }
}

/// Select the collectible snapshots, keep the leased ones, drop the registry's
/// zero entries, and delete the remaining rows.
fn collect_unleased_in(tx: &Transaction<'_>, registry: &mut LeaseCounters) -> Result<Vec<PathBuf>> {
    let anchor_id = rollback_snapshot_in(tx)?.map(|snapshot| snapshot.id);
    // Collectible: not the rollback snapshot, and its batch is not a valid
    // closed batch (the baseline included) or is accepted.
    let candidates = {
        let mut stmt = tx.prepare(
            "SELECT s.snapshot_id, s.prefix FROM snapshots s \
             LEFT JOIN valid_closed_batches b ON b.batch_index = s.batch_index \
             WHERE (?1 IS NULL OR s.snapshot_id != ?1) \
               AND (b.batch_index IS NULL OR b.accepted_input IS NOT NULL) \
             ORDER BY s.snapshot_id",
        )?;
        stmt.query_map(params![anchor_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
            ))
        })?
        .collect::<Result<Vec<_>>>()?
    };
    let leased = |id: &i64| {
        registry
            .get(id)
            .is_some_and(|counter| counter.load(Ordering::SeqCst) > 0)
    };
    let collected: Vec<(i64, PathBuf)> = candidates
        .into_iter()
        .filter(|(id, _)| !leased(id))
        .collect();
    registry.retain(|_, counter| counter.load(Ordering::SeqCst) > 0);
    let mut prefixes = Vec::with_capacity(collected.len());
    for (id, prefix) in collected {
        tx.execute("DELETE FROM snapshots WHERE snapshot_id = ?1", [id])?;
        prefixes.push(prefix);
    }
    Ok(prefixes)
}

pub(super) fn insert_baseline_snapshot_in(
    tx: &Transaction<'_>,
    prefix: &Path,
    count: L2TxCount,
) -> Result<i64> {
    insert_snapshot_in(tx, prefix, None, count)
}

pub(super) fn insert_batch_snapshot_in(
    tx: &Transaction<'_>,
    prefix: &Path,
    batch_index: u64,
    count: L2TxCount,
) -> Result<i64> {
    // Registration is composed with sealing, so the batch must already be closed.
    tx.query_row(
        "SELECT batch_index FROM valid_closed_batches WHERE batch_index = ?1",
        [u64_to_i64(batch_index)],
        |_| Ok(()),
    )?;
    insert_snapshot_in(tx, prefix, Some(batch_index), count)
}

fn insert_snapshot_in(
    tx: &Transaction<'_>,
    prefix: &Path,
    batch_index: Option<u64>,
    count: L2TxCount,
) -> Result<i64> {
    assert_eq!(
        count,
        next_l2_tx_count_in(tx)?,
        "snapshot count differs from canonical storage history"
    );
    tx.execute(
        "INSERT INTO snapshots (batch_index, l2_tx_count, prefix) VALUES (?1, ?2, ?3)",
        params![
            batch_index.map(u64_to_i64),
            u64_to_i64(count.get()),
            prefix.to_string_lossy().as_ref()
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Required by recovery/admission inside their existing transaction.
pub(super) fn has_rollback_safe_snapshot_in(conn: &Connection) -> Result<bool> {
    Ok(rollback_snapshot_in(conn)?.is_some())
}

fn rollback_snapshot_in(conn: &Connection) -> Result<Option<Snapshot>> {
    match latest_accepted_boundary_in(conn)? {
        Some((batch_index, _, _)) => snapshot_for_batch_in(conn, batch_index).map(Some),
        None => baseline_snapshot_in(conn),
    }
}

/// The greatest accepted batch: `(batch_index, nonce, inclusion block)`, the
/// block of the safe input that carried its accepted landing.
fn latest_accepted_boundary_in(conn: &Connection) -> Result<Option<(u64, u64, u64)>> {
    conn.query_row(
        "SELECT b.batch_index, b.nonce, s.block_number FROM batches b \
         JOIN safe_inputs s ON s.safe_input_index = b.accepted_input \
         WHERE b.accepted_input = (SELECT MAX(accepted_input) FROM batches)",
        [],
        |row| {
            Ok((
                i64_to_u64(row.get(0)?),
                i64_to_u64(row.get(1)?),
                i64_to_u64(row.get(2)?),
            ))
        },
    )
    .optional()
}

fn snapshot_for_batch_in(conn: &Connection, batch_index: u64) -> Result<Snapshot> {
    conn.query_row(
        "SELECT snapshot_id, prefix, l2_tx_count FROM snapshots WHERE batch_index = ?1",
        [u64_to_i64(batch_index)],
        row_to_snapshot,
    )
}

fn baseline_snapshot_in(conn: &Connection) -> Result<Option<Snapshot>> {
    conn.query_row(
        "SELECT snapshot_id, prefix, l2_tx_count FROM snapshots WHERE batch_index IS NULL",
        [],
        row_to_snapshot,
    )
    .optional()
}

pub(super) fn finalized_dump_in(
    conn: &Connection,
) -> std::result::Result<Option<FinalizedDump>, FinalizedSelectionError> {
    // A matched batch can precede a divergent accepted batch in the same L1
    // block. Its snapshot cannot represent that complete block boundary.
    if super::acceptance::canonical_divergence_in(conn)?.is_some() {
        return Err(FinalizedSelectionError::CanonicalDivergence);
    }
    if let Some((batch_index, nonce, inclusion_block)) = latest_accepted_boundary_in(conn)? {
        return Ok(Some(FinalizedDump {
            snapshot: snapshot_for_batch_in(conn, batch_index)?,
            inclusion_block,
            next_batch_nonce: nonce.checked_add(1).expect("accepted nonce overflow"),
        }));
    }
    // The baseline is the application's state at the end of the stop block,
    // which the canonical machine reaches too, so it is the era's first
    // comparison checkpoint, genesis base or not.
    let Some(snapshot) = baseline_snapshot_in(conn)? else {
        return Ok(None);
    };
    let era = era_in(conn)?;
    assert_eq!(
        snapshot.l2_tx_count, era.rebuilt_end,
        "the era baseline sits at the rebuilt history's end"
    );
    Ok(Some(FinalizedDump {
        snapshot,
        inclusion_block: era.stop_block,
        next_batch_nonce: era.anchor_batch_nonce,
    }))
}

fn latest_snapshot_in(conn: &Connection) -> Result<Option<Snapshot>> {
    let batch_index: Option<i64> = conn
        .query_row(
            "SELECT batch_index FROM valid_closed_batches ORDER BY batch_index DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match batch_index {
        Some(index) => snapshot_for_batch_in(conn, i64_to_u64(index)).map(Some),
        None => baseline_snapshot_in(conn),
    }
}

fn snapshot_next_nonce_in(conn: &Connection, snapshot_id: i64) -> Result<u64> {
    let batch_index: Option<i64> = conn.query_row(
        "SELECT batch_index FROM snapshots WHERE snapshot_id = ?1",
        [snapshot_id],
        |r| r.get(0),
    )?;
    match batch_index {
        Some(index) => Ok(batch_nonce_in(conn, i64_to_u64(index))?
            .checked_add(1)
            .expect("batch nonce overflow")),
        None => Ok(era_in(conn)?.anchor_batch_nonce),
    }
}

pub(super) fn batch_nonce_in(conn: &Connection, batch_index: u64) -> Result<u64> {
    conn.query_row(
        "SELECT nonce FROM batches WHERE batch_index = ?1",
        [u64_to_i64(batch_index)],
        |row| Ok(i64_to_u64(row.get(0)?)),
    )
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> Result<Snapshot> {
    Ok(Snapshot {
        id: row.get(0)?,
        prefix: PathBuf::from(row.get::<_, String>(1)?),
        l2_tx_count: L2TxCount::new(i64_to_u64(row.get(2)?)),
    })
}

#[cfg(test)]
mod tests;
