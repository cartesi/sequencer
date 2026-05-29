// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Snapshot lifecycle integration for the inclusion lane.
//!
//! Two responsibilities:
//!
//! 1. **At batch close**, call [`close_batch_with_snapshot`]: dump the
//!    live Application's state, then seal the batch and register the
//!    dump in `pending_snapshots` (keyed by the batch's nonce) in one
//!    transaction. The atomic seal+register guarantees every sealed
//!    batch has a promotable snapshot row. Errors propagate through the
//!    lane's exit per the fail-loud policy.
//!
//! 2. **While processing safe inputs**, thread a [`BlockObservation`]
//!    through `execute_safe_inputs_chunk`. The observer tracks the
//!    current L1 block under iteration; when the block transitions (or
//!    the input range ends), it promotes our latest-nonce batch landed
//!    in the previous block in one atomic operation. This is the
//!    "block-atomic promotion" property the watchdog endpoint relies
//!    on. Safe-input ranges always cover whole L1 blocks, so the
//!    observer never strands a partial block.
//!
//! The observer's working state is two `Option<u64>`s (current block,
//! max observed nonce); the lane creates one per
//! `execute_safe_inputs_range` call on the stack. No allocation in
//! the hot loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sequencer_core::application::{AppError, Application};

use crate::storage::{Storage, WriteHead};

/// Errors from snapshot-taking at batch close.
#[derive(Debug, thiserror::Error)]
pub enum TakeDumpError {
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("app: {0}")]
    App(#[from] AppError),
}

/// Errors from accumulated-observation promotion.
#[derive(Debug, thiserror::Error)]
pub enum PromoteError {
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
}

/// Errors from the periodic GC pass.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
}

/// Run one garbage-collection pass: delete every unreferenced dump
/// row in SQLite (atomically) and best-effort delete the corresponding
/// directories on disk. Filesystem failures log and continue — an
/// orphan file is acceptable per the no-dangling-row invariant; only
/// the reverse (SQLite row pointing at a missing path) would matter,
/// and the SQL-first ordering prevents it.
pub(super) fn run_gc<A: Application + 'static>(storage: &mut Storage) -> Result<usize, GcError> {
    let removed = storage.gc_unreferenced_dumps()?;
    for row in &removed {
        if let Err(err) = A::delete_dump(&row.prefix) {
            tracing::warn!(
                error = %err,
                prefix = ?row.prefix,
                "GC: filesystem delete failed; orphan left for next startup sweep",
            );
        }
    }
    Ok(removed.len())
}

/// Close the current batch and register its snapshot atomically.
///
/// Order matters for crash/error safety:
/// 1. Read the closing batch's nonce (assigned at open) and build a
///    unique dump prefix.
/// 2. [`Application::create_dump`] writes + fsyncs the dump **before**
///    any DB mutation. On failure nothing is sealed — the batch stays
///    the open Tip and the lane retries on its next pass.
/// 3. One DB transaction seals the batch, opens the next, and inserts
///    the `pending_snapshots` row
///    ([`Storage::close_frame_and_batch_with_pending_dump`]). A
///    committed close therefore always has a promotable snapshot; a tx
///    failure rolls the seal back, leaving only an orphan dump
///    directory (reaped by the startup sweep).
///
/// Errors propagate to the lane's main loop per the fail-loud policy —
/// a chronic snapshot failure (e.g. a full disk) is an operational
/// problem to surface, not paper over.
pub(super) fn close_batch_with_snapshot<A: Application>(
    app: &A,
    storage: &mut Storage,
    head: &mut WriteHead,
    next_safe_block: u64,
    dumps_dir: &Path,
) -> Result<(), TakeDumpError> {
    let nonce = storage.batch_nonce(head.batch_index)?;
    let prefix = make_dump_prefix(dumps_dir, nonce);
    app.create_dump(&prefix)?;
    storage.close_frame_and_batch_with_pending_dump(head, next_safe_block, &prefix, nonce)?;
    Ok(())
}

