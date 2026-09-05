// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Startup snapshot hygiene: the authority-neutral repair pass `prepare`
//! runs over the data directory before the runtime boundary. Synchronous,
//! spawns nothing, awaits nothing, holds no `RuntimeScope` — it runs inside
//! `prepare`, before admission, while zero workers exist.
//!
//! [`run_snapshot_hygiene`] runs five steps, in this order:
//!
//! 1. Reset stale leases. A crashed previous run may have left
//!    `lease_count > 0` on dumps that aren't being read by anyone now;
//!    without this, GC would skip them forever.
//! 2. Require the finalized snapshot (always-load invariant). `setup`
//!    registered the genesis snapshot and `run` gated on atomic setup
//!    completion, so it must be present — a missing one is a terminal
//!    incomplete-setup, not a cold-start to paper over (run holds no
//!    genesis app instance).
//! 3. Re-stamp the finalized dump's `info.toml` from the authoritative DB
//!    row. Idempotent, and independent of the two cleanup steps below (the
//!    finalized dump is referenced, so neither GC nor the sweep can touch
//!    it) — what matters is that it lands before the lane loads the dump,
//!    because `info.toml` is the sole authority for `setup --recovery`.
//!    Missing or corrupt metadata under a DB-referenced row is terminal,
//!    never healed.
//! 4. GC SQLite-side: drop any rows now unreferenced after promotions or
//!    invalidations that finalized just before the previous shutdown.
//! 5. Orphan FS sweep: remove directories under `dumps_dir` that aren't
//!    tracked by SQLite (crash-during-create_dump or
//!    crash-during-GC-after-row-delete artifacts).

use crate::commands::error::CommandError;
use crate::ingress::inclusion_lane::dump_info::{self, delete_dump_dir};
use sequencer_core::application::Application;

/// Run the five-step repair pass (see the module doc for the steps and
/// their ordering).
pub(super) fn run_snapshot_hygiene<A: Application + 'static>(
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<(), CommandError> {
    storage.reset_dump_leases()?;
    require_finalized_snapshot(storage)?;
    restamp_finalized_promotion(storage)?;
    let gc_removed = snapshot_gc_at_startup::<A>(storage)?;
    let sweep_removed = sweep_orphan_dumps::<A>(storage, dumps_dir)?;
    tracing::debug!(
        gc_removed,
        sweep_removed,
        "snapshot startup cleanup complete",
    );
    Ok(())
}

/// Require the finalized snapshot the lane will `from_dump` against. `setup`
/// registers the genesis snapshot and `run` gates on atomic setup completion,
/// so by the time the lane starts the snapshot must exist. A missing
/// one means the DB's setup is incomplete/corrupt — terminal
/// `SetupNotComplete` (re-run `setup`), not a cold-start to silently heal.
fn require_finalized_snapshot(storage: &mut crate::storage::Storage) -> Result<(), CommandError> {
    if storage.finalized_dump()?.is_none() {
        return Err(CommandError::Bootstrap(
            crate::commands::error::BootstrapError::SetupNotComplete,
        ));
    }
    Ok(())
}

/// Re-stamp `B` into the finalized dump's `info.toml` from the
/// authoritative DB row. Idempotent; closes the crash window between a
/// promotion's commit and the lane's in-place stamp.
fn restamp_finalized_promotion(storage: &mut crate::storage::Storage) -> Result<(), CommandError> {
    if let Some(finalized) = storage.finalized_dump()? {
        let path = finalized.dump.prefix;
        dump_info::stamp_promoted_inclusion_block(&path, finalized.inclusion_block)
            .map_err(|source| CommandError::ReferencedSnapshotArtifact { path, source })?;
    }
    Ok(())
}

/// Drop any dump rows that are now unreferenced (no pending, no
/// finalized, no leases). The companion `sweep_orphan_dumps` then
/// catches anything on disk that this leaves behind, plus
/// crash-during-create_dump orphans the SQLite layer never saw.
fn snapshot_gc_at_startup<A: Application + 'static>(
    storage: &mut crate::storage::Storage,
) -> Result<usize, CommandError> {
    let removed = storage.gc_unreferenced_dumps()?;
    for row in &removed {
        if let Err(err) = delete_dump_dir::<A>(&row.prefix) {
            tracing::warn!(
                error = %err,
                prefix = ?row.prefix,
                "startup GC: filesystem delete failed; orphan left for sweep",
            );
        }
    }
    Ok(removed.len())
}

