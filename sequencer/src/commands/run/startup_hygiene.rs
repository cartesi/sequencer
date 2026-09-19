// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Startup clears stale leases, checks rollback checkpoint metadata, then collects
//! obsolete snapshots and orphan directories before workers are admitted.

use crate::commands::error::CommandError;
use crate::ingress::inclusion_lane::dump_info::{self, delete_dump_dir};

/// Repair interrupted artifact creation/collection before starting workers.
pub(super) fn run_snapshot_hygiene(
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<(), CommandError> {
    storage.reset_dump_leases()?;
    require_rollback_snapshot(storage)?;
    let gc_removed = snapshot_gc_at_startup(storage)?;
    let sweep_removed = sweep_orphan_dumps(storage, dumps_dir)?;
    tracing::debug!(
        gc_removed,
        sweep_removed,
        "snapshot startup cleanup complete",
    );
    Ok(())
}

fn require_rollback_snapshot(storage: &mut crate::storage::Storage) -> Result<(), CommandError> {
    let snapshot = storage.rollback_snapshot()?.ok_or(CommandError::Bootstrap(
        crate::commands::error::BootstrapError::SetupNotComplete,
    ))?;
    dump_info::read_info(&snapshot.dump.prefix).map_err(|source| {
        CommandError::ReferencedSnapshotArtifact {
            path: snapshot.dump.prefix,
            source,
        }
    })?;
    Ok(())
}

/// Delete obsolete rows before their files; the sweep retries leftover files.
fn snapshot_gc_at_startup(storage: &mut crate::storage::Storage) -> Result<usize, CommandError> {
    let removed = storage.gc_unreferenced_dumps()?;
    for row in &removed {
        if let Err(err) = delete_dump_dir(&row.prefix) {
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
/// continue (the next startup retries). The post-`require_rollback_snapshot`
/// ordering matters: the genesis dump's dir is in
/// `list_dump_rows` by the time this runs, so we never delete it.
fn sweep_orphan_dumps(
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
        match delete_dump_dir(&path) {
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
    use crate::commands::test_support::create_structured_dump;
    use crate::storage::Storage;
    use crate::storage::test_helpers::temp_db;

    #[test]
    fn startup_validation_rejects_missing_referenced_snapshot_as_terminal() {
        let db = temp_db("validation-missing-snapshot");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let root = tempfile::tempdir().expect("snapshot parent");
        let missing = root.path().join("missing");
        storage
            .insert_baseline_snapshot(&missing, crate::storage::ExecutedInputCount::ZERO)
            .expect("register missing fixture");

        let err = require_rollback_snapshot(&mut storage)
            .expect_err("a durable DB reference cannot point at a missing artifact");

        assert!(matches!(
            &err,
            CommandError::ReferencedSnapshotArtifact { .. }
        ));
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
    }

    #[test]
    fn startup_validation_rejects_corrupt_referenced_snapshot_as_terminal() {
        let db = temp_db("validation-corrupt-snapshot");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let root = tempfile::tempdir().expect("snapshot parent");
        let corrupt = root.path().join("corrupt");
        std::fs::create_dir(&corrupt).expect("create snapshot directory");
        std::fs::write(corrupt.join("info.toml"), "not = valid = toml")
            .expect("write corrupt metadata");
        storage
            .insert_baseline_snapshot(&corrupt, crate::storage::ExecutedInputCount::ZERO)
            .expect("register corrupt fixture");

        let err = require_rollback_snapshot(&mut storage)
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
            .insert_baseline_snapshot(&tracked, crate::storage::ExecutedInputCount::ZERO)
            .expect("register tracked");

        // Two orphans (NOT in SQLite). One is fully formed; the other
        // mimics a crash between dir creation and the app dump (no
        // `state` subtree) — the sweep must remove both.
        let orphan_a = dumps_dir.path().join("orphan-a");
        let orphan_b = dumps_dir.path().join("orphan-b");
        create_structured_dump(&orphan_a);
        std::fs::create_dir(&orphan_b).expect("orphan b dir");

        let removed = sweep_orphan_dumps(&mut storage, dumps_dir.path()).unwrap();
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

        let removed = sweep_orphan_dumps(&mut storage, dumps_dir.path()).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn snapshot_gc_at_startup_removes_unreferenced_rows() {
        let db = temp_db("gc-startup");
        let mut storage = Storage::open(db.path.as_str()).unwrap();
        let dumps_dir = tempfile::tempdir().unwrap();
        let orphan = dumps_dir.path().join("orphan");
        create_structured_dump(&orphan);
        storage
            .write(|tx| {
                tx.execute(
                    "INSERT INTO dumps(prefix) VALUES (?1)",
                    [orphan.to_str().unwrap()],
                )
            })
            .unwrap();
        let removed = snapshot_gc_at_startup(&mut storage).unwrap();
        assert_eq!(removed, 1);
        assert!(!orphan.exists());
    }
}
