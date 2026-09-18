// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Immutable restore-artifact metadata. Accepted recovery exports add a
//! separate `checkpoint.toml` receipt selected from SQLite under a dump lease.

use std::io;
use std::path::{Path, PathBuf};

use sequencer_core::application::{AppError, Application};

/// Name of the app-owned file or directory inside a dump directory.
const APP_STATE_SUBDIR: &str = "state";
/// Name of the sequencer-owned metadata file inside a dump directory.
const INFO_FILE: &str = "info.toml";

pub const FORMAT_VERSION: u64 = 2;

/// The app's dump prefix inside `dump_dir`. Pure path derivation.
pub fn app_prefix(dump_dir: &Path) -> PathBuf {
    dump_dir.join(APP_STATE_SUBDIR)
}

/// Whether an I/O error proves that a DB-referenced snapshot artifact is
/// missing or structurally corrupt. Other filesystem failures remain
/// operational: they may clear when the device, mount, or permissions recover.
pub(crate) fn referenced_artifact_io_is_terminal(source: &io::Error) -> bool {
    matches!(
        source.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::InvalidData
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::IsADirectory
    )
}

/// Metadata known when the application artifact is created.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DumpInfo {
    pub format_version: u64,
    pub next_batch_nonce: u64,
}

impl DumpInfo {
    pub fn at_batch_close(batch_nonce: u64) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            next_batch_nonce: batch_nonce.checked_add(1).expect("batch nonce overflow"),
        }
    }

    pub fn at_baseline(next_batch_nonce: u64) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            next_batch_nonce,
        }
    }
}

/// Acceptance receipt packaged with an operator's recovery export. Its block
/// is an exact end-of-block comparison boundary under per-batch snapshotting.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointInfo {
    pub format_version: u64,
    pub next_batch_nonce: u64,
    pub inclusion_block: u64,
}

pub fn read_checkpoint_info(dump_dir: &Path) -> io::Result<CheckpointInfo> {
    let content = std::fs::read_to_string(dump_dir.join("checkpoint.toml"))?;
    let info: CheckpointInfo =
        toml::from_str(&content).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if info.format_version != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported checkpoint format",
        ));
    }
    Ok(info)
}

