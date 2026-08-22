// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Snapshot lifecycle integration for the inclusion lane. Three
//! responsibilities (see `docs/snapshots/lifecycle.md` for the full design):
//!
//! 1. **At batch close**, call [`close_batch_with_snapshot`]: dump the
//!    live Application's state, then seal the batch and register the
//!    dump in `pending_snapshots` (keyed by the batch's nonce) in one
//!    transaction. The atomic seal+register guarantees every sealed
//!    batch has a promotable snapshot row. Errors propagate through the
//!    lane's exit per the fail-loud policy.
//!
//! 2. **While processing safe inputs**, thread a [`BlockObservation`]
//!    through `execute_safe_inputs_chunk` to accumulate the highest-nonce
//!    batch of ours that landed in the range. At range close the lane
//!    promotes that one `(nonce, block)` target, folded into the same
//!    transaction that advances the drain
//!    ([`crate::storage::Storage::close_frame_only_promoting_with_executions`])
//!    — so promotion, drain, and canonical execution attributions commit
//!    atomically. Promotion is **per-range, not per-block**: the range's max
//!    nonce supersedes every lower one, and the skipped intermediate
//!    checkpoints were never observable.
//!
//! 3. **After a promotion**, the lane runs [`run_gc`] to reclaim the
//!    now-superseded dump(s). GC tracks garbage creation, not idleness.
//!
//! The observer holds one `Option<(nonce, block)>` plus direct-execution
//! receipts for the complete range. That allocation is confined to the slow
//! L1-reconciliation regime, not the user-op hot path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sequencer_core::application::Application;

use super::dump_info::{self, DumpInfo};
use crate::storage::{DirectInputExecution, SafeInputRange, Storage, WriteHead};

/// Errors from snapshot-taking at batch close.
#[derive(Debug, thiserror::Error)]
pub enum TakeDumpError {
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    CreateDump(#[from] dump_info::CreateDumpDirError),
}

/// Errors from the post-promotion GC pass.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
}

/// Errors from stamping promotion metadata (`B`) into the finalized
/// dump's `info.toml`.
#[derive(Debug, thiserror::Error)]
pub enum StampError {
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
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
        if let Err(err) = dump_info::delete_dump_dir::<A>(&row.prefix) {
            tracing::warn!(
                error = %err,
                prefix = ?row.prefix,
                "GC: filesystem delete failed; orphan left for next startup sweep",
            );
        }
    }
    Ok(removed.len())
}

/// Stamp `B` — the promotion's L1 inclusion block — into the freshly
/// finalized dump's `info.toml`, completing the checkpoint metadata
/// whose other fields were written at batch close. During live operation
/// the DB row is authoritative for `B` and `info.toml` mirrors it (a
/// crash between the promoting commit and this stamp is healed by the
/// idempotent startup re-stamp from the same row). But `info.toml` is
/// also the durable checkpoint the operator backs up and `setup
/// --recovery` reads — there the DB is gone and the file is the sole
/// authority for the resume nonce `N` — so keeping the two coherent here
/// is load-bearing, not cosmetic.
pub(super) fn stamp_finalized_promotion(storage: &mut Storage) -> Result<(), StampError> {
    let finalized = storage
        .finalized_dump()?
        .expect("a promotion just committed, so the finalized snapshot row exists");
    dump_info::stamp_promoted_inclusion_block(&finalized.dump.prefix, finalized.inclusion_block)?;
    Ok(())
}

/// Close the current batch and register its snapshot atomically.
///
/// Order matters for crash/error safety:
/// 1. Read the closing batch's nonce (assigned at open) and the replay
///    head; build a unique dump directory.
/// 2. [`dump_info::create_dump_dir_with_info`] writes + fsyncs the dir,
///    `info.toml` (`N` = nonce + 1, the replay head; `B` stamped later
///    at promotion), and the app's dump — all **before** any DB
///    mutation. On failure nothing is sealed — the batch stays the open
///    Tip; the error propagates per the lane's fail-loud policy (the
///    retry happens on the next boot, after catch-up).
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
    // The snapshot reflects state through the global valid replay head
    // as of the close; single-writer lane, so the head can't move before
    // the close transaction (which re-asserts equality).
    let l2_tx_index = storage.valid_ordered_l2_tx_head()?;
    let dump_dir = make_dump_dir(dumps_dir, nonce);
    dump_info::create_dump_dir_with_info(
        app,
        &dump_dir,
        &DumpInfo::at_batch_close(nonce, l2_tx_index),
    )?;
    storage.close_frame_and_batch_with_pending_dump(
        head,
        next_safe_block,
        &dump_dir,
        nonce,
        l2_tx_index,
        app.executed_input_count(),
    )?;
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
    let dump_dir = make_dump_dir(dumps_dir, nonce);
    dump_info::create_dump_dir_with_info(
        app,
        &dump_dir,
        &DumpInfo::at_batch_close(nonce, l2_tx_index),
    )?;
    storage.insert_pending_dump(&dump_dir, nonce, l2_tx_index)?;
    Ok(())
}

