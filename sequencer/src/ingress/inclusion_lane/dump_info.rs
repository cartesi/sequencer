// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Sequencer-owned dump-directory structure and metadata (`info.toml`).
//!
//! Every dump is a directory `dumps/<id>/` with exactly two entries:
//!
//! ```text
//! dumps/<id>/
//!   state       app-owned subtree — the prefix handed to
//!               `Application::{create_dump, from_dump, delete_dump}`
//!   info.toml   sequencer-owned checkpoint metadata (this module)
//! ```
//!
//! `info.toml` makes a finalized dump a self-contained checkpoint for the
//! recovery handoff — and, together with the app's `state`
//! subtree, the unit an operator backs up: `setup --recovery` rebuilds a wiped
//! DB from exactly this pair, reading `N` straight from the file. `next_batch_nonce`
//! (`N`) is known at batch close and written then; `promoted_inclusion_block`
//! (`B`) is known at promotion and stamped in place afterwards. An in-place update of a file
//! *inside* the dir changes no path, so the no-dangling-row invariant, leases,
//! and GC — all keyed on the immutable directory path — are untouched.
//!
//! The DB row (`dumps.prefix`) stores the dump *directory*; the app prefix is
//! always derived via [`app_prefix`]. Crash-safety ordering is unchanged from
//! the lifecycle doc: dir + `info.toml` + app dump are durable on disk before
//! the row that references the dir; row deletion precedes file deletion.

use std::io;
use std::path::{Path, PathBuf};

use sequencer_core::application::{AppError, Application};

/// Name of the app-owned subtree inside a dump directory.
const APP_STATE_SUBDIR: &str = "state";
/// Name of the sequencer-owned metadata file inside a dump directory.
const INFO_FILE: &str = "info.toml";

pub const FORMAT_VERSION: u64 = 1;

/// The app's dump prefix inside `dump_dir`. Pure path derivation.
pub fn app_prefix(dump_dir: &Path) -> PathBuf {
    dump_dir.join(APP_STATE_SUBDIR)
}

/// Sequencer-owned checkpoint metadata for one dump.
///
/// Serialized as real TOML (`#[serde(deny_unknown_fields)]` keeps the parse
/// strict — unknown keys, type mismatches, and TOML's own duplicate-key ban all
/// fail loud, the same guarantees the old hand parser gave). The on-disk layout
/// is plain `key = value` integer lines, so it is operator-readable and any
/// previously-written file round-trips unchanged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DumpInfo {
    pub format_version: u64,
    /// `N` — the batch nonce the sequencer resumes submitting at when
    /// booting from this checkpoint (snapshot batch's nonce + 1; 0 for
    /// the genesis dump). Known at batch close.
    pub next_batch_nonce: u64,
    /// Replay cursor: global valid replay head at dump time. Mirrors the
    /// snapshot row exactly.
    pub l2_tx_index: u64,
    /// `B` — the L1 inclusion block of the promotion that finalized this
    /// dump. `None` until promotion; stamped in place when the batch is
    /// observed accepted (and re-stamped from the DB at startup, closing
    /// the commit-then-stamp crash window). Omitted from the file while
    /// `None` (TOML has no null); a missing key reads back as `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promoted_inclusion_block: Option<u64>,
}

impl DumpInfo {
    /// Checkpoint metadata for the snapshot taken at the close of batch
    /// `batch_nonce`: the sequencer resumes submitting at `batch_nonce + 1`, and
    /// `B` is unknown until promotion (stamped in place then). The single home
    /// for the resume-nonce `+ 1` skew, so the batch-close and recovery-fill
    /// sites cannot drift on how `next_batch_nonce` is derived.
    pub fn at_batch_close(batch_nonce: u64, l2_tx_index: u64) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            next_batch_nonce: batch_nonce + 1,
            l2_tx_index,
            promoted_inclusion_block: None,
        }
    }

    /// Checkpoint metadata for a finalized snapshot rebuilt by `setup --recovery`
    /// at inclusion block `inclusion_block`, resuming at `resume_nonce` — the
    /// fold's next-expected nonce `N'`, already the resume value (no `+ 1`).
    pub fn at_recovery(resume_nonce: u64, l2_tx_index: u64, inclusion_block: u64) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            next_batch_nonce: resume_nonce,
            l2_tx_index,
            promoted_inclusion_block: Some(inclusion_block),
        }
    }
}

