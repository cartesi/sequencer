// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use super::*;
use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp};
use crate::storage::history::advance_recovery_generation_in;
use crate::storage::test_helpers::{SENDER_A, era, seed_safe_inputs_with_batch_nonces};
use crate::storage::{LifecycleCommand, SafeInputRange, WriteHead};
use crate::test_support::{pin_test_deployment_identity, temp_db};
use alloy_primitives::{Address, Signature};
use sequencer_core::user_op::{SignedUserOp, UserOp};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::SystemTime;

fn prefix(n: u64) -> PathBuf {
    PathBuf::from(format!("/snapshot/{n}"))
}

/// Close the open batch with a snapshot at `prefix(batch_index + 1)`.
fn close(storage: &mut Storage, head: &mut WriteHead) -> i64 {
    let index = head.batch_index;
    let safe_block = head.safe_block;
    let count = storage.next_l2_tx_count().unwrap();
    storage
        .close_frame_and_batch_with_snapshot(head, safe_block, &prefix(index + 1), count)
        .unwrap();
    storage.latest_snapshot().unwrap().unwrap().id
}

/// A baseline at `prefix(0)` and an open genesis Tip.
fn baseline_and_tip(name: &str) -> (crate::test_support::TestDb, Storage, i64, WriteHead) {
    let db = temp_db(name);
    let mut storage = Storage::open(&db.path).unwrap();
    pin_test_deployment_identity(&mut storage, SENDER_A);
    let baseline = storage
        .insert_baseline_snapshot(&prefix(0), L2TxCount::ZERO)
        .unwrap();
    let head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .unwrap();
    (db, storage, baseline, head)
}

#[test]
fn acceptance_derives_checkpoint_and_gc_preserves_every_future_candidate() {
    let (_db, mut storage, baseline, mut head) = baseline_and_tip("derived-snapshot-gc");
    let leases = SnapshotLeases::default();
    let first = close(&mut storage, &mut head);
    let second = close(&mut storage, &mut head);
    let third = close(&mut storage, &mut head);
    assert!(storage.collect_snapshots(&leases).unwrap().is_empty());
    assert_eq!(
        storage.finalized_dump().unwrap().unwrap().snapshot.id,
        baseline
    );
    seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 1, &[0]);
    assert_eq!(
        storage.finalized_dump().unwrap().unwrap().snapshot.id,
        first
    );
    assert_eq!(storage.collect_snapshots(&leases).unwrap(), vec![prefix(0)]);
    seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 2, &[1]);
    assert_eq!(
        storage.finalized_dump().unwrap().unwrap().snapshot.id,
        second
    );
    assert_eq!(storage.latest_snapshot().unwrap().unwrap().id, third);
    assert_eq!(storage.collect_snapshots(&leases).unwrap(), vec![prefix(1)]);
}

#[test]
fn missing_exact_accepted_snapshot_never_falls_back_or_collects() {
    let (_db, mut storage, _, mut head) = baseline_and_tip("missing-accepted-snapshot");
    close(&mut storage, &mut head);
    close(&mut storage, &mut head);
    seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 1, &[0, 1]);
    storage
        .conn
        .execute("DELETE FROM snapshots WHERE batch_index=1", [])
        .unwrap();
    assert!(matches!(
        storage.finalized_dump(),
        Err(FinalizedSelectionError::Storage(
            rusqlite::Error::QueryReturnedNoRows
        ))
    ));
    assert!(matches!(
        storage.latest_snapshot(),
        Err(rusqlite::Error::QueryReturnedNoRows)
    ));
    assert!(storage.has_rollback_safe_snapshot().is_err());
    assert!(
        storage
            .collect_snapshots(&SnapshotLeases::default())
            .is_err()
    );
    assert_eq!(storage.list_snapshots().unwrap().len(), 2);
}

