// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Exclusive data-directory lock: every command takes an OS-held advisory lock on
//! `<data_dir>/sequencer.lock` before touching the directory and keeps it until
//! the last clone drops. Unlike a persisted row, a kernel lock dies with its
//! process. Lifetime rules: authority-boundary ADR, mechanism 1.

use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::Arc;

use thiserror::Error;

/// The lock module's own typed error: `runtime/` is the capability
/// substrate and must not import the command layer's error taxonomy.
/// `commands::error` converts this into its `BootstrapError` classes.
#[derive(Debug, Error)]
pub(crate) enum ProcessLockError {
    /// Another live process holds the exclusive data-directory lock.
    /// Retry-safe: an orchestrated restart racing the previous owner's
    /// drain resolves on its own.
    #[error(
        "another process holds the data-directory lock ({path}); \
         refusing to run concurrently"
    )]
    Locked { path: String },
    #[error("data-directory lock {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Lock-anchor file name inside the data directory. Contents are irrelevant
/// and the file is never deleted: unlinking a lock file invites the classic
/// unlink/re-open race where two processes hold locks on different inodes of
/// the same path.
const LOCK_FILE_NAME: &str = "sequencer.lock";

/// A held exclusive lock on a data directory. Clones retain the same locked
/// descriptor; the lock is released only after the last clone drops (or on
/// process death). This lets detached blocking work retain exclusivity even
/// if the async command future awaiting it is cancelled.
#[derive(Clone, Debug)]
pub(crate) struct ProcessLock {
    /// Keeps the locked descriptor open; the OS lock lives on it.
    _file: Arc<File>,
}

impl ProcessLock {
    /// Acquire the exclusive data-directory lock without blocking.
    /// [`ProcessLockError::Locked`] means another live process owns the
    /// directory — refusing here is what makes "one process per data dir" a
    /// kernel-enforced fact rather than an operational convention.
    pub(crate) fn acquire(data_dir: &str) -> Result<Self, ProcessLockError> {
        let path = Path::new(data_dir).join(LOCK_FILE_NAME);
        let io_error = |source: std::io::Error| ProcessLockError::Io {
            path: path.display().to_string(),
            source,
        };
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_error)?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                _file: Arc::new(file),
            }),
            Err(TryLockError::WouldBlock) => Err(ProcessLockError::Locked {
                path: path.display().to_string(),
            }),
            Err(TryLockError::Error(err)) => Err(io_error(err)),
        }
    }

    /// Test lock on a leaked temp dir (bounded by test count): component
    /// tests need a held lock now that data-dir workers require one at
    /// construction.
    #[cfg(test)]
    pub(crate) fn test() -> Self {
        let dir = tempfile::tempdir().expect("test lock tempdir");
        let lock =
            Self::acquire(dir.path().to_str().expect("utf8 path")).expect("test lock acquire");
        std::mem::forget(dir);
        lock
    }

    /// Acquire a lock this process has just released, for tests asserting
    /// the release. `flock` frees a lock only when every descriptor sharing
    /// it closes, and a child spawned by a concurrent test (Anvil, a
    /// re-executed test binary) holds copies of all of them until its `exec`
    /// closes them, so an immediate acquire can see
    /// [`ProcessLockError::Locked`] for that window. It retries only while no
    /// descriptor of this process is open on the lock file, so an owner in
    /// this process that has not released fails at once, as a plain acquire
    /// would.
    #[cfg(test)]
    pub(crate) fn acquire_released(data_dir: &str) -> Self {
        use std::time::{Duration, Instant};

        let path = Path::new(data_dir).join(LOCK_FILE_NAME);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match Self::acquire(data_dir) {
                Ok(lock) => return lock,
                Err(ProcessLockError::Locked { .. }) if open_in_this_process(&path) => {
                    panic!("an owner in this process still holds {}", path.display())
                }
                Err(ProcessLockError::Locked { .. }) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => panic!("re-acquiring the released lock failed: {err}"),
            }
        }
    }
}