/// Stream a complete, restorable artifact. A receipt makes it an accepted
/// recovery export; the on-disk dump remains unchanged.
pub(crate) fn write_archive<W: io::Write>(
    writer: W,
    dump_dir: &Path,
    next_batch_nonce: u64,
    checkpoint: Option<&CheckpointInfo>,
) -> io::Result<()> {
    let info = read_info(dump_dir)?;
    if info.next_batch_nonce != next_batch_nonce {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dump nonce differs from snapshot boundary",
        ));
    }
    let mut archive = tar::Builder::new(writer);
    archive.append_path_with_name(dump_dir.join(INFO_FILE), INFO_FILE)?;
    let state = app_prefix(dump_dir);
    if std::fs::metadata(&state)?.is_dir() {
        archive.append_dir_all(APP_STATE_SUBDIR, state)?;
    } else {
        archive.append_path_with_name(state, APP_STATE_SUBDIR)?;
    }
    if let Some(checkpoint) = checkpoint {
        let bytes = toml::to_string(checkpoint)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?
            .into_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        archive.append_data(&mut header, "checkpoint.toml", bytes.as_slice())?;
    }
    archive.finish()
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
/// files and directory entries (the dump dir); the final parent-dir fsync
/// persists the dump dir's own entry in the dumps directory.
pub fn create_dump_dir_with_info<A: Application>(
    app: &mut A,
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

/// Delete a checkpoint and its metadata, including incomplete creation remnants.
pub fn delete_dump_dir(dump_dir: &Path) -> io::Result<()> {
    std::fs::remove_dir_all(dump_dir)
}

/// Write `info.toml` into `dump_dir`, durably: temp file, fsync, rename
/// over, fsync the directory. Called only during artifact creation.
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
        "expected a sequencer dump at {} with `info.toml` and a `state` app artifact. \
         Recovery additionally requires `checkpoint.toml` from `/finalized_snapshot`. \
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
            "missing `info.toml` (found `state` but no checkpoint metadata) — {expected}"
        );
    }

    format!("missing `info.toml` — {expected}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use sequencer_core::application::{
        AppOutputs, ApplicationProgress, ValidationOutcome, execute_direct_input,
    };
    use sequencer_core::history::ExecutedInputCount;
    use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
    use sequencer_core::user_op::UserOp;

    #[derive(Default)]
    struct PrefixDumpApp<const DIRECTORY: bool> {
        progress: ApplicationProgress,
        flushes: usize,
    }

    impl<const DIRECTORY: bool> Application for PrefixDumpApp<DIRECTORY> {
        fn max_method_payload_bytes() -> usize {
            0
        }

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _fee: u16,
        ) -> Result<ValidationOutcome, AppError> {
            Ok(ValidationOutcome::Accept)
        }

        fn apply_valid_user_op(
            &mut self,
            _user_op: &ValidUserOp,
            safe_block: u64,
        ) -> Result<AppOutputs, AppError> {
            self.progress.advance(safe_block);
            Ok(Vec::new())
        }

        fn apply_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
            self.progress.advance(input.block_number);
            Ok(Vec::new())
        }

        fn progress(&self) -> ApplicationProgress {
            self.progress
        }

        fn from_dump(prefix: &Path) -> Result<Self, AppError> {
            let bytes = std::fs::read(Self::state_file_in_dump(prefix))?;
            let count = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let block = u64::from_le_bytes(bytes[8..].try_into().unwrap());
            if DIRECTORY {
                assert_eq!(std::fs::read(prefix.join("manifest"))?, b"complete");
            }
            Ok(Self {
                progress: ApplicationProgress::try_new(ExecutedInputCount::new(count), block)
                    .expect("coherent dumped progress"),
                flushes: 0,
            })
        }

        fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError> {
            self.flushes += 1;
            if DIRECTORY {
                std::fs::create_dir(prefix)?;
                std::fs::write(prefix.join("manifest"), b"complete")?;
                std::fs::File::open(prefix.join("manifest"))?.sync_all()?;
            }
            let bytes = [
                self.progress.executed_input_count().get().to_le_bytes(),
                self.progress.last_executed_safe_block().to_le_bytes(),
            ]
            .concat();
            let state = Self::state_file_in_dump(prefix);
            std::fs::write(&state, bytes)?;
            std::fs::File::open(state)?.sync_all()?;
            if DIRECTORY {
                std::fs::File::open(prefix)?.sync_all()?;
            }
            std::fs::File::open(prefix.parent().unwrap())?.sync_all()?;
            Ok(())
        }

        fn state_file_in_dump(prefix: &Path) -> PathBuf {
            if DIRECTORY {
                prefix.join("progress")
            } else {
                prefix.to_path_buf()
            }
        }
    }

    fn assert_dump_prefix_round_trip<const DIRECTORY: bool>() {
        let root = tempfile::tempdir().unwrap();
        let dump = root.path().join("original");
        let input = DirectInput {
            sender: Address::ZERO,
            block_number: 7,
            payload: vec![],
        };
        let mut app = PrefixDumpApp::<DIRECTORY>::default();
        execute_direct_input(&mut app, &input).unwrap();
        let checkpoint = app.progress();
        create_dump_dir_with_info(&mut app, &dump, &sample()).unwrap();
        assert_eq!(
            app.flushes, 1,
            "dump creation can flush mutable runtime state"
        );
        assert_eq!(
            app.progress(),
            checkpoint,
            "dumping preserves logical state"
        );
        assert_eq!(app_prefix(&dump).is_dir(), DIRECTORY);
        assert_eq!(read_info(&dump).unwrap(), sample());

        let mut first = PrefixDumpApp::<DIRECTORY>::from_dump(&app_prefix(&dump)).unwrap();
        let mut second = PrefixDumpApp::<DIRECTORY>::from_dump(&app_prefix(&dump)).unwrap();
        let sibling = root.path().join("sibling");
        create_dump_dir_with_info(&mut second, &sibling, &sample()).unwrap();
        delete_dump_dir(&dump).unwrap();
        assert!(!dump.exists(), "the entire checkpoint directory is removed");
        assert_eq!(
            PrefixDumpApp::<DIRECTORY>::from_dump(&app_prefix(&sibling))
                .unwrap()
                .progress(),
            checkpoint,
            "other checkpoints survive deletion"
        );
        execute_direct_input(&mut first, &input).unwrap();
        assert_eq!(app.progress(), checkpoint);
        assert_eq!(
            second.progress(),
            checkpoint,
            "restored instances own independent state"
        );
        let successor = root.path().join("successor");
        create_dump_dir_with_info(&mut first, &successor, &sample()).unwrap();
        let reloaded = PrefixDumpApp::<DIRECTORY>::from_dump(&app_prefix(&successor)).unwrap();
        assert_eq!(
            reloaded.progress(),
            first.progress(),
            "restored state survives source deletion"
        );
        delete_dump_dir(&successor).unwrap();
        delete_dump_dir(&sibling).unwrap();
    }

    #[test]
    fn single_file_and_directory_dumps_restore_independent_instances() {
        assert_dump_prefix_round_trip::<false>();
        assert_dump_prefix_round_trip::<true>();
    }

    #[test]
    fn archives_restore_file_and_directory_artifacts_without_mutating_metadata() {
        fn check<const DIRECTORY: bool>() {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source");
            let mut app = PrefixDumpApp::<DIRECTORY>::default();
            create_dump_dir_with_info(&mut app, &source, &sample()).unwrap();
            let original_info = std::fs::read(source.join(INFO_FILE)).unwrap();
            let receipt = CheckpointInfo {
                format_version: FORMAT_VERSION,
                next_batch_nonce: 7,
                inclusion_block: 42,
            };
            let mut bytes = Vec::new();
            write_archive(&mut bytes, &source, 7, Some(&receipt)).unwrap();
            assert_eq!(
                std::fs::read(source.join(INFO_FILE)).unwrap(),
                original_info
            );
            assert!(!source.join("checkpoint.toml").exists());
            delete_dump_dir(&source).unwrap();
            let destination = root.path().join("restored");
            tar::Archive::new(bytes.as_slice())
                .unpack(&destination)
                .unwrap();
            assert_eq!(read_checkpoint_info(&destination).unwrap(), receipt);
            let restored =
                PrefixDumpApp::<DIRECTORY>::from_dump(&app_prefix(&destination)).unwrap();
            assert_eq!(restored.progress(), app.progress());
        }
        check::<false>();
        check::<true>();
    }

    #[test]
    fn archive_propagates_missing_state_metadata() {
        let root = tempfile::tempdir().unwrap();
        write_info(root.path(), &DumpInfo::at_baseline(0)).unwrap();
        let error = write_archive(Vec::new(), root.path(), 0, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    fn sample() -> DumpInfo {
        DumpInfo {
            format_version: FORMAT_VERSION,
            next_batch_nonce: 7,
        }
    }

    #[test]
    fn immutable_info_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_info(dir.path(), &sample()).unwrap();
        assert_eq!(read_info(dir.path()).unwrap(), sample());
    }

    #[test]
    fn referenced_artifact_classifier_separates_corruption_from_operational_io() {
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::InvalidData,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::NotADirectory,
            io::ErrorKind::IsADirectory,
        ] {
            assert!(
                referenced_artifact_io_is_terminal(&io::Error::from(kind)),
                "{kind:?} proves a referenced artifact is unusable"
            );
        }
        assert!(
            !referenced_artifact_io_is_terminal(&io::Error::other("filesystem unavailable")),
            "unclassified filesystem failures remain operational"
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
    fn read_tolerates_comments_blanks_and_reordering() {
        // The robustness win from real TOML over the old line parser: an
        // operator inspecting/annotating the file under recovery pressure can
        // add comments, blank lines, and reorder keys without bricking it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(INFO_FILE),
            "# checkpoint metadata\n\nnext_batch_nonce = 7\nformat_version = 2\n",
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
        assert!(msg.contains("/finalized_snapshot"), "got: {msg}");
        assert!(msg.contains("checkpoint.toml"), "got: {msg}");

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
        assert!(msg.contains("found `state`"), "got: {msg}");
    }

    #[test]
    fn read_rejects_wrong_format_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(INFO_FILE),
            "format_version = 999\nnext_batch_nonce = 0\n",
        )
        .unwrap();
        assert!(read_info(dir.path()).is_err());
    }
}
