// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Immutable batch-close and baseline snapshots, derived acceptance, and leases.
//! Files are durable before registration; GC deletes rows before their files.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Result, Transaction, params};
use sequencer_core::history::{ExecutedInputCount, HistoryVersion};

use super::convert::{i64_to_u64, u64_to_i64};
use super::history::{next_executed_input_count_in, query_history_state};
use super::{Storage, is_persistent_storage_error, is_persistent_storage_open_error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpRow {
    pub id: i64,
    pub prefix: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub dump: DumpRow,
    pub executed_input_count: ExecutedInputCount,
}

/// A batch-close snapshot whose complete predicted prefix was accepted on L1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedDump {
    pub dump: DumpRow,
    pub inclusion_block: u64,
    pub next_batch_nonce: u64,
    pub executed_input_count: ExecutedInputCount,
}

pub type ReleaseScheduler = Arc<dyn Fn(Box<dyn FnOnce() + Send + 'static>) + Send + Sync + 'static>;
pub type PersistentReleaseFailureReporter = Arc<dyn Fn(&str) + Send + Sync + 'static>;

/// An armed lease release, inseparable from the lease it holds. Handed out
/// bundled inside a [`LeasedDump`]: while it lives, `lease_count > 0` keeps GC
/// off the dump; its `Drop` releases the lease. The release re-opens a brief
/// writer connection from the storage path (the egress handlers open per-op,
/// so the guard owns the path rather than borrowing a `Storage`) and runs via
/// the injected [`ReleaseScheduler`]. Dropping it without a runtime still
/// releases — just inline. `reset_dump_leases` at startup is the crash backstop.
pub struct LeaseGuard {
    path: String,
    dump_id: i64,
    schedule: ReleaseScheduler,
    report_persistent_failure: PersistentReleaseFailureReporter,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.path);
        let dump_id = self.dump_id;
        let report_persistent_failure = self.report_persistent_failure.clone();
        (self.schedule)(Box::new(move || match Storage::open_writer(&path) {
            Ok(mut storage) => {
                if let Err(err) = storage.release_dump_lease(dump_id) {
                    if is_persistent_storage_error(&err) {
                        report_persistent_failure(&format!(
                            "snapshot lease release for dump {dump_id} failed persistently: {err}"
                        ));
                    } else {
                        tracing::warn!(
                            error = %err, dump_id,
                            "snapshot lease release failed; will be reset at next startup",
                        );
                    }
                }
            }
            Err(err) => {
                if is_persistent_storage_open_error(&err) {
                    report_persistent_failure(&format!(
                        "snapshot lease release for dump {dump_id}: writer open failed persistently: {err}"
                    ));
                } else {
                    tracing::warn!(
                        error = %err, dump_id,
                        "snapshot lease release: open failed; will be reset at next startup",
                    );
                }
            }
        }));
    }
}

/// Artifact and history metadata selected atomically with the lease increment.
pub struct LeasedDump {
    pub prefix: PathBuf,
    pub executed_input_count: ExecutedInputCount,
    pub history_version: HistoryVersion,
    pub next_batch_nonce: u64,
    pub guard: LeaseGuard,
}

pub struct FinalizedLease {
    pub inclusion_block: u64,
    pub dump: LeasedDump,
}

impl Storage {
    pub fn release_dump_lease(&mut self, dump_id: i64) -> Result<()> {
        self.write(|tx| release_dump_lease_in(tx, dump_id))
    }

    pub fn reset_dump_leases(&mut self) -> Result<usize> {
        self.write(|tx| tx.execute("UPDATE dumps SET lease_count = 0", []))
    }

    pub fn dump_lease_count(&mut self, dump_id: i64) -> Result<Option<i64>> {
        self.read(|tx| {
            tx.query_row(
                "SELECT lease_count FROM dumps WHERE id = ?1",
                [dump_id],
                |r| r.get(0),
            )
            .optional()
        })
    }