/// Accumulator for batch promotion across one safe-input processing range.
///
/// Records the highest accepted-batch nonce observed in the range and the L1
/// block it landed in, then **commits itself once** at range close (via
/// [`BlockObservation::commit`]): the promotion, if any, folds into the same
/// transaction that advances the drain
/// ([`Storage::close_frame_only_promoting_with_executions`]) — so promotion,
/// drain, and canonical execution mappings commit atomically.
///
/// Per-range (not per-block) promotion is sound because nonces land in
/// monotonic order: the range's max nonce sits in its latest
/// block-with-our-batch and supersedes every lower one (`promote_finalized`
/// deletes all pending `<= max`). Intermediate per-block checkpoints were never
/// observable anyway — `finalized` is a single async-polled row — so collapsing
/// to one promotion loses nothing.
///
/// The accepted-batch observation is constant-sized. Direct execution receipts
/// are retained for the range so the eventual frame transaction can attach
/// their canonical offsets atomically; this allocation is confined to the
/// slow L1-reconciliation regime.
pub(super) struct BlockObservation {
    /// `(nonce, inclusion_block)` of the highest accepted batch seen, or
    /// `None` if the range observed none of our batches.
    max: Option<(u64, u64)>,
    direct_executions: Vec<DirectInputExecution>,
}

impl BlockObservation {
    pub(super) fn new() -> Self {
        Self {
            max: None,
            direct_executions: Vec::new(),
        }
    }

    /// Record a safe input belonging to L1 block `block`; if it was one of our
    /// accepted batches, with `nonce`. Keeps the highest nonce and its block.
    pub(super) fn observe(&mut self, block: u64, own_batch_nonce: Option<u64>) {
        if let Some(nonce) = own_batch_nonce {
            // Monotonic landing order ⇒ a higher nonce is in a later-or-equal
            // block, so the max nonce's block is the latest block-with-our-batch.
            if self.max.is_none_or(|(max_nonce, _)| nonce > max_nonce) {
                self.max = Some((nonce, block));
            }
        }
    }

    pub(super) fn observe_direct_execution(&mut self, execution: DirectInputExecution) {
        self.direct_executions.push(execution);
    }

    /// Close the frame for this safe-frontier advance, folding the observed
    /// promotion — if any — into the **same transaction** as the drain
    /// ([`Storage::close_frame_only_promoting_with_executions`]); otherwise an
    /// attributed plain frame close.
    /// Returns whether a batch was promoted, so the caller can collect the dumps
    /// it superseded. Consumes the observation — it is spent once committed.
    pub(super) fn commit(
        self,
        storage: &mut Storage,
        head: &mut WriteHead,
        next_safe_block: u64,
        drained: SafeInputRange,
    ) -> Result<bool, rusqlite::Error> {
        match self.max {
            Some((max_nonce, inclusion_block)) => {
                storage.close_frame_only_promoting_with_executions(
                    head,
                    next_safe_block,
                    drained,
                    &self.direct_executions,
                    max_nonce,
                    inclusion_block,
                )?;
                Ok(true)
            }
            None => {
                storage.close_frame_only_with_executions(
                    head,
                    next_safe_block,
                    drained,
                    &self.direct_executions,
                )?;
                Ok(false)
            }
        }
    }

    /// Test-only inspection of the accumulated `(max_nonce, inclusion_block)`.
    /// Production drives promotion through [`commit`](Self::commit).
    #[cfg(test)]
    pub(super) fn promotion(&self) -> Option<(u64, u64)> {
        self.max
    }
}

