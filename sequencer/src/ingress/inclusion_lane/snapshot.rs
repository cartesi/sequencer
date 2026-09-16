// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Durable batch-close artifacts and filesystem cleanup after SQLite GC.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::dump_info::{self, DumpInfo};
use crate::storage::{Storage, WriteHead};
use sequencer_core::application::Application;

#[derive(Debug, thiserror::Error)]
pub enum TakeDumpError {
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    CreateDump(#[from] dump_info::CreateDumpDirError),
}

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
}

pub(super) fn run_gc(storage: &mut Storage) -> Result<usize, GcError> {
    let removed = storage.gc_unreferenced_dumps()?;
    for row in &removed {
        if let Err(err) = dump_info::delete_dump_dir(&row.prefix) {
            tracing::warn!(error = %err, prefix = ?row.prefix,
                "GC: filesystem delete failed; orphan left for next startup sweep");
        }
    }
    Ok(removed.len())
}

/// Files become durable before the atomic batch seal and snapshot registration.
/// A failed commit leaves only an orphan directory for the startup sweep.
pub(super) fn close_batch_with_snapshot<A: Application>(
    app: &mut A,
    storage: &mut Storage,
    head: &mut WriteHead,
    next_safe_block: u64,
    dumps_dir: &Path,
) -> Result<(), TakeDumpError> {
    let batch_index = head.batch_index;
    let nonce = storage.batch_nonce(batch_index)?;
    let dump_dir = make_dump_dir(dumps_dir, nonce);
    dump_info::create_dump_dir_with_info(app, &dump_dir, &DumpInfo::at_batch_close(nonce))?;
    storage.close_frame_and_batch_with_snapshot(
        head,
        next_safe_block,
        &dump_dir,
        batch_index,
        app.executed_input_count(),
    )?;
    Ok(())
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