    /// GC keeps the accepted rollback checkpoint and every valid snapshot beyond
    /// it. Intermediate optimistic snapshots may become the next accepted head.
    /// Row selection/deletion serializes with lease acquisition; files follow.
    pub fn gc_unreferenced_dumps(&mut self) -> Result<Vec<DumpRow>> {
        self.write(|tx| {
            let anchor = rollback_snapshot_in(tx)?;
            let anchor_id = anchor.as_ref().map(|s| s.dump.id);
            let accepted_nonce =
                latest_accepted_boundary_in(tx)?.map(|(_, nonce, _)| u64_to_i64(nonce));
            let candidates = {
                let mut stmt = tx.prepare(
                    "SELECT d.id, d.prefix FROM dumps d \
                     LEFT JOIN snapshots s ON s.dump_id = d.id \
                     LEFT JOIN valid_closed_batches b ON b.batch_index = s.batch_index \
                     WHERE d.lease_count = 0 AND (?1 IS NULL OR d.id != ?1) \
                       AND (b.batch_index IS NULL OR (?2 IS NOT NULL AND b.nonce <= ?2)) \
                     ORDER BY d.id",
                )?;
                stmt.query_map(params![anchor_id, accepted_nonce], row_to_dump_row)?
                    .collect::<Result<Vec<_>>>()?
            };
            for dump in &candidates {
                tx.execute("DELETE FROM snapshots WHERE dump_id = ?1", [dump.id])?;
                tx.execute("DELETE FROM dumps WHERE id = ?1", [dump.id])?;
            }
            Ok(candidates)
        })
    }

    /// The latest accepted batch must have its own snapshot. A missing artifact
    /// is corruption, never a request to use an older accepted snapshot.
    pub fn finalized_dump(&mut self) -> Result<Option<FinalizedDump>> {
        self.read(|tx| finalized_dump_in(tx))
    }

    pub fn latest_snapshot(&mut self) -> Result<Option<Snapshot>> {
        self.read(|tx| latest_snapshot_in(tx))
    }

    pub fn rollback_snapshot(&mut self) -> Result<Option<Snapshot>> {
        self.read(|tx| rollback_snapshot_in(tx))
    }

    pub fn has_rollback_safe_snapshot(&mut self) -> Result<bool> {
        self.read(|tx| has_rollback_safe_snapshot_in(tx))
    }

    pub fn acquire_finalized_lease(
        &mut self,
        schedule: ReleaseScheduler,
        report_persistent_failure: PersistentReleaseFailureReporter,
    ) -> Result<Option<FinalizedLease>> {
        let acquired = self.write(|tx| {
            let Some(snapshot) = finalized_dump_in(tx)? else {
                return Ok(None);
            };
            let history_version = query_history_state(tx)?.version;
            acquire_dump_lease_in(tx, snapshot.dump.id)?;
            Ok(Some((snapshot, history_version)))
        })?;
        Ok(acquired.map(|(snapshot, history_version)| FinalizedLease {
            inclusion_block: snapshot.inclusion_block,
            dump: LeasedDump {
                prefix: snapshot.dump.prefix,
                executed_input_count: snapshot.executed_input_count,
                next_batch_nonce: snapshot.next_batch_nonce,
                history_version,
                // Only a committed increment arms a release. A failed COMMIT
                // rolls back the lease and must never schedule its decrement.
                guard: LeaseGuard {
                    path: self.path.clone(),
                    dump_id: snapshot.dump.id,
                    schedule,
                    report_persistent_failure,
                },
            },
        }))
    }

    pub fn acquire_latest_snapshot_lease(
        &mut self,
        schedule: ReleaseScheduler,
        report_persistent_failure: PersistentReleaseFailureReporter,
    ) -> Result<Option<LeasedDump>> {
        let acquired = self.write(|tx| {
            let Some(snapshot) = latest_snapshot_in(tx)? else {
                return Ok(None);
            };
            let history_version = query_history_state(tx)?.version;
            let next_batch_nonce = snapshot_next_nonce_in(tx, snapshot.dump.id)?;
            acquire_dump_lease_in(tx, snapshot.dump.id)?;
            Ok(Some((snapshot, history_version, next_batch_nonce)))
        })?;
        Ok(
            acquired.map(|(snapshot, history_version, next_batch_nonce)| LeasedDump {
                prefix: snapshot.dump.prefix,
                executed_input_count: snapshot.executed_input_count,
                history_version,
                next_batch_nonce,
                guard: LeaseGuard {
                    path: self.path.clone(),
                    dump_id: snapshot.dump.id,
                    schedule,
                    report_persistent_failure,
                },
            }),
        )
    }