/// Errors from creating a structured dump directory.
#[derive(Debug, thiserror::Error)]
pub enum CreateDumpDirError {
    #[error("app: {0}")]
    App(#[from] AppError),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Create one structured dump directory: the dir itself, the
/// sequencer-owned `info.toml`, then the app's dump under `state` —
/// every file durable before the caller writes the DB row that
/// references the dir. The app's `create_dump` contract fsyncs its
/// subtree and its parent (the dump dir); the final parent-dir fsync
/// persists the dump dir's own entry in the dumps directory.
pub fn create_dump_dir_with_info<A: Application>(
    app: &A,
    dump_dir: &Path,
    info: &DumpInfo,
) -> Result<(), CreateDumpDirError> {
    std::fs::create_dir(dump_dir)?;
    write_info(dump_dir, info)?;
    app.create_dump(&app_prefix(dump_dir))?;
    let parent = dump_dir
        .parent()
        .expect("dump dir always lives inside a dumps directory");
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Delete one structured dump directory: the app's subtree via its
/// `delete_dump` hook (when present — an orphan from a crash between
/// dir creation and `create_dump` legitimately lacks it), then the
/// rest of the dir (`info.toml` + the dir itself).
pub fn delete_dump_dir<A: Application>(dump_dir: &Path) -> Result<(), AppError> {
    let app_prefix = app_prefix(dump_dir);
    if app_prefix.exists() {
        A::delete_dump(&app_prefix)?;
    }
    std::fs::remove_dir_all(dump_dir)?;
    Ok(())
}

/// Write `info.toml` into `dump_dir`, durably: temp file, fsync, rename
/// over, fsync the directory. Safe both for initial creation and for the
/// in-place promotion stamp.
pub fn write_info(dump_dir: &Path, info: &DumpInfo) -> io::Result<()> {
    let content = toml::to_string(info)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("info.toml: {e}")))?;

    let tmp = dump_dir.join(format!("{INFO_FILE}.tmp"));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, dump_dir.join(INFO_FILE))?;
    std::fs::File::open(dump_dir)?.sync_all()?;
    Ok(())
}

/// Read and strictly parse `info.toml` from `dump_dir`. Unknown keys
/// (`deny_unknown_fields`), duplicate keys (TOML forbids them), malformed
/// values, and missing required keys all fail loud — a dump with corrupt
/// metadata is not silently usable. The `format_version` gate runs after the
/// parse; there is a single frozen schema (`FORMAT_VERSION`), so a
/// forward-version dump is rejected outright rather than partially read.
///
/// Missing/`ENOENT` paths get an operator-facing diagnosis via
/// [`diagnose_missing_dump`] so a watchdog CM checkpoint (or any other
/// non-dump directory) is not reported as a bare "No such file or directory".
pub fn read_info(dump_dir: &Path) -> io::Result<DumpInfo> {
    let content = match std::fs::read_to_string(dump_dir.join(INFO_FILE)) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                diagnose_missing_dump(dump_dir),
            ));
        }
        Err(e) => return Err(e),
    };
    let bad = |reason: String| io::Error::new(io::ErrorKind::InvalidData, reason);

    let info: DumpInfo = toml::from_str(&content).map_err(|e| bad(format!("info.toml: {e}")))?;
    if info.format_version != FORMAT_VERSION {
        return Err(bad(format!(
            "info.toml: unsupported format_version {} (expected {FORMAT_VERSION})",
            info.format_version
        )));
    }
    Ok(info)
}

/// Explain why `dump_dir` is not a usable sequencer checkpoint dump.
///
/// Called when `info.toml` is missing. Distinguishes the common operator
/// mistake of pointing `--checkpoint-dump-dir` at a **watchdog** CM checkpoint
/// (`manifest.json` + `snapshot/`) from a merely empty/wrong path.
pub fn diagnose_missing_dump(dump_dir: &Path) -> String {
    let expected = format!(
        "expected a finalized sequencer dump at {} with `info.toml` and a `state/` \
         app subtree (usually `$CARTESI_SEQUENCER_DATA_DIR/dumps/<id>/`). \
         See docs/snapshots/lifecycle.md and docs/recovery/cockroach.md.",
        dump_dir.display()
    );

    if !dump_dir.exists() {
        return format!("path does not exist — {expected}");
    }
    if !dump_dir.is_dir() {
        return format!("path is not a directory — {expected}");
    }

    let has_manifest = dump_dir.join("manifest.json").is_file();
    let has_snapshot = dump_dir.join("snapshot").is_dir();
    if has_manifest || has_snapshot {
        let found = match (has_manifest, has_snapshot) {
            (true, true) => "manifest.json and snapshot/",
            (true, false) => "manifest.json",
            (false, true) => "snapshot/",
            (false, false) => unreachable!(),
        };
        return format!(
            "this looks like a watchdog Cartesi Machine checkpoint (found {found}), \
             not a sequencer dump. `setup --recovery --checkpoint-dump-dir` cannot \
             use watchdog state under .../checkpoints/<block>/ — those are CM \
             snapshots for the watchdog compare loop. {expected}"
        );
    }

    let has_state = dump_dir.join(APP_STATE_SUBDIR).exists();
    if has_state {
        return format!(
            "missing `info.toml` (found `state/` but no checkpoint metadata) — {expected}"
        );
    }

    format!("missing `info.toml` — {expected}")
}

