// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Startup clears stale leases, checks rollback checkpoint metadata, then collects
//! obsolete snapshots and orphan directories before workers are admitted.

use std::os::unix::fs::MetadataExt;

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
/// Filesystem-only — no SQLite writes here. Read all retained identities before
/// deleting anything: setup and run may spell the same directory differently.
/// Resolution failures stop the sweep; deletion failures log and retry next
/// startup. The baseline is already registered when this runs.
fn sweep_orphan_dumps(
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<usize, CommandError> {
    let known = storage
        .list_dump_rows()?
        .into_iter()
        .map(|row| {
            dump_identity(&row.prefix).map_err(|source| CommandError::ReferencedSnapshotArtifact {
                path: row.prefix,
                source,
            })
        })
        .collect::<Result<std::collections::HashSet<_>, _>>()?;
    let mut removed = 0;
    for entry in std::fs::read_dir(dumps_dir)? {
        let entry = entry?;
        let path = entry.path();
        let retained = match dump_identity(&path) {
            Ok(identity) => known.contains(&identity),
            // GC or an earlier orphan deletion can leave an unregistered
            // dangling symlink. Retained references already resolved above.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(err.into()),
        };
        if retained {
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

fn dump_identity(path: &std::path::Path) -> std::io::Result<(u64, u64)> {
    // Canonical paths can still differ across mount aliases and macOS firmlinks.
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
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
    fn sweep_preserves_mixed_references_across_path_spellings() {
        for spelling in [
            "relative-to-absolute",
            "absolute-to-relative",
            "stored-dot",
            "sweep-dot",
        ] {
            let db = temp_db(spelling);
            let mut storage = Storage::open(&db.path).unwrap();
            // Avoid changing the process-wide cwd while other tests are running.
            let root = tempfile::tempdir_in(".").unwrap();
            let relative = std::path::PathBuf::from(root.path().file_name().unwrap()).join("dumps");
            std::fs::create_dir(&relative).unwrap();
            let absolute = relative.canonicalize().unwrap();
            let (stored_dir, sweep_dir) = match spelling {
                "relative-to-absolute" => (relative.clone(), absolute.clone()),
                "absolute-to-relative" => (absolute.clone(), relative),
                "stored-dot" => (std::path::Path::new(".").join(&relative), relative),
                "sweep-dot" => (relative.clone(), std::path::Path::new(".").join(relative)),
                _ => unreachable!(),
            };
            let tracked = absolute.join("tracked");
            create_structured_dump(&tracked);
            storage
                .insert_baseline_snapshot(
                    &stored_dir.join("tracked"),
                    crate::storage::ExecutedInputCount::ZERO,
                )
                .unwrap();
            // A literal match must not hide the other row's aliased spelling.
            let literal_match = sweep_dir.join("literal-match");
            create_structured_dump(&literal_match);
            storage
                .write(|tx| {
                    tx.execute(
                        "INSERT INTO dumps(prefix) VALUES (?1)",
                        [literal_match.to_str().unwrap()],
                    )
                })
                .unwrap();
            let orphan = absolute.join("orphan");
            create_structured_dump(&orphan);

            assert_eq!(
                sweep_orphan_dumps(&mut storage, &sweep_dir).unwrap(),
                1,
                "{spelling}"
            );

            assert!(tracked.join("info.toml").is_file(), "{spelling}");
            assert!(literal_match.join("info.toml").is_file(), "{spelling}");
            assert!(!orphan.exists(), "{spelling}");
            assert_eq!(storage.list_dump_rows().unwrap().len(), 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn sweep_preserves_symlinked_parent_and_dump_aliases() {
        for stored_via_alias in [false, true] {
            let db = temp_db("sweep-symlinks");
            let mut storage = Storage::open(&db.path).unwrap();
            let root = tempfile::tempdir().unwrap();
            let dumps = root.path().join("dumps");
            std::fs::create_dir(&dumps).unwrap();
            let parent_alias = root.path().join("parent-alias");
            std::os::unix::fs::symlink(&dumps, &parent_alias).unwrap();
            let tracked = dumps.join("tracked");
            create_structured_dump(&tracked);
            let dump_alias = dumps.join("dump-alias");
            std::os::unix::fs::symlink(&tracked, &dump_alias).unwrap();
            let stored = if stored_via_alias {
                parent_alias.join("dump-alias")
            } else {
                tracked.clone()
            };
            storage
                .insert_baseline_snapshot(&stored, crate::storage::ExecutedInputCount::ZERO)
                .unwrap();
            let orphan = dumps.join("orphan");
            create_structured_dump(&orphan);
            let sweep_dir = if stored_via_alias {
                &dumps
            } else {
                &parent_alias
            };

            assert_eq!(sweep_orphan_dumps(&mut storage, sweep_dir).unwrap(), 1);

            assert!(tracked.join("info.toml").is_file());
            assert!(dump_alias.join("info.toml").is_file());
            assert!(stored.join("info.toml").is_file());
            assert!(!orphan.exists());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sweep_preserves_mixed_firmlink_and_literal_references() {
        let db = temp_db("sweep-firmlink-alias");
        let mut storage = Storage::open(&db.path).unwrap();
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let dumps = root.path().join("dumps");
        std::fs::create_dir(&dumps).unwrap();
        let alias =
            std::path::Path::new("/System/Volumes/Data").join(dumps.strip_prefix("/").unwrap());
        let tracked = dumps.join("tracked");
        create_structured_dump(&tracked);
        storage
            .insert_baseline_snapshot(&tracked, crate::storage::ExecutedInputCount::ZERO)
            .unwrap();

        // This alias survives realpath resolution, unlike an ordinary symlink.
        let listed = alias.join("tracked");
        assert_ne!(
            tracked.canonicalize().unwrap(),
            listed.canonicalize().unwrap()
        );
        let stored_metadata = std::fs::metadata(&tracked).unwrap();
        let listed_metadata = std::fs::metadata(&listed).unwrap();
        assert_eq!(
            (stored_metadata.dev(), stored_metadata.ino()),
            (listed_metadata.dev(), listed_metadata.ino())
        );

        // The sets overlap literally, so a global disjoint-set guard is insufficient.
        let literal_match = alias.join("literal-match");
        create_structured_dump(&literal_match);
        storage
            .write(|tx| {
                tx.execute(
                    "INSERT INTO dumps(prefix) VALUES (?1)",
                    [literal_match.to_str().unwrap()],
                )
            })
            .unwrap();
        let orphan = dumps.join("orphan");
        create_structured_dump(&orphan);

        let removed = sweep_orphan_dumps(&mut storage, &alias).unwrap();

        assert!(
            tracked.join("info.toml").is_file(),
            "the referenced artifact must survive its mount alias"
        );
        assert!(literal_match.join("info.toml").is_file());
        assert!(!orphan.exists());
        assert_eq!(removed, 1);
        assert_eq!(storage.list_dump_rows().unwrap().len(), 2);
    }

    #[test]
    fn sweep_resolves_every_reference_before_deleting_any_artifact() {
        let db = temp_db("sweep-unresolved-reference");
        let mut storage = Storage::open(&db.path).unwrap();
        let dumps = tempfile::tempdir().unwrap();
        let tracked = dumps.path().join("tracked");
        create_structured_dump(&tracked);
        storage
            .insert_baseline_snapshot(
                &dumps.path().join(".").join("tracked"),
                crate::storage::ExecutedInputCount::ZERO,
            )
            .unwrap();
        let missing = dumps.path().join("missing");
        storage
            .write(|tx| {
                tx.execute(
                    "INSERT INTO dumps(prefix) VALUES (?1)",
                    [missing.to_str().unwrap()],
                )
            })
            .unwrap();
        let orphan = dumps.path().join("orphan");
        create_structured_dump(&orphan);

        let error = sweep_orphan_dumps(&mut storage, dumps.path()).unwrap_err();

        assert!(matches!(
            &error,
            CommandError::ReferencedSnapshotArtifact { path, source }
                if path == &missing && source.kind() == std::io::ErrorKind::NotFound
        ));
        assert_eq!(error.exit_code(), crate::commands::error::EXIT_TERMINAL);
        assert!(tracked.join("info.toml").is_file());
        assert!(
            orphan.join("info.toml").is_file(),
            "resolution precedes deletion"
        );
        assert_eq!(storage.list_dump_rows().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn sweep_removes_orphan_symlinks_without_following_them() {
        let db = temp_db("sweep-orphan-symlinks");
        let mut storage = Storage::open(&db.path).unwrap();
        let root = tempfile::tempdir().unwrap();
        let dumps = root.path().join("dumps");
        std::fs::create_dir(&dumps).unwrap();
        let tracked = dumps.join("tracked");
        create_structured_dump(&tracked);
        storage
            .insert_baseline_snapshot(&tracked, crate::storage::ExecutedInputCount::ZERO)
            .unwrap();
        let outside = root.path().join("outside");
        create_structured_dump(&outside);
        let orphan = dumps.join("orphan");
        create_structured_dump(&orphan);
        for (name, target) in [
            ("orphan-alias", orphan),
            ("dangling", root.path().join("missing")),
            ("outside-alias", outside.clone()),
        ] {
            std::os::unix::fs::symlink(target, dumps.join(name)).unwrap();
        }

        assert_eq!(sweep_orphan_dumps(&mut storage, &dumps).unwrap(), 4);
        assert!(tracked.join("info.toml").is_file());
        assert!(outside.join("info.toml").is_file());
        let remaining: Vec<_> = std::fs::read_dir(&dumps)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(remaining, vec![std::ffi::OsString::from("tracked")]);
        assert_eq!(storage.list_dump_rows().unwrap().len(), 1);
    }

    #[test]
    fn startup_resets_persisted_crash_leases_before_collecting_artifacts() {
        let db = temp_db("startup-crash-leases");
        let mut storage = Storage::open(&db.path).unwrap();
        let dumps = tempfile::tempdir().unwrap();
        let baseline = dumps.path().join("baseline");
        create_structured_dump(&baseline);
        let baseline_id = storage
            .insert_baseline_snapshot(&baseline, crate::storage::ExecutedInputCount::ZERO)
            .unwrap();
        let obsolete = dumps.path().join("obsolete");
        create_structured_dump(&obsolete);
        let obsolete_id = storage
            .write(|tx| {
                tx.execute(
                    "UPDATE dumps SET lease_count = 1 WHERE id = ?1",
                    [baseline_id],
                )?;
                tx.execute(
                    "INSERT INTO dumps(prefix, lease_count) VALUES (?1, 1)",
                    [obsolete.to_str().unwrap()],
                )?;
                Ok(tx.last_insert_rowid())
            })
            .unwrap();
        drop(storage);

        let mut storage = Storage::open(&db.path).unwrap();
        assert_eq!(storage.dump_lease_count(obsolete_id).unwrap(), Some(1));
        assert!(storage.gc_unreferenced_dumps().unwrap().is_empty());
        assert!(obsolete.exists(), "the persisted lease blocks ordinary GC");

        run_snapshot_hygiene(&mut storage, dumps.path()).unwrap();

        assert_eq!(storage.dump_lease_count(baseline_id).unwrap(), Some(0));
        assert_eq!(storage.dump_lease_count(obsolete_id).unwrap(), None);
        assert!(
            baseline.exists(),
            "the rollback baseline survives startup GC"
        );
        assert!(
            !obsolete.exists(),
            "startup collects the abandoned leased artifact"
        );
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