    pub fn list_dump_rows(&mut self) -> Result<Vec<DumpRow>> {
        self.read(|tx| {
            let mut stmt = tx.prepare("SELECT id, prefix FROM dumps ORDER BY id")?;
            stmt.query_map([], row_to_dump_row)?.collect()
        })
    }

    pub fn batch_nonce(&mut self, batch_index: u64) -> Result<u64> {
        self.read(|tx| batch_nonce_in(tx, batch_index))
    }

    #[cfg(test)]
    pub(crate) fn insert_batch_snapshot(&mut self, prefix: &Path, batch_index: u64) -> Result<i64> {
        self.write(|tx| {
            insert_batch_snapshot_in(tx, prefix, batch_index, next_executed_input_count_in(tx)?)
        })
    }

    #[cfg(test)]
    pub(crate) fn insert_baseline_snapshot(
        &mut self,
        prefix: &Path,
        count: ExecutedInputCount,
    ) -> Result<i64> {
        self.write(|tx| insert_baseline_snapshot_in(tx, prefix, count))
    }
}

pub(super) fn insert_baseline_snapshot_in(
    tx: &Transaction<'_>,
    prefix: &Path,
    count: ExecutedInputCount,
) -> Result<i64> {
    insert_snapshot_in(tx, prefix, None, count)
}