/// Register a pending snapshot for an **already-closed** batch (test
/// helper for seeding `pending_snapshots`). Production closes batches
/// via [`close_batch_with_snapshot`], which is atomic; this two-step
/// "dump then insert" shape only exists so tests can stage pending rows
/// against batches sealed by `seed_closed_batches`.
#[cfg(test)]
pub(super) fn take_dump_at_batch_close<A: Application>(
    app: &A,
    storage: &mut Storage,
    dumps_dir: &Path,
    closed_batch_index: u64,
) -> Result<(), TakeDumpError> {
    let nonce = storage.batch_nonce(closed_batch_index)?;
    // The snapshot reflects state through the global valid replay head,
    // not just this batch's own rows — so an empty batch correctly
    // records the prior head rather than genesis.
    let l2_tx_index = storage.valid_ordered_l2_tx_head()?;
    let prefix = make_dump_prefix(dumps_dir, nonce);
    app.create_dump(&prefix)?;
    storage.insert_pending_dump(&prefix, nonce, l2_tx_index)?;
    Ok(())
}

/// Accumulator for batch-promotion observations across safe-input
/// processing. Tracks the L1 block currently being scanned and the
/// largest nonce of our own batches observed within it. When the
/// block boundary moves, flushes the latest-nonce dump to
/// `finalized_snapshot` in one transaction (which also cleans up any
/// stale pending rows behind it). Safe-input ranges always cover
/// whole blocks (the lane only advances when the safe head moves), so
/// the observer always sees complete groups.
///
/// Constant memory: two `Option<u64>`s, no allocation. Tracking only
/// the max is sufficient because the promotion's job is to point
/// finalized at the latest-state dump and clean pending; both
/// concerns are about `max_observed_nonce`, not the full set, and L1
/// wallet nonces guarantee batches land in monotonic order so
/// "everything ≤ max landed somewhere ≤ this block".
pub(super) struct BlockObservation {
    current_block: Option<u64>,
    max_observed_nonce: Option<u64>,
}

impl BlockObservation {
    pub(super) fn new() -> Self {
        Self {
            current_block: None,
            max_observed_nonce: None,
        }
    }

    /// Record that a safe input belongs to L1 block `block`, and if it
    /// was identified as one of our accepted batches, with `nonce`. If
    /// `block` differs from the block we were tracking, the previous
    /// block's max nonce (if any) is promoted in one SQL transaction
    /// before starting fresh on `block`.
    pub(super) fn record(
        &mut self,
        storage: &mut Storage,
        block: u64,
        own_batch_nonce: Option<u64>,
    ) -> Result<(), PromoteError> {
        match self.current_block {
            None => self.current_block = Some(block),
            Some(current) if current == block => {}
            Some(_) => {
                self.flush(storage)?;
                self.current_block = Some(block);
            }
        }

        if let Some(nonce) = own_batch_nonce {
            self.max_observed_nonce = Some(self.max_observed_nonce.map_or(nonce, |m| m.max(nonce)));
        }
        Ok(())
    }

    /// Promote the current block's max-observed nonce (if any) to
    /// `finalized_snapshot`, then clear the observer. Must be called
    /// at the end of a processing range so the last block's
    /// observation isn't dropped.
    pub(super) fn flush(&mut self, storage: &mut Storage) -> Result<(), PromoteError> {
        let block = self.current_block.take();
        let max_nonce = self.max_observed_nonce.take();
        if let (Some(block), Some(max_nonce)) = (block, max_nonce) {
            storage.promote_finalized(max_nonce, block)?;
        }
        Ok(())
    }
}