fn make_dump_dir(dumps_dir: &Path, nonce: u64) -> PathBuf {
    // Unique per call within a process: nonce + nanos + atomic counter.
    // Nonces can be reused across recovery cascades, so they alone
    // don't guarantee uniqueness; the nanos+counter pair does. The name
    // is opaque — checkpoint metadata lives in the dir's `info.toml`,
    // never in the path.
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

    use alloy_primitives::Address;
    use sequencer_core::application::{
        AppError, AppOutputs, Application, ApplicationProgress, ApplyInputCapability,
        InvalidReason, ProgressCommitCapability,
    };
    use sequencer_core::l2_tx::ValidUserOp;
    use sequencer_core::user_op::UserOp;

    use crate::storage::Storage;
    use crate::storage::test_helpers::{seed_closed_batches, temp_db};

    use super::dump_info;
    use super::{BlockObservation, take_dump_at_batch_close};

    /// Minimal Application that records every `create_dump` call. The
    /// other trait methods aren't exercised in these tests.
    struct RecordingDumpApp {
        dumps: Mutex<Vec<PathBuf>>,
        progress: ApplicationProgress,
    }

    impl RecordingDumpApp {
        fn new() -> Self {
            Self {
                dumps: Mutex::new(Vec::new()),
                progress: ApplicationProgress::default(),
            }
        }

        fn recorded(&self) -> Vec<PathBuf> {
            self.dumps.lock().unwrap().clone()
        }
    }

    impl Application for RecordingDumpApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = 0;

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<(), InvalidReason> {
            Ok(())
        }

        fn apply_valid_user_op(
            &mut self,
            _capability: ApplyInputCapability<'_>,
            _user_op: &ValidUserOp,
            _safe_block: u64,
        ) -> Result<AppOutputs, AppError> {
            Ok(Vec::new())
        }

        fn apply_direct_input(
            &mut self,
            _capability: ApplyInputCapability<'_>,
            _input: &sequencer_core::l2_tx::DirectInput,
        ) -> Result<AppOutputs, AppError> {
            unimplemented!("not used in these tests")
        }

        fn execution_progress(&self) -> &ApplicationProgress {
            &self.progress
        }

        fn execution_progress_mut(
            &mut self,
            _capability: ProgressCommitCapability<'_>,
        ) -> &mut ApplicationProgress {
            &mut self.progress
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
        let app_prefix = &recorded[0];
        assert!(app_prefix.starts_with(dumps_dir.path()));
        assert!(
            app_prefix.join("state").exists(),
            "dump's state file exists"
        );

        let pending = storage.latest_pending_dump().unwrap().unwrap();
        assert_eq!(pending.nonce, 1);
        // The DB row stores the dump dir; the app dumped into `state`.
        assert_eq!(dump_info::app_prefix(&pending.dump.prefix), *app_prefix);
        assert_eq!(pending.l2_tx_index, 0); // empty batch → no L2 txs

        // info.toml carries the resume nonce and the same cursor;
        // B is unstamped until promotion.
        let info = dump_info::read_info(&pending.dump.prefix).unwrap();
        assert_eq!(info.next_batch_nonce, 2);
        assert_eq!(info.l2_tx_index, 0);
        assert_eq!(info.promoted_inclusion_block, None);
    }

    #[test]
    fn block_observation_keeps_the_max_nonce_and_its_block() {
        let mut obs = BlockObservation::new();
        obs.observe(500, None); // not one of our batches
        obs.observe(500, Some(0)); // our batch nonce 0, block 500
        obs.observe(501, Some(1)); // our batch nonce 1, block 501
        obs.observe(502, None); // not one of our batches
        // Promotes the highest nonce at the block it landed in.
        assert_eq!(obs.promotion(), Some((1, 501)));
    }

    #[test]
    fn block_observation_max_block_is_the_latest_block_with_our_batch() {
        // Several of our batches across blocks; the max nonce's block wins,
        // even when a still-later block has none of our batches.
        let mut obs = BlockObservation::new();
        obs.observe(999, Some(0));
        obs.observe(999, Some(1));
        obs.observe(1000, Some(2));
        obs.observe(1001, None);
        assert_eq!(obs.promotion(), Some((2, 1000)));
    }

    #[test]
    fn block_observation_with_no_observed_nonces_has_no_promotion() {
        let mut obs = BlockObservation::new();
        obs.observe(500, None);
        obs.observe(500, None);
        obs.observe(501, None);
        assert_eq!(obs.promotion(), None);
    }

    #[test]
    fn run_gc_drops_unreferenced_rows_and_their_filesystem_prefixes() {
        let (mut storage, _db) = temp_storage_with_closed_batches("run-gc-removes-fs", 3);
        let dumps_dir = tempfile::tempdir().unwrap();
        let app = RecordingDumpApp::new();

        // Create two snapshots and promote both — the first becomes
        // unreferenced when the second supersedes it as finalized.
        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 0).unwrap();
        take_dump_at_batch_close(&app, &mut storage, dumps_dir.path(), 1).unwrap();
        storage.promote_finalized(0, 500).unwrap();
        storage.promote_finalized(1, 501).unwrap();

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
    #[derive(Default)]
    struct FailingDumpApp {
        progress: ApplicationProgress,
    }

    impl Application for FailingDumpApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = 0;

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<(), InvalidReason> {
            Ok(())
        }

        fn apply_valid_user_op(
            &mut self,
            _capability: ApplyInputCapability<'_>,
            _user_op: &ValidUserOp,
            _safe_block: u64,
        ) -> Result<AppOutputs, AppError> {
            Ok(Vec::new())
        }

        fn apply_direct_input(
            &mut self,
            _capability: ApplyInputCapability<'_>,
            _input: &sequencer_core::l2_tx::DirectInput,
        ) -> Result<AppOutputs, AppError> {
            unimplemented!("not used in these tests")
        }

        fn execution_progress(&self) -> &ApplicationProgress {
            &self.progress
        }

        fn execution_progress_mut(
            &mut self,
            _capability: ProgressCommitCapability<'_>,
        ) -> &mut ApplicationProgress {
            &mut self.progress
        }

        fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
            Ok(Self::default())
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
        let app = FailingDumpApp::default();
        let err =
            super::close_batch_with_snapshot(&app, &mut storage, &mut head, 0, dumps_dir.path())
                .expect_err("create_dump failure must abort the close");
        assert!(matches!(
            err,
            super::TakeDumpError::CreateDump(dump_info::CreateDumpDirError::App(_))
        ));

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