/// Walk `dumps_dir` and delete any dump directory that isn't in
/// `Storage::list_dump_rows`. Catches:
///
/// - **crash-during-create**: a dump dir exists on disk (possibly
///   without its app subtree or `info.toml`) but no SQLite row was
///   ever written for it.
/// - **crash-during-GC**: SQLite row was deleted but the filesystem
///   delete either wasn't reached or failed.
///
/// Filesystem-only — no SQLite writes here. Failures log and
/// continue (the next startup retries). The post-`require_finalized_snapshot`
/// ordering matters: the genesis dump's dir is in
/// `list_dump_rows` by the time this runs, so we never delete it.
fn sweep_orphan_dumps<A: Application + 'static>(
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<usize, CommandError> {
    let known: std::collections::HashSet<std::path::PathBuf> = storage
        .list_dump_rows()?
        .into_iter()
        .map(|row| row.prefix)
        .collect();
    let mut removed = 0;
    for entry in std::fs::read_dir(dumps_dir)? {
        let entry = entry?;
        let path = entry.path();
        if known.contains(&path) {
            continue;
        }
        match delete_dump_dir::<A>(&path) {
            Ok(()) => removed += 1,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    ?path,
                    "orphan dump sweep: delete failed; will retry next startup",
                );
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::{SweepTestApp, create_structured_dump};
    use crate::storage::Storage;
    use crate::storage::test_helpers::temp_db;

    #[test]
    fn startup_restamp_rejects_missing_referenced_snapshot_as_terminal() {
        let db = temp_db("restamp-missing-snapshot");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let root = tempfile::tempdir().expect("snapshot parent");
        let missing = root.path().join("missing");
        storage
            .insert_finalized_dump(&missing, 7, 0)
            .expect("register missing fixture");

        let err = restamp_finalized_promotion(&mut storage)
            .expect_err("a durable DB reference cannot point at a missing artifact");

        assert!(matches!(
            &err,
            CommandError::ReferencedSnapshotArtifact { .. }
        ));
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
    }

    #[test]
    fn startup_restamp_rejects_corrupt_referenced_snapshot_as_terminal() {
        let db = temp_db("restamp-corrupt-snapshot");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let root = tempfile::tempdir().expect("snapshot parent");
        let corrupt = root.path().join("corrupt");
        std::fs::create_dir(&corrupt).expect("create snapshot directory");
        std::fs::write(corrupt.join("info.toml"), "not = valid = toml")
            .expect("write corrupt metadata");
        storage
            .insert_finalized_dump(&corrupt, 7, 0)
            .expect("register corrupt fixture");

        let err = restamp_finalized_promotion(&mut storage)
            .expect_err("corrupt durable metadata cannot be retried as operational I/O");

        assert!(matches!(
            &err,
            CommandError::ReferencedSnapshotArtifact { .. }
        ));
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
    }

    #[test]
    fn sweep_orphan_dumps_removes_directories_not_in_storage() {
        let db = temp_db("sweep-orphans");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");

        // Tracked dump (in SQLite).
        let tracked = dumps_dir.path().join("tracked");
        create_structured_dump(&tracked);
        storage
            .insert_finalized_dump(&tracked, 0, 0)
            .expect("register tracked");

        // Two orphans (NOT in SQLite). One is fully formed; the other
        // mimics a crash between dir creation and the app dump (no
        // `state` subtree) — the sweep must remove both.
        let orphan_a = dumps_dir.path().join("orphan-a");
        let orphan_b = dumps_dir.path().join("orphan-b");
        create_structured_dump(&orphan_a);
        std::fs::create_dir(&orphan_b).expect("orphan b dir");

        let removed = sweep_orphan_dumps::<SweepTestApp>(&mut storage, dumps_dir.path()).unwrap();
        assert_eq!(removed, 2);
        assert!(tracked.exists(), "tracked dump must survive");
        assert!(!orphan_a.exists());
        assert!(!orphan_b.exists());
    }

    #[test]
    fn sweep_orphan_dumps_on_empty_directory_is_noop() {
        let db = temp_db("sweep-empty");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");

        let removed = sweep_orphan_dumps::<SweepTestApp>(&mut storage, dumps_dir.path()).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn snapshot_gc_at_startup_removes_unreferenced_rows() {
        let db = temp_db("gc-startup");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");

        // Two dumps: superseded + finalized.
        let superseded = dumps_dir.path().join("superseded");
        let finalized = dumps_dir.path().join("finalized");
        create_structured_dump(&superseded);
        create_structured_dump(&finalized);
        storage
            .insert_pending_dump(&superseded, 0, 0)
            .expect("pending 0");
        storage.promote_finalized(0, 0).expect("promote 0");
        storage
            .insert_pending_dump(&finalized, 1, 0)
            .expect("pending 1");
        storage.promote_finalized(1, 0).expect("promote 1");
        // `superseded`'s row is now unreferenced (replaced by
        // finalized's promotion), but the directory is still on disk.

        let removed = snapshot_gc_at_startup::<SweepTestApp>(&mut storage).unwrap();
        assert_eq!(removed, 1);
        assert!(!superseded.exists(), "GC removed the superseded directory");
        assert!(finalized.exists(), "current finalized survived");
    }
}