fn make_dump_prefix(dumps_dir: &Path, nonce: u64) -> PathBuf {
    // Unique per call within a process: nonce + nanos + atomic counter.
    // Nonces can be reused across recovery cascades, so they alone
    // don't guarantee uniqueness; the nanos+counter pair does.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dumps_dir.join(format!("nonce-{nonce}-{nanos}-{counter}"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use alloy_primitives::{Address, U256};
    use sequencer_core::application::{AppError, AppOutputs, Application, InvalidReason};
    use sequencer_core::l2_tx::ValidUserOp;
    use sequencer_core::user_op::UserOp;

    use crate::storage::Storage;
    use crate::storage::test_helpers::{seed_closed_batches, temp_db};

    use super::{BlockObservation, take_dump_at_batch_close};

    /// Minimal Application that records every `create_dump` call. The
    /// other trait methods aren't exercised in these tests.
    struct RecordingDumpApp {
        dumps: Mutex<Vec<PathBuf>>,
    }

    impl RecordingDumpApp {
        fn new() -> Self {
            Self {
                dumps: Mutex::new(Vec::new()),
            }
        }

        fn recorded(&self) -> Vec<PathBuf> {
            self.dumps.lock().unwrap().clone()
        }
    }

    impl Application for RecordingDumpApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = 0;

        fn current_user_nonce(&self, _sender: Address) -> u32 {
            0
        }

        fn current_user_balance(&self, _sender: Address) -> U256 {
            U256::ZERO
        }

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<(), InvalidReason> {
            Ok(())
        }

        fn execute_valid_user_op(
            &mut self,
            _user_op: &ValidUserOp,
        ) -> Result<AppOutputs, AppError> {
            Ok(Vec::new())
        }

        fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
            unimplemented!("not used in these tests")
        }

        fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
            std::fs::create_dir(prefix)?;
            std::fs::write(prefix.join("state"), b"recorded")?;
            self.dumps.lock().unwrap().push(prefix.to_path_buf());
            Ok(())
        }

        fn delete_dump(prefix: &Path) -> Result<(), AppError> {
            std::fs::remove_dir_all(prefix)?;
            Ok(())
        }

        fn state_file_in_dump(prefix: &Path) -> PathBuf {
            prefix.join("state")
        }
    }

    /// Open a storage with `count` closed batches (indices 0..count-1,
    /// nonces 0..count-1) using the public API. These batches have no
    /// sequenced txs, so the global valid replay head is 0 and the
    /// recorded `l2_tx_index` is 0. That's fine for these tests; the
    /// l2_tx_index propagation through the snapshot lifecycle is
    /// covered by storage-layer tests.
    fn temp_storage_with_closed_batches(
        name: &str,
        count: u64,
    ) -> (Storage, crate::storage::test_helpers::TestDb) {
        let db = temp_db(name);
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        seed_closed_batches(&mut storage, count);
        (storage, db)
    }

    #[test]
    fn take_dump_at_batch_close_creates_dump_and_pending_row() {
        let (mut storage, _db) = temp_storage_with_closed_batches("take-dump", 3);
        let dumps_dir = tempfile::tempdir().unwrap();
        let app = RecordingDumpApp::new();

        // Batch index 1 is closed with nonce 1 (per seed_closed_batches).
        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 1).unwrap();

        let recorded = app.recorded();
        assert_eq!(recorded.len(), 1);
        let prefix = &recorded[0];
        assert!(prefix.starts_with(dumps_dir.path()));
        assert!(prefix.join("state").exists(), "dump's state file exists");

        let pending = storage.latest_pending_dump().unwrap().unwrap();
        assert_eq!(pending.nonce, 1);
        assert_eq!(pending.dump.prefix, *prefix);
        assert_eq!(pending.l2_tx_index, 0); // empty batch → no L2 txs
    }

    #[test]
    fn block_observation_promotes_at_block_transition() {
        let (mut storage, _db) = temp_storage_with_closed_batches("observation-transition", 3);
        let dumps_dir = tempfile::tempdir().unwrap();
        let app = RecordingDumpApp::new();

        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 0).unwrap();
        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 1).unwrap();

        let mut observation = BlockObservation::new();
        // First input: block 500, not our batch.
        observation.record(&mut storage, 500, None).unwrap();
        // Second input: block 500, IS our batch with nonce 0.
        observation.record(&mut storage, 500, Some(0)).unwrap();
        // No promotion yet — same block.
        assert!(storage.finalized_dump().unwrap().is_none());

        // Block transition to 501 flushes block 500's accumulation.
        observation.record(&mut storage, 501, None).unwrap();
        let finalized = storage.finalized_dump().unwrap().unwrap();
        assert_eq!(finalized.inclusion_block, 500);
        assert_eq!(finalized.dump.prefix, app.recorded()[0]);

        // Second observation in block 501.
        observation.record(&mut storage, 501, Some(1)).unwrap();
        // Still not promoted (no block transition yet).
        assert_eq!(
            storage.finalized_dump().unwrap().unwrap().inclusion_block,
            500
        );

        // Final flush promotes block 501's accumulation.
        observation.flush(&mut storage).unwrap();
        let finalized = storage.finalized_dump().unwrap().unwrap();
        assert_eq!(finalized.inclusion_block, 501);
        assert_eq!(finalized.dump.prefix, app.recorded()[1]);
    }

    #[test]
    fn block_observation_groups_multiple_batches_in_same_block_atomically() {
        let (mut storage, _db) = temp_storage_with_closed_batches("observation-multi-block", 4);
        let dumps_dir = tempfile::tempdir().unwrap();
        let app = RecordingDumpApp::new();

        // Seed three pending dumps with nonces 0, 1, 2.
        for nonce in 0..3 {
            take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), nonce).unwrap();
        }
        assert_eq!(app.recorded().len(), 3);

        let mut observation = BlockObservation::new();
        // All three batches land in block 999.
        observation.record(&mut storage, 999, Some(0)).unwrap();
        observation.record(&mut storage, 999, Some(1)).unwrap();
        observation.record(&mut storage, 999, Some(2)).unwrap();
        observation.flush(&mut storage).unwrap();

        // After the single block's flush, finalized points at the dump
        // for the largest nonce (2). The lesser-nonce dumps for 0 and
        // 1 are unreferenced and GC-eligible.
        let finalized = storage.finalized_dump().unwrap().unwrap();
        assert_eq!(finalized.inclusion_block, 999);
        assert_eq!(finalized.dump.prefix, app.recorded()[2]);

        let gc_prefixes: Vec<PathBuf> = storage
            .gc_dump_rows()
            .unwrap()
            .into_iter()
            .map(|row| row.prefix)
            .collect();
        assert_eq!(gc_prefixes.len(), 2);
        assert!(gc_prefixes.contains(&app.recorded()[0]));
        assert!(gc_prefixes.contains(&app.recorded()[1]));
    }

    #[test]
    fn block_observation_with_no_observed_nonces_does_not_finalize() {
        let (mut storage, _db) = temp_storage_with_closed_batches("observation-empty", 1);
        let mut observation = BlockObservation::new();

        observation.record(&mut storage, 500, None).unwrap();
        observation.record(&mut storage, 500, None).unwrap();
        observation.record(&mut storage, 501, None).unwrap();
        observation.flush(&mut storage).unwrap();

        assert!(storage.finalized_dump().unwrap().is_none());
    }

    #[test]
    fn block_observation_flush_is_idempotent_after_first_call() {
        let (mut storage, _db) = temp_storage_with_closed_batches("observation-idempotent", 2);
        let dumps_dir = tempfile::tempdir().unwrap();
        let app = RecordingDumpApp::new();

        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 0).unwrap();

        let mut observation = BlockObservation::new();
        observation.record(&mut storage, 500, Some(0)).unwrap();
        observation.flush(&mut storage).unwrap();
        // A second flush with no new observations is a no-op.
        observation.flush(&mut storage).unwrap();

        let finalized = storage.finalized_dump().unwrap().unwrap();
        assert_eq!(finalized.inclusion_block, 500);
    }

    #[test]
    fn run_gc_drops_unreferenced_rows_and_their_filesystem_prefixes() {
        let (mut storage, _db) = temp_storage_with_closed_batches("run-gc-removes-fs", 3);
        let dumps_dir = tempfile::tempdir().unwrap();
        let app = RecordingDumpApp::new();

        // Create two snapshots and promote both — the first becomes
        // unreferenced when the second supersedes it.
        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 0).unwrap();
        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 1).unwrap();
        let mut obs = BlockObservation::new();
        obs.record(&mut storage, 500, Some(0)).unwrap();
        obs.flush(&mut storage).unwrap();
        let mut obs = BlockObservation::new();
        obs.record(&mut storage, 501, Some(1)).unwrap();
        obs.flush(&mut storage).unwrap();

        // The first dump's directory is on disk (RecordingDumpApp
        // wrote a "state" file in it). After GC it should be gone.
        let recorded = app.recorded();
        let superseded_prefix = recorded.first().expect("a dump was recorded").clone();
        assert!(superseded_prefix.exists(), "pre-GC sanity");

        let removed = super::run_gc::<RecordingDumpApp>(&mut storage).unwrap();
        assert_eq!(removed, 1, "exactly one unreferenced dump cleaned");
        assert!(!superseded_prefix.exists(), "filesystem prefix removed too",);

        // The current finalized's prefix survives.
        let finalized = storage.finalized_dump().unwrap().unwrap();
        assert!(finalized.dump.prefix.exists());
    }

    #[test]
    fn run_gc_with_no_eligible_rows_is_a_noop() {
        let (mut storage, _db) = temp_storage_with_closed_batches("run-gc-noop", 1);
        let dumps_dir = tempfile::tempdir().unwrap();
        let app = RecordingDumpApp::new();
        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 0).unwrap();

        // Pending row references the dump; nothing eligible.
        let removed = super::run_gc::<RecordingDumpApp>(&mut storage).unwrap();
        assert_eq!(removed, 0);
        assert!(app.recorded()[0].exists());
    }

    /// Application whose `create_dump` always fails — used to exercise
    /// the atomic-close failure path.
    struct FailingDumpApp;

    impl Application for FailingDumpApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = 0;

        fn current_user_nonce(&self, _sender: Address) -> u32 {
            0
        }

        fn current_user_balance(&self, _sender: Address) -> U256 {
            U256::ZERO
        }

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<(), InvalidReason> {
            Ok(())
        }

        fn execute_valid_user_op(
            &mut self,
            _user_op: &ValidUserOp,
        ) -> Result<AppOutputs, AppError> {
            Ok(Vec::new())
        }

        fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
            Ok(FailingDumpApp)
        }

        fn create_dump(&self, _prefix: &Path) -> Result<(), AppError> {
            Err(AppError::Internal {
                reason: "simulated create_dump failure".to_string(),
            })
        }

        fn delete_dump(_prefix: &Path) -> Result<(), AppError> {
            Ok(())
        }

        fn state_file_in_dump(prefix: &Path) -> PathBuf {
            prefix.join("state")
        }
    }

    #[test]
    fn close_batch_with_snapshot_create_dump_failure_leaves_batch_open() {
        let db = temp_db("close-snapshot-create-fail");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let mut head = storage
            .initialize_open_state(0, crate::storage::SafeInputRange::empty_at(0))
            .expect("init open state");
        let open_before = head.batch_index;

        let dumps_dir = tempfile::tempdir().unwrap();
        let app = FailingDumpApp;
        let err =
            super::close_batch_with_snapshot(&app, &mut storage, &mut head, 0, dumps_dir.path())
                .expect_err("create_dump failure must abort the close");
        assert!(matches!(err, super::TakeDumpError::App(_)));

        // Nothing sealed: the batch is still the open Tip, no successor,
        // and no pending snapshot row was written.
        let open_after = storage
            .open_state()
            .unwrap()
            .expect("tip must still be open");
        assert_eq!(
            open_after.batch_index, open_before,
            "batch must remain the open Tip after a create_dump failure"
        );
        assert!(
            storage.latest_pending_dump().unwrap().is_none(),
            "no pending snapshot row when create_dump failed"
        );
    }

    #[test]
    fn close_batch_with_snapshot_seals_and_registers_pending_atomically() {
        let db = temp_db("close-snapshot-atomic");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let mut head = storage
            .initialize_open_state(0, crate::storage::SafeInputRange::empty_at(0))
            .expect("init open state");
        let sealed_index = head.batch_index;

        let dumps_dir = tempfile::tempdir().unwrap();
        let app = RecordingDumpApp::new();
        super::close_batch_with_snapshot(&app, &mut storage, &mut head, 0, dumps_dir.path())
            .expect("atomic close");

        // Head advanced to the freshly opened batch; the sealed batch
        // has its pending snapshot row and the dump exists on disk.
        assert_ne!(
            head.batch_index, sealed_index,
            "head should advance to the next batch"
        );
        let pending = storage
            .latest_pending_dump()
            .unwrap()
            .expect("pending row exists");
        assert_eq!(
            pending.nonce, 0,
            "pending keyed by the sealed batch's nonce"
        );
        assert_eq!(app.recorded().len(), 1, "dump created on disk");
        assert!(pending.dump.prefix.join("state").exists());
    }
}