pub(super) fn insert_batch_snapshot_in(
    tx: &Transaction<'_>,
    prefix: &Path,
    batch_index: u64,
    count: ExecutedInputCount,
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
    count: ExecutedInputCount,
) -> Result<i64> {
    assert_eq!(
        count,
        next_executed_input_count_in(tx)?,
        "snapshot count differs from canonical storage history"
    );
    tx.execute(
        "INSERT INTO dumps (prefix) VALUES (?1)",
        [prefix.to_string_lossy().as_ref()],
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO snapshots (dump_id, batch_index, executed_input_count) VALUES (?1, ?2, ?3)",
        params![id, batch_index.map(u64_to_i64), u64_to_i64(count.get())],
    )?;
    Ok(id)
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

fn latest_accepted_boundary_in(conn: &Connection) -> Result<Option<(u64, u64, u64)>> {
    conn.query_row(
        "SELECT b.batch_index, a.nonce, a.inclusion_block FROM safe_accepted_batches a \
         LEFT JOIN valid_closed_batches b ON b.nonce = a.nonce \
         ORDER BY a.safe_input_index DESC LIMIT 1",
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
        "SELECT s.dump_id, d.prefix, s.executed_input_count FROM snapshots s \
         LEFT JOIN dumps d ON d.id = s.dump_id WHERE s.batch_index = ?1",
        [u64_to_i64(batch_index)],
        row_to_snapshot,
    )
}

fn baseline_snapshot_in(conn: &Connection) -> Result<Option<Snapshot>> {
    conn.query_row(
        "SELECT s.dump_id, d.prefix, s.executed_input_count FROM snapshots s \
         LEFT JOIN dumps d ON d.id = s.dump_id WHERE s.batch_index IS NULL",
        [],
        row_to_snapshot,
    )
    .optional()
}

fn finalized_dump_in(conn: &Connection) -> Result<Option<FinalizedDump>> {
    if let Some((batch_index, nonce, inclusion_block)) = latest_accepted_boundary_in(conn)? {
        let snapshot = snapshot_for_batch_in(conn, batch_index)?;
        return Ok(Some(FinalizedDump {
            dump: snapshot.dump,
            executed_input_count: snapshot.executed_input_count,
            inclusion_block,
            next_batch_nonce: nonce.checked_add(1).expect("accepted nonce overflow"),
        }));
    }
    // Genesis is independently known canonical. A rebuilt baseline may contain
    // the recovery fold's speculative final drain and is never comparable at C.
    let Some(snapshot) = baseline_snapshot_in(conn)? else {
        return Ok(None);
    };
    let history = query_history_state(conn)?;
    if history.base_safe_block == 0 && history.base_executed_input_count == 0 {
        Ok(Some(FinalizedDump {
            dump: snapshot.dump,
            executed_input_count: snapshot.executed_input_count,
            inclusion_block: 0,
            next_batch_nonce: super::mutations::batch_tree_anchor_in(conn)?,
        }))
    } else {
        Ok(None)
    }
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

fn snapshot_next_nonce_in(conn: &Connection, dump_id: i64) -> Result<u64> {
    let batch_index: Option<i64> = conn.query_row(
        "SELECT batch_index FROM snapshots WHERE dump_id = ?1",
        [dump_id],
        |r| r.get(0),
    )?;
    match batch_index {
        Some(index) => Ok(batch_nonce_in(conn, i64_to_u64(index))?
            .checked_add(1)
            .expect("batch nonce overflow")),
        None => super::mutations::batch_tree_anchor_in(conn),
    }
}

fn acquire_dump_lease_in(tx: &Transaction<'_>, id: i64) -> Result<()> {
    let changed = tx.execute(
        "UPDATE dumps SET lease_count = lease_count + 1 WHERE id = ?1",
        [id],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

fn release_dump_lease_in(tx: &Transaction<'_>, id: i64) -> Result<()> {
    let changed = tx.execute(
        "UPDATE dumps SET lease_count = lease_count - 1 WHERE id = ?1",
        [id],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

pub(super) fn batch_nonce_in(conn: &Connection, batch_index: u64) -> Result<u64> {
    conn.query_row(
        "SELECT nonce FROM batches WHERE batch_index = ?1",
        [u64_to_i64(batch_index)],
        |row| Ok(i64_to_u64(row.get(0)?)),
    )
}

fn row_to_dump_row(row: &rusqlite::Row<'_>) -> Result<DumpRow> {
    Ok(DumpRow {
        id: row.get(0)?,
        prefix: PathBuf::from(row.get::<_, String>(1)?),
    })
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> Result<Snapshot> {
    Ok(Snapshot {
        dump: row_to_dump_row(row)?,
        executed_input_count: ExecutedInputCount::new(i64_to_u64(row.get(2)?)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp};
    use crate::storage::history::{advance_recovery_generation_in, initialize_history_in};
    use crate::storage::test_helpers::{
        SENDER_A, pin_test_deployment_identity, seed_safe_inputs_with_batch_nonces, temp_db,
    };
    use crate::storage::{LifecycleCommand, SafeInputRange, WriteHead};
    use alloy_primitives::{Address, Signature};
    use sequencer_core::user_op::{SignedUserOp, UserOp};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::SystemTime;

    fn inline(release: Box<dyn FnOnce() + Send>) {
        release();
    }
    fn reporter() -> PersistentReleaseFailureReporter {
        Arc::new(|_| {})
    }
    fn prefix(n: u64) -> PathBuf {
        PathBuf::from(format!("/snapshot/{n}"))
    }

    fn close(storage: &mut Storage, head: &mut WriteHead) -> i64 {
        let index = head.batch_index;
        let safe_block = head.safe_block;
        let count = storage.next_executed_input_count().unwrap();
        storage
            .close_frame_and_batch_with_snapshot(head, safe_block, &prefix(index + 1), index, count)
            .unwrap();
        storage.latest_snapshot().unwrap().unwrap().dump.id
    }

    #[test]
    fn acceptance_derives_checkpoint_and_gc_preserves_every_future_candidate() {
        let db = temp_db("derived-snapshot-gc");
        let mut storage = Storage::open(&db.path).unwrap();
        pin_test_deployment_identity(&mut storage, SENDER_A);
        let baseline = storage
            .insert_baseline_snapshot(&prefix(0), ExecutedInputCount::ZERO)
            .unwrap();
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        let first = close(&mut storage, &mut head);
        let second = close(&mut storage, &mut head);
        let third = close(&mut storage, &mut head);
        assert!(storage.gc_unreferenced_dumps().unwrap().is_empty());
        assert_eq!(storage.finalized_dump().unwrap().unwrap().dump.id, baseline);
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 1, &[0]);
        assert_eq!(storage.finalized_dump().unwrap().unwrap().dump.id, first);
        assert_eq!(
            storage
                .gc_unreferenced_dumps()
                .unwrap()
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![baseline]
        );
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 2, &[1]);
        assert_eq!(storage.finalized_dump().unwrap().unwrap().dump.id, second);
        assert_eq!(storage.latest_snapshot().unwrap().unwrap().dump.id, third);
        assert_eq!(
            storage
                .gc_unreferenced_dumps()
                .unwrap()
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![first]
        );
    }

    #[test]
    fn missing_exact_accepted_snapshot_never_falls_back_or_collects() {
        let db = temp_db("missing-accepted-snapshot");
        let mut storage = Storage::open(&db.path).unwrap();
        pin_test_deployment_identity(&mut storage, SENDER_A);
        storage
            .insert_baseline_snapshot(&prefix(0), ExecutedInputCount::ZERO)
            .unwrap();
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        close(&mut storage, &mut head);
        close(&mut storage, &mut head);
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 1, &[0, 1]);
        storage
            .conn
            .execute("DELETE FROM snapshots WHERE batch_index=1", [])
            .unwrap();
        assert!(matches!(
            storage.finalized_dump(),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        assert!(matches!(
            storage.latest_snapshot(),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        assert!(storage.has_rollback_safe_snapshot().is_err());
        assert!(storage.gc_unreferenced_dumps().is_err());
        assert_eq!(storage.list_dump_rows().unwrap().len(), 3);
    }

    #[test]
    fn rebuilt_baseline_is_restorable_but_not_an_accepted_comparison_checkpoint() {
        let db = temp_db("rebuilt-snapshot");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
        storage
            .write(|tx| {
                initialize_history_in(tx, ExecutedInputCount::new(41), 100)?;
                insert_baseline_snapshot_in(tx, &prefix(0), ExecutedInputCount::new(41))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            storage
                .latest_snapshot()
                .unwrap()
                .unwrap()
                .executed_input_count
                .get(),
            41
        );
        assert!(storage.finalized_dump().unwrap().is_none());
        assert!(storage.has_rollback_safe_snapshot().unwrap());
        assert!(storage.gc_unreferenced_dumps().unwrap().is_empty());
    }

    #[test]
    fn leases_keep_artifact_count_and_version_together_after_acceptance_and_generation_change() {
        let db = temp_db("snapshot-version");
        let mut storage = Storage::open(&db.path).unwrap();
        pin_test_deployment_identity(&mut storage, SENDER_A);
        let baseline = storage
            .insert_baseline_snapshot(&prefix(0), ExecutedInputCount::ZERO)
            .unwrap();
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        let old = storage
            .acquire_latest_snapshot_lease(Arc::new(inline), reporter())
            .unwrap()
            .unwrap();
        let old_version = old.history_version;
        let (respond_to, _) = tokio::sync::oneshot::channel();
        storage
            .append_executed_user_ops_chunk(
                &mut head,
                &[IncludedUserOp {
                    pending: PendingUserOp {
                        signed: SignedUserOp {
                            sender: Address::ZERO,
                            signature: Signature::test_signature(),
                            user_op: UserOp {
                                nonce: 0,
                                max_fee: u16::MAX,
                                data: vec![].into(),
                            },
                        },
                        respond_to,
                        received_at: SystemTime::now(),
                    },
                    executed_input_offset: ExecutedInputCount::ZERO,
                }],
            )
            .unwrap();
        close(&mut storage, &mut head);
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 1, &[0]);
        storage.write(advance_recovery_generation_in).unwrap();
        let new = storage
            .acquire_finalized_lease(Arc::new(inline), reporter())
            .unwrap()
            .unwrap();
        assert_eq!(old.prefix, prefix(0));
        assert_eq!(old.executed_input_count, ExecutedInputCount::ZERO);
        assert_eq!(old.history_version, old_version);
        assert_eq!(new.dump.executed_input_count.get(), 1);
        assert_eq!(new.dump.next_batch_nonce, 1);
        assert_ne!(new.dump.history_version, old_version);
        assert!(storage.gc_unreferenced_dumps().unwrap().is_empty());
        drop(old);
        assert_eq!(storage.gc_unreferenced_dumps().unwrap()[0].id, baseline);
    }

    #[test]
    fn invalidated_snapshot_is_not_selected_and_its_lease_blocks_collection() {
        let db = temp_db("invalidated-snapshot");
        let mut storage = Storage::open(&db.path).unwrap();
        storage
            .insert_baseline_snapshot(&prefix(0), ExecutedInputCount::ZERO)
            .unwrap();
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        let id = close(&mut storage, &mut head);
        let lease = storage
            .acquire_latest_snapshot_lease(Arc::new(inline), reporter())
            .unwrap()
            .unwrap();
        storage
            .conn
            .execute(
                "UPDATE batches SET invalidated_at_ms=1 WHERE batch_index=0",
                [],
            )
            .unwrap();
        assert_eq!(
            storage.latest_snapshot().unwrap().unwrap().dump.prefix,
            prefix(0)
        );
        assert!(storage.gc_unreferenced_dumps().unwrap().is_empty());
        drop(lease);
        assert_eq!(storage.gc_unreferenced_dumps().unwrap()[0].id, id);
    }

    #[test]
    fn failed_lease_commit_never_arms_a_release() {
        let db = temp_db("failed-lease-commit");
        let mut storage = Storage::open(&db.path).unwrap();
        let id = storage
            .insert_baseline_snapshot(&prefix(0), ExecutedInputCount::ZERO)
            .unwrap();
        storage.conn.execute_batch(
            "CREATE TABLE lease_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE lease_child(parent_id INTEGER REFERENCES lease_parent(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER fail_lease AFTER UPDATE OF lease_count ON dumps WHEN NEW.lease_count > OLD.lease_count
             BEGIN INSERT INTO lease_child VALUES(1); END;"
        ).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let schedule: ReleaseScheduler = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert!(
            storage
                .acquire_latest_snapshot_lease(schedule.clone(), reporter())
                .is_err()
        );
        assert!(
            storage
                .acquire_finalized_lease(schedule, reporter())
                .is_err()
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(storage.dump_lease_count(id).unwrap(), Some(0));
    }

    #[test]
    fn failed_history_query_never_leases_or_arms_a_release() {
        let db = temp_db("failed-lease-query");
        let mut storage = Storage::open(&db.path).unwrap();
        let id = storage
            .insert_baseline_snapshot(&prefix(0), ExecutedInputCount::ZERO)
            .unwrap();
        storage
            .conn
            .execute_batch("DROP TABLE history_state")
            .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let schedule: ReleaseScheduler = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert!(
            storage
                .acquire_latest_snapshot_lease(schedule.clone(), reporter())
                .is_err()
        );
        assert!(
            storage
                .acquire_finalized_lease(schedule, reporter())
                .is_err()
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(storage.dump_lease_count(id).unwrap(), Some(0));
    }

    #[test]
    fn persistent_release_failure_reaches_reporter() {
        let db = temp_db("persistent-lease");
        let _storage = Storage::open(&db.path).unwrap();
        let reported = Arc::new(AtomicBool::new(false));
        let flag = reported.clone();
        drop(LeaseGuard {
            path: db.path,
            dump_id: i64::MAX,
            schedule: Arc::new(inline),
            report_persistent_failure: Arc::new(move |_| {
                flag.store(true, Ordering::SeqCst);
            }),
        });
        assert!(reported.load(Ordering::SeqCst));
    }
}