#[test]
fn the_era_baseline_is_the_comparison_checkpoint_at_the_stop_block() {
    let db = temp_db("rebuilt-snapshot");
    let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
    pin_test_deployment_identity(&mut storage, SENDER_A);
    storage
        .publish_era(
            &prefix(0),
            &crate::storage::NewEra {
                anchor_batch_nonce: 7,
                ..era(41, 41, 100)
            },
        )
        .unwrap();
    assert_eq!(
        storage
            .latest_snapshot()
            .unwrap()
            .unwrap()
            .l2_tx_count
            .get(),
        41
    );
    let checkpoint = storage.finalized_dump().unwrap().expect("baseline");
    assert_eq!(checkpoint.snapshot.prefix, prefix(0));
    assert_eq!(
        (
            checkpoint.inclusion_block,
            checkpoint.snapshot.l2_tx_count.get(),
            checkpoint.next_batch_nonce
        ),
        (100, 41, 7)
    );
    assert!(storage.has_rollback_safe_snapshot().unwrap());
    assert!(
        storage
            .collect_snapshots(&SnapshotLeases::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn snapshot_prefix_is_immutable() {
    let (_db, storage, baseline, _) = baseline_and_tip("snapshot-prefix-immutable");
    let error = storage
        .conn
        .execute(
            "UPDATE snapshots SET prefix = '/elsewhere' WHERE snapshot_id = ?1",
            [baseline],
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("snapshot boundaries are immutable"),
        "{error}"
    );
}

#[test]
fn leases_keep_artifact_count_and_version_together_after_acceptance_and_generation_change() {
    let (_db, mut storage, baseline, mut head) = baseline_and_tip("snapshot-version");
    let leases = SnapshotLeases::default();
    let old = storage
        .acquire_latest_snapshot_lease(&leases)
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
                l2_tx_index: L2TxCount::ZERO,
            }],
        )
        .unwrap();
    close(&mut storage, &mut head);
    seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 1, &[0]);
    storage.write(advance_recovery_generation_in).unwrap();
    let new = storage.acquire_finalized_lease(&leases).unwrap().unwrap();
    assert_eq!(old.prefix, prefix(0));
    assert_eq!(old.l2_tx_count, L2TxCount::ZERO);
    assert_eq!(old.history_version, old_version);
    assert_eq!(new.dump.l2_tx_count.get(), 1);
    assert_eq!(new.dump.next_batch_nonce, 1);
    assert_ne!(new.dump.history_version, old_version);
    assert!(storage.collect_snapshots(&leases).unwrap().is_empty());
    assert_eq!(leases.count(baseline), Some(1));
    drop(old);
    assert_eq!(storage.collect_snapshots(&leases).unwrap(), vec![prefix(0)]);
}

#[test]
fn invalidated_snapshot_is_not_selected_and_its_lease_blocks_collection() {
    let (_db, mut storage, _, mut head) = baseline_and_tip("invalidated-snapshot");
    let leases = SnapshotLeases::default();
    close(&mut storage, &mut head);
    let lease = storage
        .acquire_latest_snapshot_lease(&leases)
        .unwrap()
        .unwrap();
    assert_eq!(lease.prefix, prefix(1));
    storage
        .conn
        .execute(
            "UPDATE batches SET invalidated_at_ms=1 WHERE batch_index=0",
            [],
        )
        .unwrap();
    assert_eq!(
        storage.latest_snapshot().unwrap().unwrap().prefix,
        prefix(0)
    );
    assert!(storage.collect_snapshots(&leases).unwrap().is_empty());
    drop(lease);
    assert_eq!(storage.collect_snapshots(&leases).unwrap(), vec![prefix(1)]);
}

#[test]
fn a_lease_taken_before_gc_keeps_its_row_until_the_guard_drops() {
    let (_db, mut storage, baseline, mut head) = baseline_and_tip("lease-before-gc");
    let leases = SnapshotLeases::default();
    let lease = storage.acquire_finalized_lease(&leases).unwrap().unwrap();
    assert_eq!(lease.dump.prefix, prefix(0));
    close(&mut storage, &mut head);
    seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 1, &[0]);

    assert!(storage.collect_snapshots(&leases).unwrap().is_empty());
    assert!(
        storage
            .list_snapshots()
            .unwrap()
            .iter()
            .any(|snapshot| snapshot.id == baseline),
        "a leased row is never collected"
    );
    drop(lease);
    assert_eq!(leases.count(baseline), Some(0));
    assert_eq!(storage.collect_snapshots(&leases).unwrap(), vec![prefix(0)]);
    assert!(
        storage
            .list_snapshots()
            .unwrap()
            .iter()
            .all(|snapshot| snapshot.id != baseline)
    );
}

#[test]
fn gc_removes_zero_entries_and_keeps_live_ones() {
    let (_db, mut storage, baseline, mut head) = baseline_and_tip("lease-zero-entries");
    let leases = SnapshotLeases::default();
    drop(storage.acquire_finalized_lease(&leases).unwrap().unwrap());
    let first = close(&mut storage, &mut head);
    let live = storage
        .acquire_latest_snapshot_lease(&leases)
        .unwrap()
        .unwrap();
    assert_eq!(leases.entries(), 2);
    assert_eq!(leases.count(baseline), Some(0));

    // Nothing is collectible yet: removing zero entries does not need a candidate.
    assert!(storage.collect_snapshots(&leases).unwrap().is_empty());
    assert_eq!(leases.entries(), 1);
    assert_eq!(leases.count(baseline), None);
    assert_eq!(leases.count(first), Some(1));
    drop(live);
    assert!(storage.collect_snapshots(&leases).unwrap().is_empty());
    assert_eq!(leases.entries(), 0);
}

