// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Exclusive data-directory process lock.
//!
//! Every subcommand that touches a data directory (`run`, `setup`,
//! `flush-mempool`) acquires an OS-held advisory
//! lock on `<data_dir>/sequencer.lock` before reading or mutating anything
//! there. `run` transfers it into the structured runtime scope, which retains
//! it until every runtime-owned data-dir task has stopped; nested blocking
//! work retains a clone, including during pre-worker setup/recovery. The other
//! commands hold it until they and any detached blocking work return.
//! Persisted lifecycle rows cannot distinguish a live owner from a stale one;
//! a kernel-held lock can — it vanishes with the process, however the process
//! dies. This prevents two processes from racing settlement, rebuild, or
//! boot on one data dir, and is the exclusive-ownership primitive the
//! authority-boundary ADR's durable lifecycle builds on. A non-owning weak
//! witness to the same descriptor also drives terminal shutdown: after two
//! seconds, a live witness means some runtime-owned work has not drained and
//! the process aborts.

use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::{Arc, Weak};

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
    #[error(transparent)]
    Io(#[from] std::io::Error),
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

/// Non-owning observation of a command/runtime lifetime. The terminal abort
/// watchdog uses this to distinguish a completed drain from work that still
/// owns the data directory without retaining the lock itself.
#[derive(Clone, Debug)]
pub(crate) struct ProcessLockWitness {
    file: Weak<File>,
}

impl ProcessLockWitness {
    pub(crate) fn is_held(&self) -> bool {
        self.file.upgrade().is_some()
    }
}

impl ProcessLock {
    /// Acquire the exclusive data-directory lock without blocking.
    /// [`ProcessLockError::Locked`] means another live process owns the
    /// directory — refusing here is what makes "one process per data dir" a
    /// kernel-enforced fact rather than an operational convention.
    pub(crate) fn acquire(data_dir: &str) -> Result<Self, ProcessLockError> {
        let path = Path::new(data_dir).join(LOCK_FILE_NAME);
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                _file: Arc::new(file),
            }),
            Err(TryLockError::WouldBlock) => Err(ProcessLockError::Locked {
                path: path.display().to_string(),
            }),
            Err(TryLockError::Error(err)) => Err(err.into()),
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

    /// Return a non-owning witness for the lifetime of this lock and all of
    /// its clones. Observing the witness never extends that lifetime.
    pub(crate) fn witness(&self) -> ProcessLockWitness {
        ProcessLockWitness {
            file: Arc::downgrade(&self._file),
        }
    }
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
        ProcessLock::acquire(data_dir).expect("acquire after release");
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
        ProcessLock::acquire(data_dir).expect("last clone releases the lock");
    }

    #[test]
    fn weak_witness_tracks_the_last_lock_owner_without_retaining_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");
        let original = ProcessLock::acquire(data_dir).expect("acquire");
        let retained = original.clone();
        let witness = original.witness();

        assert!(witness.is_held());
        drop(original);
        assert!(
            witness.is_held(),
            "a retained clone still owns the lifetime"
        );
        drop(retained);
        assert!(
            !witness.is_held(),
            "the weak witness must not retain the lock"
        );
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
}