/// Whether a descriptor of this process is open on `path`: its descriptor
/// table, read through `/dev/fd` (`/proc/self/fd` on Linux), matched by
/// inode and change time, since macOS reports devfs's device for a `/dev/fd`
/// entry. A spawned child's copies are in the child's table.
#[cfg(test)]
fn open_in_this_process(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let target = std::fs::metadata(path).expect("stat the lock file");
    let table = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(table)
        .expect("read this process's descriptor table")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::metadata(entry.path()).ok())
        .any(|meta| {
            meta.ino() == target.ino()
                && meta.ctime() == target.ctime()
                && meta.ctime_nsec() == target.ctime_nsec()
        })
}

/// Spawn blocking work while retaining the command/runtime lock for the
/// closure's real lifetime. Dropping the async join handle detaches Tokio
/// blocking work; this wrapper prevents that detach from releasing exclusive
/// data-directory ownership prematurely. The lock is required: a data-dir
/// blocking task without ownership is unrepresentable.
pub(crate) fn spawn_blocking_with_lock<F, R>(
    process_lock: ProcessLock,
    work: F,
) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _process_lock = process_lock;
        work()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_refuses_while_held_and_succeeds_after_release() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");

        let held = ProcessLock::acquire(data_dir).expect("first acquire");
        let refused =
            ProcessLock::acquire(data_dir).expect_err("a held lock must refuse a second owner");
        assert!(
            matches!(&refused, ProcessLockError::Locked { .. }),
            "expected Locked, got {refused:?}"
        );
        // The command layer classifies contention retry-safe: the previous
        // owner may be draining.
        let projected = crate::commands::error::CommandError::from(refused);
        assert_eq!(
            projected.exit_code(),
            crate::commands::error::EXIT_RESTART_TRANSIENT,
        );

        drop(held);
        ProcessLock::acquire_released(data_dir);
    }

    #[test]
    fn open_in_this_process_tracks_the_lock_descriptor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");
        let path = Path::new(data_dir).join(LOCK_FILE_NAME);

        let held = ProcessLock::acquire(data_dir).expect("acquire");
        assert!(open_in_this_process(&path));
        drop(held);
        assert!(!open_in_this_process(&path));
    }

    #[test]
    fn locks_on_distinct_data_dirs_are_independent() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let _a = ProcessLock::acquire(dir_a.path().to_str().expect("utf8")).expect("lock a");
        let _b = ProcessLock::acquire(dir_b.path().to_str().expect("utf8")).expect("lock b");
    }

    #[test]
    fn clone_retains_exclusivity_after_original_drops() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");
        let original = ProcessLock::acquire(data_dir).expect("acquire");
        let retained = original.clone();

        drop(original);
        ProcessLock::acquire(data_dir).expect_err("retained clone must keep the lock held");

        drop(retained);
        ProcessLock::acquire_released(data_dir);
    }

    #[tokio::test]
    async fn detached_blocking_work_retains_exclusivity_until_it_stops() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();
        let held = ProcessLock::acquire(&data_dir).expect("acquire");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let task = spawn_blocking_with_lock(held.clone(), move || {
            let _ = started_tx.send(());
            release_rx.recv().expect("release blocking work");
        });
        started_rx.await.expect("blocking work started");
        drop(task); // detach the running blocking closure
        drop(held);

        ProcessLock::acquire(&data_dir)
            .expect_err("detached blocking work must retain process ownership");
        release_tx.send(()).expect("release blocking work");

        // The closure has no join handle now, so poll the kernel lock with a
        // bounded yield loop rather than racing its final drop.
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if ProcessLock::acquire(&data_dir).is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking closure should release its retained lock");
    }

    #[test]
    fn io_failure_names_the_lock_path() {
        let parent = tempfile::tempdir().expect("create parent dir");
        let missing = parent.path().join("missing");
        let lock_path = missing.join(LOCK_FILE_NAME).display().to_string();

        let err = ProcessLock::acquire(missing.to_str().expect("utf8 path"))
            .expect_err("a lock in a missing directory cannot open");

        assert!(matches!(&err, ProcessLockError::Io { path, .. } if *path == lock_path));
        let projected = crate::commands::error::CommandError::from(err);
        assert!(projected.to_string().contains(&lock_path), "{projected}");
        assert_eq!(
            projected.exit_code(),
            crate::commands::error::EXIT_UNCLASSIFIED
        );
    }
}