/// Stamp `B` (the promotion's inclusion block) into an existing
/// `info.toml`. Idempotent: re-stamping the same block is a no-op write.
pub fn stamp_promoted_inclusion_block(dump_dir: &Path, block: u64) -> io::Result<()> {
    let mut info = read_info(dump_dir)?;
    if info.promoted_inclusion_block == Some(block) {
        return Ok(());
    }
    info.promoted_inclusion_block = Some(block);
    write_info(dump_dir, &info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DumpInfo {
        DumpInfo {
            format_version: FORMAT_VERSION,
            next_batch_nonce: 7,
            l2_tx_index: 123,
            promoted_inclusion_block: None,
        }
    }

    #[test]
    fn write_then_read_round_trips_without_promotion() {
        let dir = tempfile::tempdir().unwrap();
        write_info(dir.path(), &sample()).unwrap();
        assert_eq!(read_info(dir.path()).unwrap(), sample());
    }

    #[test]
    fn stamp_fills_b_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write_info(dir.path(), &sample()).unwrap();

        stamp_promoted_inclusion_block(dir.path(), 456).unwrap();
        let stamped = read_info(dir.path()).unwrap();
        assert_eq!(stamped.promoted_inclusion_block, Some(456));
        assert_eq!(stamped.next_batch_nonce, 7, "other fields untouched");

        stamp_promoted_inclusion_block(dir.path(), 456).unwrap();
        assert_eq!(
            read_info(dir.path()).unwrap().promoted_inclusion_block,
            Some(456)
        );
    }

    #[test]
    fn read_rejects_unknown_duplicate_and_missing_keys() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(dir.path().join(INFO_FILE), "mystery = 1\n").unwrap();
        assert!(read_info(dir.path()).is_err(), "unknown key must reject");

        std::fs::write(
            dir.path().join(INFO_FILE),
            "format_version = 1\nformat_version = 1\n",
        )
        .unwrap();
        assert!(read_info(dir.path()).is_err(), "duplicate key must reject");

        std::fs::write(dir.path().join(INFO_FILE), "format_version = 1\n").unwrap();
        assert!(read_info(dir.path()).is_err(), "missing keys must reject");
    }

    #[test]
    fn on_disk_bytes_stay_simple_key_value_lines() {
        // The serialized form must remain plain `key = value` integer lines:
        // no `[table]` headers, no quoting. This is the operator-readable
        // contract and what any previously-written file already looks like.
        let dir = tempfile::tempdir().unwrap();
        let mut info = sample();
        info.promoted_inclusion_block = Some(456);
        write_info(dir.path(), &info).unwrap();
        let bytes = std::fs::read_to_string(dir.path().join(INFO_FILE)).unwrap();
        assert_eq!(
            bytes,
            "format_version = 1\nnext_batch_nonce = 7\nl2_tx_index = 123\n\
             promoted_inclusion_block = 456\n"
        );
    }

    #[test]
    fn read_tolerates_comments_blanks_and_reordering() {
        // The robustness win from real TOML over the old line parser: an
        // operator inspecting/annotating the file under recovery pressure can
        // add comments, blank lines, and reorder keys without bricking it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(INFO_FILE),
            "# checkpoint metadata\n\nl2_tx_index = 123\nnext_batch_nonce = 7\n\
             format_version = 1\n",
        )
        .unwrap();
        assert_eq!(read_info(dir.path()).unwrap(), sample());
    }

    #[test]
    fn diagnose_missing_dump_flags_watchdog_checkpoint_layout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manifest.json"), "{\"safe_block\":1}\n").unwrap();
        std::fs::create_dir(dir.path().join("snapshot")).unwrap();

        let msg = diagnose_missing_dump(dir.path());
        assert!(
            msg.contains("watchdog Cartesi Machine checkpoint"),
            "got: {msg}"
        );
        assert!(msg.contains("manifest.json and snapshot/"), "got: {msg}");
        assert!(msg.contains("dumps/<id>/"), "got: {msg}");

        let err = read_info(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("watchdog Cartesi Machine checkpoint"),
            "got: {err_msg}"
        );
        assert!(
            !err_msg.contains("os error 2"),
            "must not surface raw ENOENT alone, got: {err_msg}"
        );
    }

    #[test]
    fn diagnose_missing_dump_when_path_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let msg = diagnose_missing_dump(&missing);
        assert!(msg.contains("path does not exist"), "got: {msg}");
        assert!(msg.contains("info.toml"), "got: {msg}");
    }

    #[test]
    fn diagnose_missing_dump_when_state_present_without_info() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(APP_STATE_SUBDIR)).unwrap();
        let msg = diagnose_missing_dump(dir.path());
        assert!(msg.contains("missing `info.toml`"), "got: {msg}");
        assert!(msg.contains("found `state/`"), "got: {msg}");
    }

    #[test]
    fn read_rejects_wrong_format_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(INFO_FILE),
            "format_version = 999\nnext_batch_nonce = 0\nl2_tx_index = 0\n",
        )
        .unwrap();
        assert!(read_info(dir.path()).is_err());
    }
}