#[test]
fn two_leases_on_one_snapshot_both_keep_it() {
    let (_db, mut storage, baseline, mut head) = baseline_and_tip("two-leases");
    let leases = SnapshotLeases::default();
    let first = storage.acquire_finalized_lease(&leases).unwrap().unwrap();
    let second = storage
        .acquire_latest_snapshot_lease(&leases)
        .unwrap()
        .unwrap();
    assert_eq!(
        (first.dump.prefix.clone(), second.prefix.clone()),
        (prefix(0), prefix(0))
    );
    assert_eq!(leases.count(baseline), Some(2));
    close(&mut storage, &mut head);
    seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 1, &[0]);

    drop(first);
    assert_eq!(leases.count(baseline), Some(1));
    assert!(storage.collect_snapshots(&leases).unwrap().is_empty());
    drop(second);
    assert_eq!(storage.collect_snapshots(&leases).unwrap(), vec![prefix(0)]);
}

#[test]
fn a_failed_selection_takes_no_lease() {
    let (db, storage, _, _) = baseline_and_tip("failed-selection-no-lease");
    storage
        .conn
        .execute_batch("DROP TRIGGER trg_era_not_deletable; DELETE FROM era")
        .unwrap();
    let leases = SnapshotLeases::default();
    let mut reader = Storage::open_read_only(&db.path).unwrap();
    assert!(reader.acquire_finalized_lease(&leases).is_err());
    assert!(reader.acquire_latest_snapshot_lease(&leases).is_err());
    assert_eq!(leases.entries(), 0);
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// One thread leases the oldest snapshot in a loop while another supersedes and
/// collects it in a loop. After every GC that may have overlapped a lease has
/// returned, the leased row must still exist.
#[test]
fn concurrent_leases_never_hold_a_collected_row() {
    const ROUNDS: u64 = 200;
    let (db, mut writer, _, mut head) = baseline_and_tip("lease-gc-race");
    let leases = SnapshotLeases::default();
    let gc_started = AtomicU64::new(0);
    let gc_finished = AtomicU64::new(0);
    let stop = AtomicBool::new(false);

    /// Releases the leaser when the collector finishes or panics: the scope
    /// joins the leaser, which otherwise waits on the collector forever.
    struct ReleaseLeaser<'a> {
        stop: &'a AtomicBool,
        gc_finished: &'a AtomicU64,
    }
    impl Drop for ReleaseLeaser<'_> {
        fn drop(&mut self) {
            self.gc_finished.store(u64::MAX, Ordering::SeqCst);
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    std::thread::scope(|scope| {
        let leaser = scope.spawn(|| {
            let mut reader = Storage::open_read_only(&db.path).unwrap();
            let mut leased = 0_u64;
            while !stop.load(Ordering::SeqCst) {
                let oldest = |tx: &Transaction<'_>| {
                    tx.query_row(
                        "SELECT snapshot_id FROM snapshots ORDER BY snapshot_id LIMIT 1",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map(|id| id.map(|id| (id, id)))
                };
                let (id, guard) = match leases.lease(&mut reader, oldest) {
                    Ok(Some(leased)) => leased,
                    Ok(None) => continue,
                    Err(error) if is_busy(&error) => continue,
                    Err(error) => panic!("lease: {error}"),
                };
                // Every GC that could have overlapped the lease has committed.
                let started = gc_started.load(Ordering::SeqCst);
                while gc_finished.load(Ordering::SeqCst) < started {
                    std::thread::yield_now();
                }
                let exists = loop {
                    match reader.read(|tx| {
                        tx.query_row(
                            "SELECT COUNT(*) FROM snapshots WHERE snapshot_id = ?1",
                            [id],
                            |row| row.get::<_, i64>(0),
                        )
                    }) {
                        Ok(count) => break count == 1,
                        Err(error) if is_busy(&error) => continue,
                        Err(error) => panic!("check leased row: {error}"),
                    }
                };
                assert!(exists, "GC collected leased snapshot {id}");
                drop(guard);
                leased += 1;
            }
            leased
        });

        let release = ReleaseLeaser {
            stop: &stop,
            gc_finished: &gc_finished,
        };
        let mut collected = 0;
        for round in 0..ROUNDS {
            close(&mut writer, &mut head);
            seed_safe_inputs_with_batch_nonces(&mut writer, SENDER_A, round + 1, &[round]);
            gc_started.fetch_add(1, Ordering::SeqCst);
            collected += writer.collect_snapshots(&leases).unwrap().len();
            gc_finished.fetch_add(1, Ordering::SeqCst);
        }
        drop(release);
        let leased = leaser.join().expect("leaser thread");
        assert!(leased > 0, "the leaser never took a lease");
        assert!(collected > 0, "GC never collected a superseded snapshot");
    });
}
