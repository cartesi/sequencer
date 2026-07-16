// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use app_core::application::default_private_keys;
use sequencer_rust_client::SequencerClient;
use tempfile::TempDir;
use tokio::process::{Child, Command};

use crate::HarnessResult;
use crate::paths;
use crate::rollups::{DEVNET_CHAIN_ID, DevnetRollupsStack};
use crate::util::{
    build_local_endpoint, io_other, path_as_str, send_graceful_terminate, timestamped_log_path,
    wait_for_http_readiness,
};
use crate::wallet::{TestSigner, WalletL1Client, WalletL2Client};
use crate::ws::WsClient;

const DEFAULT_SEQUENCER_START_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// Cadence at which the harness mines an L1 block during a recovery boot's
/// readiness wait when [`ManagedSequencer::set_mine_l1_during_boot`] is on.
/// Fast enough that Anvil's `safe` tag advances past the flushed nonce within
/// the readiness window; the exact value is a harness detail, not test-visible.
const BOOT_L1_MINE_INTERVAL: Duration = Duration::from_millis(200);
/// Readiness budget for a *recovery* boot (mining-during-boot enabled). The WP2
/// mempool flush polls `get_transaction_count(Safe)` once per L1 block time
/// (`safe_poll_interval = seconds_per_block`, 12 s here), so it cannot resolve
/// the stranded slot in under one poll cycle — well past the 10 s normal-boot
/// budget. Allow several cycles of slack so a recovery boot has room to flush,
/// cascade, and come up. Recovery boots are legitimately slow (review R4 class
/// 10); this is the harness counterpart to that.
const RECOVERY_BOOT_START_TIMEOUT: Duration = Duration::from_secs(45);
const DEFAULT_SEQUENCER_RUST_LOG: &str = "info";
pub const DEFAULT_DEVNET_SEQUENCER_BIN: &str = "target/debug/wallet-sequencer-devnet";
pub const DEFAULT_TEST_LOGS_DIR: &str = "tests/e2e/results";

#[derive(Debug, Clone)]
pub struct ManagedSequencerConfig {
    pub sequencer_bin: PathBuf,
    pub log_prefix: String,
    pub logs_dir: PathBuf,
    /// When false, the child runs without libfaketime (for tests that never
    /// manipulate wall clock). Requires libfaketime in PATH when true.
    pub faketime: bool,
}

/// Snapshot of the `batches` table. Returned by
/// [`ManagedSequencer::count_batches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchCounts {
    pub total: u64,
    pub sealed: u64,
    pub invalidated: u64,
}

/// Drive the next respawn's `setup` phase into `--recovery` mode against a
/// captured checkpoint. Set via [`ManagedSequencer::set_recovery_setup`] before
/// wiping the DB and respawning.
#[derive(Debug, Clone)]
pub struct RecoverySetupParams {
    /// `B` — the checkpoint's L1 inclusion block (`--checkpoint-block`).
    pub checkpoint_block: u64,
    /// The captured checkpoint dump dir (`--checkpoint-dump-dir`).
    pub checkpoint_dump_dir: PathBuf,
}

/// A checkpoint captured from a running sequencer's finalized snapshot, ready to
/// drive `setup --recovery`. Returned by
/// [`ManagedSequencer::capture_finalized_checkpoint`].
#[derive(Debug, Clone)]
pub struct RecoveryCheckpoint {
    /// The copied dump dir (lives outside the DB/`dumps` reset scope, so it
    /// survives [`ManagedSequencer::reset_database`]).
    pub dir: PathBuf,
    /// `B` — the finalized snapshot's L1 inclusion block.
    pub checkpoint_block: u64,
    /// `N` — the snapshot's recorded resume nonce (`info.toml next_batch_nonce`).
    pub resume_nonce: u64,
}

/// Outcome of a single [`ManagedSequencer::respawn_and_watch`] attempt.
#[derive(Debug)]
pub enum RespawnAttemptOutcome {
    /// The child came up and stayed alive for the requested stabilization
    /// window.
    Stable,
    /// `respawn()` itself returned `Err` — the child exited during bootstrap
    /// before HTTP became ready. Typically surfaces
    /// `RecoveryError::Refuse(...)` from the startup decision table.
    RespawnFailed(String),
    /// `respawn()` returned `Ok` but the child exited within the
    /// stabilization window. Typically surfaces a danger-detector worker
    /// exit from the first post-boot poll.
    ExitedPostRespawn(std::process::ExitStatus),
}

impl RespawnAttemptOutcome {
    pub fn is_stable(&self) -> bool {
        matches!(self, Self::Stable)
    }
}

/// Parameters for [`ManagedSequencer::respawn_until_stable`]. See that
/// method's doc for how `advance_per_retry` interacts with the restart cycle.
#[derive(Debug, Clone)]
pub struct RespawnPolicy {
    pub max_attempts: u32,
    pub stabilization: Duration,
    pub advance_per_retry: Option<Duration>,
}

/// Which child in the devnet stack exited during [`ManagedSequencer::observe_stack_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackChildExit {
    Sequencer,
    Anvil,
}

pub struct ManagedSequencer {
    rollups: DevnetRollupsStack,
    child: Child,
    shutdown_timeout: Duration,
    sequencer_bin: PathBuf,
    log_prefix: String,
    logs_dir: PathBuf,
    _data_dir: TempDir,
    data_dir_path: PathBuf,
    endpoint: String,
    log_path: PathBuf,
    /// Overrides the `--eth-rpc-url` the sequencer uses. When `None`, the
    /// sequencer dials Anvil directly. When `Some(url)`, it dials the
    /// override (e.g., a `TcpProxy` in front of Anvil for outage tests).
    /// Persists across `respawn()` so post-restart behavior is consistent.
    l1_endpoint_override: Option<String>,
    /// Overrides the `--chain-id` argument passed to the sequencer binary.
    /// When `None`, defaults to `DEVNET_CHAIN_ID` (matches Anvil). Set to
    /// a non-matching value to test chain-id-mismatch failure modes
    chain_id_override: Option<u64>,
    /// Cached libfaketime dylib/so path (computed once on spawn). `None` when
    /// [`ManagedSequencerConfig::faketime`] is false.
    libfaketime_path: Option<PathBuf>,
    /// Path to the file libfaketime re-reads for its offset. `None` when
    /// faketime is disabled.
    faketime_rc_path: Option<PathBuf>,
    /// Internal cumulative forward-offset tracker for
    /// [`Self::advance_wall_and_mine`]. Not touched by
    /// [`Self::set_faketime_offset`].
    cumulative_offset_secs: u64,
    /// When `true`, [`Self::respawn`] mines L1 blocks concurrently with the
    /// post-boot readiness wait so a recovery boot's mempool flush can make
    /// progress. See [`Self::set_mine_l1_during_boot`]. Persists across
    /// respawns like the other overrides.
    mine_l1_during_boot: bool,
    /// When `Some`, [`Self::respawn`] runs the `setup` phase in `--recovery`
    /// mode against this checkpoint (and mines L1 concurrently with setup so
    /// its flush can settle). See [`Self::set_recovery_setup`].
    recovery_setup: Option<RecoverySetupParams>,
    /// When `Some(secs)`, passes `--max-batch-open-seconds` to the `run` phase
    /// so the inclusion lane seals the open batch promptly (the default is 2h).
    /// Used by tests that need a batch to close + be promoted. Persists across
    /// respawns. See [`Self::set_max_batch_open_seconds`].
    max_batch_open_seconds: Option<u64>,
}

pub fn default_devnet_sequencer_config(log_prefix: impl Into<String>) -> ManagedSequencerConfig {
    ManagedSequencerConfig {
        sequencer_bin: paths::resolve_devnet_sequencer_bin(),
        log_prefix: log_prefix.into(),
        logs_dir: PathBuf::from(DEFAULT_TEST_LOGS_DIR),
        faketime: true,
    }
}

/// Devnet config without libfaketime (watchdog compare and other wall-clock-neutral tests).
pub fn devnet_sequencer_config_no_faketime(
    log_prefix: impl Into<String>,
) -> ManagedSequencerConfig {
    ManagedSequencerConfig {
        faketime: false,
        ..default_devnet_sequencer_config(log_prefix)
    }
}

impl ManagedSequencer {
    pub async fn spawn(config: ManagedSequencerConfig) -> HarnessResult<Self> {
        let logs_dir = paths::resolve_from_workspace(&config.logs_dir);
        let sequencer_bin = if config.sequencer_bin.is_absolute() {
            config.sequencer_bin.clone()
        } else {
            paths::resolve_from_workspace(&config.sequencer_bin)
        };
        let log_prefix = config.log_prefix;
        let rollups = DevnetRollupsStack::spawn(log_prefix.as_str(), logs_dir.as_path()).await?;

        fs::create_dir_all(logs_dir.as_path())?;
        let data_dir = TempDir::new()
            .map_err(|err| io_other(format!("failed to create temp data dir: {err}")))?;
        let data_dir_path = data_dir.path().to_path_buf();

        let (libfaketime_path, faketime_rc_path) = if config.faketime {
            let libfaketime_path = find_libfaketime()?;
            let faketime_rc_path = data_dir_path.join("faketime.rc");
            fs::write(faketime_rc_path.as_path(), "+0\n")
                .map_err(|err| io_other(format!("create faketime rc file: {err}")))?;
            (Some(libfaketime_path), Some(faketime_rc_path))
        } else {
            (None, None)
        };

        let SpawnedSequencerProcess {
            child,
            endpoint,
            log_path,
        } = spawn_sequencer_process(
            sequencer_bin.as_path(),
            log_prefix.as_str(),
            logs_dir.as_path(),
            data_dir_path.as_path(),
            &rollups,
            None,
            None,
            libfaketime_path.as_deref(),
            faketime_rc_path.as_deref(),
            // First boot has no stranded nonce to flush, so it never needs L1
            // to advance during readiness.
            false,
            // First boot is always genesis setup, never recovery.
            None,
            // Default batch-open deadline on first boot.
            None,
        )
        .await?;

        Ok(Self {
            rollups,
            child,
            shutdown_timeout: DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT,
            sequencer_bin,
            log_prefix,
            logs_dir,
            _data_dir: data_dir,
            data_dir_path,
            endpoint,
            log_path,
            l1_endpoint_override: None,
            chain_id_override: None,
            faketime_rc_path,
            libfaketime_path,
            cumulative_offset_secs: 0,
            mine_l1_during_boot: false,
            recovery_setup: None,
            max_batch_open_seconds: None,
        })
    }

    /// Configure the sequencer to dial `l1_endpoint` instead of Anvil directly.
    /// The override applies to the *next* `respawn()` and persists until cleared.
    /// Intended for tests that route through a [`crate::TcpProxy`].
    ///
    /// Does not affect the currently-running sequencer process.
    pub fn set_l1_endpoint_override(&mut self, l1_endpoint: Option<String>) {
        self.l1_endpoint_override = l1_endpoint;
    }

    /// Override the `--chain-id` argument passed to the `setup` phase on the
    /// next [`Self::respawn`]. When `None`, defaults to the devnet chain id
    /// (matches Anvil).
    ///
    /// Used by chain-id mismatch tests to inject a mismatched chain id and
    /// assert that bootstrap refuses before silently pinning a wrong identity.
    /// NOTE (post setup/run split): `--chain-id` now flows only to `setup`,
    /// which is idempotent and early-returns once the `setup_complete` marker
    /// exists — so the override only takes effect on a fresh or
    /// [`Self::reset_database`]-d data dir. On an already-set-up DB, `setup`
    /// is a no-op and `run` validates against the *pinned* chain id, so the
    /// override is silently inert. Does not affect a running process.
    pub fn set_chain_id_override(&mut self, chain_id: Option<u64>) {
        self.chain_id_override = chain_id;
    }

    /// Make [`Self::respawn`] (this one and subsequent ones, until cleared)
    /// mine L1 blocks concurrently with the post-boot readiness wait.
    ///
    /// Why this exists: a *recovery* boot whose persisted state names a wallet
    /// nonce that L1 never accepted — e.g. a batch-submission tx was dropped
    /// from the mempool ([`Self::drop_all_pending_txs`]) — runs the WP2 mempool
    /// flush during startup. The flush submits a no-op at the stranded nonce
    /// slot and blocks until that slot becomes *safe* (`safe_nonce >=
    /// watermark + 1`, review R1a). Re-enabling auto-mining is not enough:
    /// auto-mining lands the no-op tx but mints no *further* blocks, so Anvil's
    /// `safe` tag never advances past it and the flush waits until the boot
    /// hits the readiness timeout. In production the chain keeps producing
    /// blocks throughout a slow recovery boot; an on-demand-mining devnet
    /// produces none. This makes the harness mine a steady trickle for the
    /// boot window (dropped the instant readiness is reached), reproducing that
    /// drift so the flush — and the cascade behind it — can complete on the
    /// first respawn.
    ///
    /// Only the tests that deliberately strand a submitted nonce before a
    /// recovery respawn need this. Leave it off otherwise: every other boot
    /// reaches readiness without new blocks, and mining empty blocks during an
    /// unrelated boot would perturb L1-block-sensitive assertions.
    pub fn set_mine_l1_during_boot(&mut self, enabled: bool) {
        self.mine_l1_during_boot = enabled;
    }

    /// Write a faketime offset to the rc file. Effective **immediately** for
    /// the running sequencer (if any) and persists across respawns. The
    /// libfaketime library re-reads the file on every time call (we pass
    /// `FAKETIME_NO_CACHE=1`), so the next `SystemTime::now()` inside the
    /// child sees the new offset.
    ///
    /// Format follows faketime's `-f` flag: `"+5h"`, `"-1h"`, `"+1d"`, or
    /// `"+NNNs"` for absolute seconds. Passing `None` resets to `+0`.
    /// See `man faketime` for advanced options (speed-up, interval mode).
    ///
    /// Does not mine L1 blocks — use [`Self::advance_wall_and_mine`] when you
    /// want wall-clock and L1 to move together.
    ///
    /// Replaces any cumulative advance tracked by
    /// [`Self::advance_wall_and_mine`], and resets its counter.
    pub fn set_faketime_offset(&mut self, offset: Option<String>) -> HarnessResult<()> {
        let rc = self
            .faketime_rc_path
            .as_ref()
            .ok_or_else(|| io_other("faketime is disabled for this ManagedSequencer"))?;
        let s = offset.as_deref().unwrap_or("+0");
        fs::write(rc.as_path(), format!("{s}\n"))
            .map_err(|err| io_other(format!("write faketime rc file: {err}")))?;
        self.cumulative_offset_secs = 0;
        Ok(())
    }

    /// Delete the sequencer DB file (and its `-wal` / `-shm` siblings) AND the
    /// `dumps/` subtree, simulating a brand-new install — no pinned identity,
    /// no batches, no safe-input rows, no genesis snapshot. Call while the
    /// sequencer is stopped. (Clearing the DB but not `dumps/` would leave a
    /// stale `genesis-*` dir; the next `setup` mints a fresh one and the old
    /// one lingers until run's orphan sweep — harmless, but not "brand-new".)
    ///
    /// Distinct from "clear only the identity row": that would leave the DB
    /// holding `batches` / `safe_inputs` / `l1_safe_head` rows from prior
    /// boots, which `IdentityError::OrphanedState` then refuses on the next
    /// `setup`. The right "first boot" simulation is to start from an empty DB.
    ///
    /// Used by the no-cache-bootstrap and live-RPC chain-id-mismatch tests:
    /// both want to exercise the no-cached-identity bootstrap path (next boot
    /// re-runs `setup`).
    pub fn reset_database(&self) -> HarnessResult<()> {
        let db_path = self.data_dir_path.join("sequencer.db");
        for suffix in ["", "-wal", "-shm"] {
            let path = db_path.with_extension(format!("db{suffix}"));
            match fs::remove_file(path.as_path()) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(io_other(format!("reset DB ({path:?}): {err}")).into()),
            }
        }
        let dumps_dir = self.data_dir_path.join("dumps");
        match fs::remove_dir_all(dumps_dir.as_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_other(format!("reset dumps ({dumps_dir:?}): {err}")).into()),
        }
        Ok(())
    }

    /// Drive the next [`Self::respawn`]'s `setup` phase into `--recovery` against
    /// `params` (and mine L1 concurrently with setup so its flush can settle).
    /// Set this after capturing a checkpoint and wiping the DB. `None` restores
    /// the normal (genesis) setup. Persists across respawns until cleared.
    pub fn set_recovery_setup(&mut self, params: Option<RecoverySetupParams>) {
        self.recovery_setup = params;
    }

    /// Pass `--max-batch-open-seconds` to the `run` phase on the next (and
    /// subsequent) respawns so the inclusion lane seals the open batch after
    /// `secs` instead of the 2h default. `None` restores the default. Lets a
    /// test drive a batch to close + promotion without a 2h clock advance.
    pub fn set_max_batch_open_seconds(&mut self, secs: Option<u64>) {
        self.max_batch_open_seconds = secs;
    }

    /// Read the finalized snapshot's `(inclusion_block B, resume_nonce N)`. `B`
    /// is `0` for the genesis snapshot and `> 0` once a real batch has been
    /// promoted — poll this (mining L1 in between) to wait for a recoverable
    /// checkpoint. Read-only.
    pub fn finalized_snapshot_info(&self) -> HarnessResult<(u64, u64)> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;
        let (prefix, inclusion_block): (String, i64) = conn
            .query_row(
                "SELECT d.prefix, f.inclusion_block FROM finalized_snapshot f \
                 JOIN dumps d ON d.id = f.dump_id WHERE f.singleton_id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|err| io_other(format!("read finalized_snapshot: {err}")))?;
        let resume_nonce = read_info_next_batch_nonce(Path::new(&prefix))?;
        Ok((inclusion_block as u64, resume_nonce))
    }

    /// Copy the current finalized snapshot dump to `<data_dir>/checkpoint` (which
    /// survives [`Self::reset_database`], since that only clears `sequencer.db*`
    /// and `dumps/`), returning the captured checkpoint. Call after a batch has
    /// been promoted (`finalized_snapshot_info().0 > 0`).
    pub fn capture_finalized_checkpoint(&self) -> HarnessResult<RecoveryCheckpoint> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;
        let (prefix, inclusion_block): (String, i64) = conn
            .query_row(
                "SELECT d.prefix, f.inclusion_block FROM finalized_snapshot f \
                 JOIN dumps d ON d.id = f.dump_id WHERE f.singleton_id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|err| io_other(format!("read finalized_snapshot: {err}")))?;
        let src = PathBuf::from(prefix);
        let dst = self.data_dir_path.join("checkpoint");
        if dst.exists() {
            fs::remove_dir_all(dst.as_path())
                .map_err(|err| io_other(format!("clear prior checkpoint ({dst:?}): {err}")))?;
        }
        copy_dir_recursive(src.as_path(), dst.as_path())
            .map_err(|err| io_other(format!("copy checkpoint {src:?} -> {dst:?}: {err}")))?;
        let resume_nonce = read_info_next_batch_nonce(dst.as_path())?;
        Ok(RecoveryCheckpoint {
            dir: dst,
            checkpoint_block: inclusion_block as u64,
            resume_nonce,
        })
    }

    /// Rewrite the L1 safe-head observation to "unknown", simulating a DB
    /// that has never successfully synced from L1. Call while the sequencer
    /// is stopped.
    ///
    /// Used by the wall-clock-never-synced test: the danger check treats a
    /// missing safe-head row as an unusable L1 view and refuses to
    /// proceed. Deleting this row while the deployment identity is
    /// populated lets us hit that branch without losing the pinned chain
    /// ID / InputBox address (which would fail earlier in bootstrap, not
    /// in the wall-clock fallback).
    pub fn clear_l1_safe_head_observation(&self) -> HarnessResult<()> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open(db_path.as_path())
            .map_err(|err| io_other(format!("open DB: {err}")))?;
        conn.execute("DELETE FROM l1_safe_head WHERE singleton_id = 0", [])
            .map_err(|err| io_other(format!("reset L1 safe-head observation: {err}")))?;
        Ok(())
    }

    /// Read-only snapshot of the `safe_accepted_batches` view: rows
    /// recovered from the L1-side scheduler frontier (i.e., batches the
    /// sequencer has *observed accepted on chain*). Returns `(count,
    /// min_nonce)` — count is the row count, min_nonce is `MIN(nonce)` or
    /// `None` if empty.
    ///
    /// Used by the nonce-0 recovery test to confirm a recovery batch (which reuses nonce 0)
    /// actually lands and gets accepted on L1 — proving the
    /// `populate_safe_accepted_batches` cursor handles
    /// reused-nonce-after-cascade correctly.
    pub fn count_safe_accepted_batches(&self) -> HarnessResult<(u64, Option<u64>)> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM safe_accepted_batches", [], |row| {
                row.get(0)
            })
            .map_err(|err| io_other(format!("count safe_accepted_batches: {err}")))?;
        let min_nonce: Option<i64> = conn
            .query_row("SELECT MIN(nonce) FROM safe_accepted_batches", [], |row| {
                row.get(0)
            })
            .map_err(|err| io_other(format!("min nonce: {err}")))?;
        Ok((count as u64, min_nonce.map(|n| n as u64)))
    }

    /// The batch-tree anchor nonce, read from the run DB read-only: `N'` after
    /// cockroach recovery, `0` for a genesis deployment. Lets a recovery e2e
    /// assert the rebuilt tree is rooted at the checkpoint's resume nonce (I16).
    pub fn batch_tree_anchor(&self) -> HarnessResult<u64> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;
        let anchor: i64 = conn
            .query_row(
                "SELECT nonce FROM batch_tree_anchor WHERE singleton_id = 0",
                [],
                |row| row.get(0),
            )
            .map_err(|err| io_other(format!("read batch_tree_anchor: {err}")))?;
        Ok(anchor as u64)
    }

    /// The canonical-divergence marker (review R2/I15) from the run DB, or `None`
    /// when the frontier is healthy. A recovery/resync e2e asserts this is `None`
    /// to prove the content-identity check did NOT spuriously freeze the frontier
    /// (e.g. a false-positive against below-anchor collapsed history) — otherwise
    /// that silent-failure class is invisible at e2e level.
    pub fn canonical_divergence(&self) -> HarnessResult<Option<String>> {
        use rusqlite::OptionalExtension;
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;
        conn.query_row(
            "SELECT kind FROM canonical_divergence WHERE singleton_id = 0",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| io_other(format!("read canonical_divergence: {err}")).into())
    }

    /// Snapshot of the `batches` table: `(total, sealed, invalidated)`.
    /// Reads the DB file read-only; safe to call while the sequencer is
    /// running. Useful for asserting that batch closure happened during a
    /// test segment (e.g., the sequencer kept processing through an outage).
    pub fn count_batches(&self) -> HarnessResult<BatchCounts> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM batches", [], |row| row.get(0))
            .map_err(|err| io_other(format!("count batches: {err}")))?;
        let sealed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE sealed_at_ms IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(|err| io_other(format!("count sealed batches: {err}")))?;
        let invalidated: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE invalidated_at_ms IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(|err| io_other(format!("count invalidated batches: {err}")))?;

        Ok(BatchCounts {
            total: total as u64,
            sealed: sealed as u64,
            invalidated: invalidated as u64,
        })
    }

    /// Assert the schema-level tree invariants on the sequencer's DB. Runs
    /// against the DB file read-only; safe to call whether the sequencer is
    /// running or stopped (SQLite WAL + read-only flag handles concurrent
    /// writers).
    ///
    /// Invariants checked:
    ///   1. At most one `valid_open_batch` row (partial unique index
    ///      `ux_single_valid_tip` should guarantee this structurally —
    ///      we verify it in case the index ever regressed).
    ///   2. Every valid batch's `nonce` equals `parent.nonce + 1`, or 0 if
    ///      `parent_batch_index IS NULL`.
    ///   3. Every `parent_batch_index` is NULL or references an existing
    ///      batch (FK-backed, verified explicitly for cross-DB-tool
    ///      portability).
    ///   4. The nonces on the valid path form a contiguous `0..N` sequence.
    ///
    /// Panics with a specific violation message if any invariant fails.
    /// Harness-only check (no sequencer changes) that catches regressions
    /// which slip past user-visible e2e assertions.
    pub fn assert_schema_invariants(&self) -> HarnessResult<()> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;

        // The batch-tree anchor: the nonce the single parentless root carries
        // (0 for a genesis deployment, N' for a cockroach-recovered one). The
        // root/contiguity invariants below are stated relative to it, so a
        // recovered tree (rooted at N') passes the same checks a genesis tree
        // (rooted at 0) does. Mirrors `trg_enforce_nonce_contiguity`. Reuses the
        // `batch_tree_anchor()` accessor (its own short-lived read-only handle)
        // rather than re-issuing the query against `conn`.
        let anchor = self.batch_tree_anchor()? as i64;

        // 1. At most one valid open batch.
        let open_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM valid_open_batch", [], |row| {
                row.get(0)
            })
            .map_err(|err| io_other(format!("count valid_open_batch: {err}")))?;
        if open_count > 1 {
            panic!("schema invariant: more than one valid Tip ({open_count} rows)");
        }

        // 1b. At most one *valid* parentless root (the anchored root).
        let valid_root_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM valid_batches WHERE parent_batch_index IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(|err| io_other(format!("count valid roots: {err}")))?;
        if valid_root_count > 1 {
            panic!(
                "schema invariant: more than one valid parentless root ({valid_root_count} rows)"
            );
        }

        // 2. Nonce contiguity via parent.
        let mut stmt = conn
            .prepare(
                "SELECT b.batch_index, b.parent_batch_index, b.nonce, p.nonce \
                 FROM batches b LEFT JOIN batches p ON p.batch_index = b.parent_batch_index",
            )
            .map_err(|err| io_other(format!("prepare nonce-check: {err}")))?;
        let rows: Vec<(i64, Option<i64>, i64, Option<i64>)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map_err(|err| io_other(format!("query nonce-check: {err}")))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|err| io_other(format!("collect nonce-check: {err}")))?;
        for (bi, parent, nonce, parent_nonce) in &rows {
            match (parent, parent_nonce) {
                (None, _) => {
                    if *nonce != anchor {
                        panic!(
                            "schema invariant: batch {bi} has NULL parent but nonce {nonce} (expected anchor {anchor})"
                        );
                    }
                }
                (Some(p), None) => {
                    panic!(
                        "schema invariant: batch {bi}'s parent {p} doesn't exist (FK violation)"
                    );
                }
                (Some(_), Some(pn)) => {
                    if *nonce != pn + 1 {
                        panic!(
                            "schema invariant: batch {bi} nonce={nonce}, expected parent.nonce+1 = {}",
                            pn + 1
                        );
                    }
                }
            }
        }

        // 3. Valid-path nonce uniqueness and contiguity.
        let mut stmt = conn
            .prepare("SELECT nonce FROM valid_batches ORDER BY nonce ASC")
            .map_err(|err| io_other(format!("prepare valid-nonces: {err}")))?;
        let valid_nonces: Vec<i64> = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|err| io_other(format!("query valid-nonces: {err}")))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|err| io_other(format!("collect valid-nonces: {err}")))?;
        for pair in valid_nonces.windows(2) {
            if pair[0] == pair[1] {
                panic!(
                    "schema invariant: duplicate valid nonce {} in {valid_nonces:?}",
                    pair[0]
                );
            }
        }
        for (i, &n) in valid_nonces.iter().enumerate() {
            if n != anchor + i as i64 {
                panic!(
                    "schema invariant: valid nonces not contiguous from anchor {anchor}: {valid_nonces:?}"
                );
            }
        }

        Ok(())
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn log_path(&self) -> &Path {
        self.log_path.as_path()
    }

    pub fn data_dir(&self) -> &Path {
        self.data_dir_path.as_path()
    }

    pub fn domain_chain_id(&self) -> u64 {
        DEVNET_CHAIN_ID
    }

    pub fn verifying_contract(&self) -> Address {
        self.rollups.app_address()
    }

    pub fn l1_endpoint(&self) -> &str {
        self.rollups.l1_endpoint()
    }

    pub fn app_address(&self) -> Address {
        self.rollups.app_address()
    }

    pub fn input_box_address(&self) -> Address {
        self.rollups.input_box_address()
    }

    pub fn erc20_portal_address(&self) -> Address {
        self.rollups.erc20_portal_address()
    }

    pub fn supported_erc20_token(&self) -> Address {
        self.rollups.supported_erc20_token()
    }

    pub async fn deploy_extra_mock_erc20(&self) -> HarnessResult<Address> {
        self.rollups.deploy_extra_mock_erc20().await
    }

    pub async fn mine_l1_blocks(&self, block_count: u64) -> HarnessResult<()> {
        self.rollups.mine_l1_blocks(block_count).await
    }

    /// Toggle Anvil's auto-mining mode. When disabled, txs accumulate in
    /// the mempool until an explicit mine or re-enable. Used to hold a
    /// sequencer's batch-submission tx out of a block while the chain
    /// advances, reproducing the "delayed inclusion" fault that the
    /// scheduler handles by skipping past-stale batches.
    pub async fn set_automine(&self, enabled: bool) -> HarnessResult<()> {
        self.rollups.set_automine(enabled).await
    }

    /// Drop every pending tx from Anvil's mempool. Typical use: after the
    /// sequencer has submitted a batch-submission tx, drop it to simulate
    /// a gateway losing the payload. Combined with `mine_l1_blocks` to
    /// advance the chain without the dropped tx landing, this reproduces
    /// the "tx never mined" variant of delayed-inclusion.
    pub async fn drop_all_pending_txs(&self) -> HarnessResult<()> {
        self.rollups.drop_all_pending_txs().await
    }

    /// Advance both the sequencer's wall clock and the L1 chain by `duration`,
    /// maintaining the block-time coupling invariant (`seconds_per_block`,
    /// default 12 for Ethereum mainnet parity).
    ///
    /// This is the primary tool for simulating elapsed outage time. Effective
    /// **immediately** — works whether the sequencer is running or stopped:
    ///   - The faketime rc file is updated; the running sequencer's next time
    ///     call (or a post-respawn first call) sees the shifted clock.
    ///   - Anvil mines `duration.as_secs() / SECONDS_PER_BLOCK` blocks.
    ///
    /// **Cumulative**: calling with `1h` twice totals `+2h`, not `+1h`. Use
    /// [`Self::set_faketime_offset`] to jump to a specific offset or reset.
    ///
    /// Tests that need decoupled wall-clock vs L1 (e.g., the `saturating_sub`
    /// backward-jump test) should use [`Self::set_faketime_offset`] and
    /// [`Self::mine_l1_blocks`] directly.
    ///
    /// Assumes `CARTESI_SEQUENCER_SECONDS_PER_BLOCK = 12`. If a test changes that via env,
    /// this helper's block count will be wrong — prefer the direct dials in
    /// that case.
    pub async fn advance_wall_and_mine(&mut self, duration: Duration) -> HarnessResult<()> {
        const SECONDS_PER_BLOCK: u64 = 12;
        let secs = duration.as_secs();
        let blocks = secs / SECONDS_PER_BLOCK;
        self.mine_l1_blocks(blocks).await?;
        self.cumulative_offset_secs = self.cumulative_offset_secs.saturating_add(secs);
        let rc = self
            .faketime_rc_path
            .as_ref()
            .ok_or_else(|| io_other("faketime is disabled for this ManagedSequencer"))?;
        fs::write(rc.as_path(), format!("+{}s\n", self.cumulative_offset_secs))
            .map_err(|err| io_other(format!("write faketime rc file: {err}")))?;
        Ok(())
    }

    /// Watch the sequencer child for `grace` time without consuming its
    /// exit handle.
    ///
    /// - Returns `Ok(None)` if the child is still alive when `grace`
    ///   elapses. The internal `wait()` future is dropped, so subsequent
    ///   calls to [`Self::wait_for_exit`] / [`Self::respawn_and_watch`]
    ///   still work.
    /// - Returns `Ok(Some(status))` if the child exited inside the
    ///   window. The exit status is captured and the child is reaped;
    ///   the caller shouldn't call `wait_for_exit` afterwards (it would
    ///   hang).
    ///
    /// Used by negative-control tests that need to assert the sequencer
    /// *stayed up* across a condition that, if a bug existed, would make
    /// it exit.
    pub async fn observe_for(
        &mut self,
        grace: Duration,
    ) -> HarnessResult<Option<std::process::ExitStatus>> {
        tokio::select! {
            wait_result = self.child.wait() => {
                let status = wait_result
                    .map_err(|err| io_other(format!("child.wait(): {err}")))?;
                Ok(Some(status))
            }
            _ = tokio::time::sleep(grace) => Ok(None),
        }
    }

    /// Like [`Self::observe_for`], but also watches the Anvil child. Returns
    /// which process exited when one dies inside `grace`.
    pub async fn observe_stack_for(
        &mut self,
        grace: Duration,
    ) -> HarnessResult<Option<(StackChildExit, std::process::ExitStatus)>> {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|err| io_other(format!("sequencer.try_wait(): {err}")))?
            {
                return Ok(Some((StackChildExit::Sequencer, status)));
            }
            if let Some(status) = self
                .rollups
                .try_wait_anvil()
                .map_err(|err| io_other(format!("anvil.try_wait(): {err}")))?
            {
                return Ok(Some((StackChildExit::Anvil, status)));
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let remaining = deadline - now;
            tokio::time::sleep(std::cmp::min(remaining, Duration::from_millis(200))).await;
        }
    }

    /// Wait for the sequencer process to exit on its own. Returns the
    /// process's exit status. Times out after `timeout` to avoid hanging
    /// tests when the process refuses to exit.
    ///
    /// Used by tests that expect the sequencer to detect a condition
    /// (e.g., wall-clock danger) and self-exit with a non-zero status.
    /// After this returns, call [`Self::respawn`] to start a fresh process.
    pub async fn wait_for_exit(
        &mut self,
        timeout: Duration,
    ) -> HarnessResult<std::process::ExitStatus> {
        let status = tokio::time::timeout(timeout, self.child.wait())
            .await
            .map_err(|_| {
                io_other(format!(
                    "wait_for_exit: sequencer did not exit within {timeout:?}"
                ))
            })?
            .map_err(|err| io_other(format!("wait_for_exit: {err}")))?;
        Ok(status)
    }

    /// Respawn the sequencer and watch the child for `stabilization` to
    /// confirm it stays alive. Classifies the outcome so tests can model an
    /// orchestrator restart cycle without re-deriving the failure modes.
    ///
    /// There are two distinct "unstable" shapes the sequencer can take:
    ///   - The child dies during bootstrap (before HTTP readiness), which
    ///     makes `respawn()` itself return `Err`. Canonical cause:
    ///     `RecoveryError::Refuse(...)` from the startup decision table
    ///     when L1 is unreachable and the persisted state looks stalled.
    ///   - The child comes up (HTTP ready, bootstrap passed), then one of
    ///     the internal tasks returns a fatal error and the process exits.
    ///     Canonical cause: danger-detector worker exit when the first
    ///     post-boot poll sees a batch past `danger_threshold`.
    ///
    /// The race between bootstrap-finishes and submitter-first-tick is
    /// short (the poll interval is 5s by default, but the first tick runs
    /// immediately), so both cases can surface for a single logical event —
    /// tests should generally treat either as "not stable" and retry.
    ///
    /// Callers must ensure the previous child is already reaped (via
    /// [`Self::stop`] or [`Self::wait_for_exit`]) — same rule as
    /// [`Self::respawn`].
    pub async fn respawn_and_watch(
        &mut self,
        stabilization: Duration,
    ) -> HarnessResult<RespawnAttemptOutcome> {
        if let Err(err) = self.respawn().await {
            return Ok(RespawnAttemptOutcome::RespawnFailed(err.to_string()));
        }
        tokio::select! {
            wait_result = self.child.wait() => {
                let status = wait_result
                    .map_err(|err| io_other(format!("child.wait(): {err}")))?;
                Ok(RespawnAttemptOutcome::ExitedPostRespawn(status))
            }
            _ = tokio::time::sleep(stabilization) => {
                Ok(RespawnAttemptOutcome::Stable)
            }
        }
    }

    /// Loop [`Self::respawn_and_watch`] until the sequencer stays up for
    /// `policy.stabilization`, or `policy.max_attempts` is reached. Returns
    /// the full sequence of attempts.
    ///
    /// The restart-loop convergence story: an aged Tip in the danger zone
    /// (not yet past-stale) auto-closes on respawn, and the resulting closed
    /// batch is in the danger zone, so the detector exits with `DangerDetected`.
    /// Startup recovery's cascade fires at `MAX_WAIT_BLOCKS`, not at the
    /// danger threshold — so the loop only converges once enough *additional*
    /// L1 blocks have aged the batch past `MAX_WAIT_BLOCKS`. In production
    /// the orchestrator restart itself takes seconds, during which real L1
    /// blocks are produced; `advance_per_retry` simulates that drift. Tests
    /// that expect a short hiccup to self-heal (no danger involved) should
    /// leave `advance_per_retry` unset.
    ///
    /// The loop always returns Ok — assert on the final attempt's outcome
    /// to decide pass/fail in the test body.
    pub async fn respawn_until_stable(
        &mut self,
        policy: RespawnPolicy,
    ) -> HarnessResult<Vec<RespawnAttemptOutcome>> {
        let mut outcomes = Vec::with_capacity(policy.max_attempts as usize);
        for attempt in 0..policy.max_attempts {
            let outcome = self.respawn_and_watch(policy.stabilization).await?;
            let stable = outcome.is_stable();
            outcomes.push(outcome);
            if stable {
                break;
            }
            let is_last = attempt + 1 == policy.max_attempts;
            if let Some(advance) = policy.advance_per_retry.filter(|_| !is_last) {
                self.advance_wall_and_mine(advance).await?;
            }
        }
        Ok(outcomes)
    }

    /// Kill the sequencer process. Anvil stays running, so `mine_l1_blocks()` still works.
    pub async fn stop(&mut self) -> HarnessResult<()> {
        self.shutdown_child().await
    }

    /// Respawn the sequencer process using the same data directory and Anvil instance.
    ///
    /// Honors any `l1_endpoint_override` set via [`Self::set_l1_endpoint_override`]
    /// and the faketime offset in the rc file (see [`Self::set_faketime_offset`] /
    /// [`Self::advance_wall_and_mine`]).
    pub async fn respawn(&mut self) -> HarnessResult<()> {
        let SpawnedSequencerProcess {
            child,
            endpoint,
            log_path,
        } = spawn_sequencer_process(
            self.sequencer_bin.as_path(),
            self.log_prefix.as_str(),
            self.logs_dir.as_path(),
            self.data_dir_path.as_path(),
            &self.rollups,
            self.l1_endpoint_override.as_deref(),
            self.chain_id_override,
            self.libfaketime_path.as_deref(),
            self.faketime_rc_path.as_deref(),
            self.mine_l1_during_boot,
            self.recovery_setup.as_ref(),
            self.max_batch_open_seconds,
        )
        .await?;
        self.child = child;
        self.endpoint = endpoint;
        self.log_path = log_path;
        Ok(())
    }

    pub async fn restart(&mut self) -> HarnessResult<()> {
        self.stop().await?;
        self.respawn().await
    }

    /// Read the current sequencer log file contents.
    pub fn read_log_contents(&self) -> HarnessResult<String> {
        std::fs::read_to_string(&self.log_path).map_err(Into::into)
    }

    pub async fn ws(&self, from_offset: u64) -> HarnessResult<WsClient> {
        let client = self.sequencer_client()?;
        WsClient::connect(&client, from_offset).await
    }

    pub async fn wallet_l1(&self, signer: TestSigner) -> HarnessResult<WalletL1Client> {
        WalletL1Client::connect(
            self.l1_endpoint(),
            self.app_address(),
            self.erc20_portal_address(),
            self.supported_erc20_token(),
            signer,
        )
        .await
    }

    pub fn wallet_l2(&self, signer: TestSigner) -> HarnessResult<WalletL2Client> {
        WalletL2Client::new(
            self.endpoint(),
            self.domain_chain_id(),
            self.verifying_contract(),
            signer,
        )
    }

    pub async fn shutdown(mut self) -> HarnessResult<()> {
        let sequencer_result = self.shutdown_child().await;
        let rollups_result = self.rollups.shutdown().await;
        sequencer_result?;
        rollups_result
    }

    fn sequencer_client(&self) -> HarnessResult<SequencerClient> {
        SequencerClient::new_with_timeout(self.endpoint.clone(), Duration::from_secs(5))
            .map_err(|err| io_other(format!("failed to create sequencer client: {err}")).into())
    }

    async fn shutdown_child(&mut self) -> HarnessResult<()> {
        send_graceful_terminate(&mut self.child).await;
        match tokio::time::timeout(self.shutdown_timeout, self.child.wait()).await {
            Ok(wait_result) => {
                let _ = wait_result?;
                Ok(())
            }
            Err(_) => {
                self.child.start_kill()?;
                let _ = self.child.wait().await;
                Ok(())
            }
        }
    }
}

use alloy_primitives::Address;

struct SpawnedSequencerProcess {
    child: Child,
    endpoint: String,
    log_path: PathBuf,
}

/// Read `next_batch_nonce` out of a dump dir's `info.toml` (the resume nonce
/// `N`). The file is the simple `key = value` form the sequencer writes.
fn read_info_next_batch_nonce(dump_dir: &Path) -> HarnessResult<u64> {
    let info_path = dump_dir.join("info.toml");
    let content = fs::read_to_string(info_path.as_path())
        .map_err(|err| io_other(format!("read {info_path:?}: {err}")))?;
    for line in content.lines() {
        if let Some((key, value)) = line.split_once(" = ")
            && key.trim() == "next_batch_nonce"
        {
            return value
                .trim()
                .parse::<u64>()
                .map_err(|err| io_other(format!("parse next_batch_nonce: {err}")).into());
        }
    }
    Err(io_other(format!("info.toml missing next_batch_nonce: {info_path:?}")).into())
}

/// Recursively copy a directory tree (used to stash a checkpoint dump where
/// [`ManagedSequencer::reset_database`] won't wipe it).
fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn spawn_sequencer_process(
    sequencer_bin: &Path,
    log_prefix: &str,
    logs_dir: &Path,
    data_dir: &Path,
    rollups: &DevnetRollupsStack,
    l1_endpoint_override: Option<&str>,
    chain_id_override: Option<u64>,
    libfaketime_path: Option<&Path>,
    faketime_rc_path: Option<&Path>,
    mine_l1_during_boot: bool,
    recovery: Option<&RecoverySetupParams>,
    max_batch_open_seconds: Option<u64>,
) -> HarnessResult<SpawnedSequencerProcess> {
    let (endpoint, http_addr) = build_local_endpoint()?;
    let log_path = timestamped_log_path(logs_dir, log_prefix);

    // Dedicated batch submitter = anvil account 9 (NOT the deployer/funder,
    // account 0). `setup`'s detection gate refuses when the
    // submitter's wallet nonce is unsettled; the deployer's deploy-tx tail is
    // not safe at setup time, so reusing it false-positives. Account 9 starts
    // at nonce 0. Must match `DEVNET_SEQUENCER_ADDRESS` (app-core); a debug
    // assert below pins that. Account 9 is the highest standard funded anvil
    // account, so it stays clear of the e2e wallets (0–3) and the bench
    // workload (indices 0..concurrency).
    const DEVNET_SUBMITTER_ACCOUNT_INDEX: usize = 9;
    let batch_submitter_key = default_private_keys()
        .get(DEVNET_SUBMITTER_ACCOUNT_INDEX)
        .cloned()
        .unwrap_or_else(|| {
            "0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6".to_string()
        });
    // `setup` takes the submitter *address* (it never signs); `run` takes the
    // key. Derive the address from the key so the harness configures both
    // consistently.
    let submitter_address = TestSigner::from_private_key_hex(&batch_submitter_key)?.address();
    // `assert_eq!`, not `debug_assert_eq!`: benches build the harness `--release`,
    // where a debug-assert is a no-op — drift between app-core and the harness
    // submitter must fail fast in every build. Cost is one comparison per spawn.
    assert_eq!(
        submitter_address,
        app_core::application::DEVNET_SEQUENCER_ADDRESS,
        "harness submitter (anvil account {DEVNET_SUBMITTER_ACCOUNT_INDEX}) must equal \
         DEVNET_SEQUENCER_ADDRESS — keep app-core and the harness in sync"
    );
    let batch_submitter_address = submitter_address.to_string();
    let eth_rpc_url = l1_endpoint_override.unwrap_or_else(|| rollups.l1_endpoint());

    let chain_id = chain_id_override.unwrap_or(DEVNET_CHAIN_ID);
    let bin = path_as_str(sequencer_bin)?.to_owned();

    // libfaketime is applied via env vars (not the `faketime` wrapper binary),
    // which the file-based FAKETIME_TIMESTAMP_FILE mechanism reads on every
    // time call (FAKETIME_NO_CACHE=1) so tests can shift the clock at runtime.
    let open_log = |append: bool| -> HarnessResult<std::fs::File> {
        Ok(OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(!append)
            .append(append)
            .open(log_path.as_path())?)
    };

    // Phase A — `setup` (run once; idempotent on re-spawns, so it is a cheap
    // no-op once the DB is set up, and needs L1 only on the first run / after
    // a DB reset). Run to completion as a blocking step; a bootstrap refusal
    // (e.g. a chain-id mismatch) surfaces here as a non-zero exit, just as it
    // surfaced from the monolithic boot before the split.
    let mut setup_cmd = Command::new(&bin);
    if let (Some(lib), Some(rc)) = (libfaketime_path, faketime_rc_path) {
        apply_faketime_env(&mut setup_cmd, lib, rc)?;
    }
    let setup_log = open_log(false)?;
    let setup_log_err = setup_log.try_clone()?;
    setup_cmd
        .arg("setup")
        .arg("--data-dir")
        .arg(path_as_str(data_dir)?)
        .arg("--eth-rpc-url")
        .arg(eth_rpc_url)
        .arg("--chain-id")
        .arg(chain_id.to_string())
        .arg("--app-address")
        .arg(rollups.app_address().to_string())
        .arg("--batch-submitter-address")
        .arg(&batch_submitter_address);
    if let Some(recovery) = recovery {
        // Recovery setup rebuilds the wiped DB from a checkpoint: it signs (the
        // flush), so it takes the key, plus the checkpoint block + dump dir.
        setup_cmd
            .arg("--recovery")
            .arg("--checkpoint-block")
            .arg(recovery.checkpoint_block.to_string())
            .arg("--checkpoint-dump-dir")
            .arg(path_as_str(&recovery.checkpoint_dump_dir)?)
            .arg("--batch-submitter-private-key")
            .arg(&batch_submitter_key);
    }
    setup_cmd
        .env("RUST_LOG", DEFAULT_SEQUENCER_RUST_LOG)
        .stdout(Stdio::from(setup_log))
        .stderr(Stdio::from(setup_log_err));
    let mut setup_child = setup_cmd
        .spawn()
        .map_err(|err| io_other(format!("failed to spawn `setup` for '{bin}': {err}")))?;
    // Bound the setup phase so a stalled/lagging RPC during the initial sync
    // can't hang the harness indefinitely. Recovery setup additionally flushes
    // the wallet nonce, which can block until a stranded slot becomes safe — so
    // mine L1 concurrently (nothing else does on an on-demand devnet) and give
    // it the generous recovery-boot budget. Genesis setup never flushes.
    let setup_status = if recovery.is_some() {
        let deadline = std::time::Instant::now() + RECOVERY_BOOT_START_TIMEOUT;
        loop {
            if let Some(status) = setup_child
                .try_wait()
                .map_err(|err| io_other(format!("poll recovery `setup`: {err}")))?
            {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = setup_child.start_kill();
                let _ = setup_child.wait().await;
                return Err(io_other(format!(
                    "recovery `setup` timed out after {:?} (see {})",
                    RECOVERY_BOOT_START_TIMEOUT,
                    log_path.display()
                ))
                .into());
            }
            let _ = rollups.mine_l1_blocks(1).await;
            tokio::time::sleep(BOOT_L1_MINE_INTERVAL).await;
        }
    } else {
        match tokio::time::timeout(DEFAULT_SEQUENCER_START_TIMEOUT, setup_child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = setup_child.start_kill();
                let _ = setup_child.wait().await;
                return Err(io_other(format!(
                    "sequencer `setup` timed out after {:?} (see {})",
                    DEFAULT_SEQUENCER_START_TIMEOUT,
                    log_path.display()
                ))
                .into());
            }
        }
    };
    if !setup_status.success() {
        return Err(io_other(format!(
            "sequencer `setup` failed: status={setup_status} (see {})",
            log_path.display()
        ))
        .into());
    }

    // Phase B — `run` (re-spawned on every restart; reads identity from the
    // setup DB).
    let mut run_cmd = Command::new(&bin);
    if let (Some(lib), Some(rc)) = (libfaketime_path, faketime_rc_path) {
        apply_faketime_env(&mut run_cmd, lib, rc)?;
    }
    let run_log = open_log(true)?;
    let run_log_err = run_log.try_clone()?;
    // `kill_on_drop`: a `ManagedSequencer` normally reaps its child via
    // `shutdown()` / `stop()`, but an error path that drops the struct without
    // either (e.g. a benchmark returning early on a misconfigured run) would
    // otherwise leave the child running while the owning `TempDir` deletes the
    // data dir — the child's next DB open then fails `ENOENT` and it exits with
    // a confusing `unable to open database file` that masks the real failure.
    // `child` is declared before `_data_dir`, so on drop the SIGKILL lands
    // before the dir is removed.
    run_cmd
        .kill_on_drop(true)
        .arg("run")
        .arg("--http-addr")
        .arg(http_addr)
        .arg("--data-dir")
        .arg(path_as_str(data_dir)?)
        .arg("--eth-rpc-url")
        .arg(eth_rpc_url)
        .arg("--batch-submitter-private-key")
        .arg(&batch_submitter_key);
    if let Some(secs) = max_batch_open_seconds {
        run_cmd
            .arg("--max-batch-open-seconds")
            .arg(secs.to_string());
    }
    run_cmd
        .env("RUST_LOG", DEFAULT_SEQUENCER_RUST_LOG)
        .stdout(Stdio::from(run_log))
        .stderr(Stdio::from(run_log_err));
    let mut child = run_cmd.spawn().map_err(|err| {
        io_other(format!(
            "failed to spawn sequencer binary '{}': {err}",
            sequencer_bin.display()
        ))
    })?;

    if mine_l1_during_boot {
        // A recovery boot blocks in the WP2 mempool flush until the stranded
        // wallet-nonce slot becomes safe, which needs L1 to keep producing
        // blocks. On an on-demand-mining devnet nothing produces them, so mine
        // a trickle concurrently with the readiness wait — mirroring a live
        // chain that advances during a slow boot. The miner is dropped the
        // instant readiness resolves, so L1 only advances for the boot window.
        let miner = async {
            loop {
                tokio::time::sleep(BOOT_L1_MINE_INTERVAL).await;
                // Transient RPC hiccups mid-boot are non-fatal — the next tick
                // retries. A genuine mining failure manifests as the boot
                // timing out, reported by the readiness arm.
                let _ = rollups.mine_l1_blocks(1).await;
            }
        };
        tokio::select! {
            res = wait_for_http_readiness(
                endpoint.as_str(),
                &mut child,
                RECOVERY_BOOT_START_TIMEOUT,
            ) => res?,
            _ = miner => unreachable!("boot miner loops forever; only readiness resolves"),
        }
    } else {
        wait_for_http_readiness(
            endpoint.as_str(),
            &mut child,
            DEFAULT_SEQUENCER_START_TIMEOUT,
        )
        .await?;
    }

    Ok(SpawnedSequencerProcess {
        child,
        endpoint,
        log_path,
    })
}

/// Configure the child process env to preload libfaketime and point it at
/// the rc file for dynamic offsets. macOS uses `DYLD_INSERT_LIBRARIES` +
/// `DYLD_FORCE_FLAT_NAMESPACE=1`; Linux uses `LD_PRELOAD`.
fn apply_faketime_env(
    cmd: &mut Command,
    libfaketime_path: &Path,
    faketime_rc_path: &Path,
) -> HarnessResult<()> {
    let lib = path_as_str(libfaketime_path)?;
    let rc = path_as_str(faketime_rc_path)?;
    if cfg!(target_os = "macos") {
        cmd.env("DYLD_INSERT_LIBRARIES", lib)
            .env("DYLD_FORCE_FLAT_NAMESPACE", "1");
    } else {
        cmd.env("LD_PRELOAD", lib);
    }
    cmd.env("FAKETIME_TIMESTAMP_FILE", rc)
        .env("FAKETIME_NO_CACHE", "1");
    Ok(())
}

/// Locate the libfaketime shared library. Searches:
///   1. `$LIBFAKETIME_LIB` (explicit override).
///   2. `lib/faketime/libfaketime.{1.dylib,so.1}` relative to the `faketime`
///      binary's prefix (Nix layout).
///   3. Linux distro multiarch lib dirs such as
///      `/usr/lib/x86_64-linux-gnu/faketime` (Debian/Ubuntu apt layout).
fn find_libfaketime() -> HarnessResult<PathBuf> {
    if let Ok(p) = std::env::var("LIBFAKETIME_LIB") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(io_other(format!("LIBFAKETIME_LIB={p:?} does not exist")).into());
    }

    let path =
        std::env::var("PATH").map_err(|err| io_other(format!("PATH env var unreadable: {err}")))?;
    let faketime_bin = std::env::split_paths(&path)
        .map(|p| p.join("faketime"))
        .find(|p| p.exists())
        .ok_or_else(|| {
            io_other("`faketime` binary not found in PATH; add libfaketime to the dev shell")
        })?;

    let prefix = faketime_bin
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| {
            io_other(format!(
                "faketime path has no grandparent: {faketime_bin:?}"
            ))
        })?;
    let lib_dirs = candidate_libfaketime_dirs(prefix);
    let candidates = libfaketime_file_names();
    if let Some(path) = find_libfaketime_in_dirs(lib_dirs.as_slice(), candidates) {
        return Ok(path);
    }

    let searched = lib_dirs
        .iter()
        .map(|p| format!("{p:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(io_other(format!(
        "libfaketime not found under any searched directory [{searched}] (tried {candidates:?})"
    ))
    .into())
}

fn libfaketime_file_names() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["libfaketime.1.dylib", "libfaketime.dylib"]
    } else {
        &["libfaketime.so.1", "libfaketime.so"]
    }
}

fn candidate_libfaketime_dirs(prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let lib_dir = prefix.join("lib");
    dirs.push(lib_dir.join("faketime"));

    if cfg!(target_os = "linux") {
        if let Ok(entries) = fs::read_dir(&lib_dir) {
            let mut multiarch_dirs = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .filter(|path| path.file_name().is_some_and(|name| name != "faketime"))
                .map(|path| path.join("faketime"))
                .collect::<Vec<_>>();
            multiarch_dirs.sort();
            dirs.extend(multiarch_dirs);
        }
        dirs.push(prefix.join("lib64").join("faketime"));
    }

    dirs.dedup();
    dirs
}

fn find_libfaketime_in_dirs(lib_dirs: &[PathBuf], candidates: &[&str]) -> Option<PathBuf> {
    for lib_dir in lib_dirs {
        for name in candidates {
            let path = lib_dir.join(name);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{candidate_libfaketime_dirs, find_libfaketime_in_dirs};

    #[cfg(target_os = "linux")]
    #[test]
    fn libfaketime_lookup_finds_debian_multiarch_layout() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let prefix = temp.path();
        let multiarch_dir = prefix.join("lib").join("x86_64-linux-gnu").join("faketime");
        fs::create_dir_all(&multiarch_dir).expect("create multiarch faketime dir");
        let expected = multiarch_dir.join("libfaketime.so.1");
        fs::write(&expected, b"fake so").expect("write fake lib");

        let dirs = candidate_libfaketime_dirs(prefix);
        let found = find_libfaketime_in_dirs(dirs.as_slice(), &["libfaketime.so.1"])
            .expect("multiarch lib should be discovered");

        assert_eq!(found, expected);
    }

    #[test]
    fn libfaketime_lookup_prefers_direct_prefix_layout() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let prefix = temp.path();
        let direct_dir = prefix.join("lib").join("faketime");
        let multiarch_dir = prefix.join("lib").join("x86_64-linux-gnu").join("faketime");
        fs::create_dir_all(&direct_dir).expect("create direct faketime dir");
        fs::create_dir_all(&multiarch_dir).expect("create multiarch faketime dir");
        let expected = direct_dir.join("libfaketime.so.1");
        let fallback = multiarch_dir.join("libfaketime.so.1");
        fs::write(&expected, b"direct").expect("write direct lib");
        fs::write(&fallback, b"fallback").expect("write fallback lib");

        let dirs = candidate_libfaketime_dirs(prefix);
        let found = find_libfaketime_in_dirs(dirs.as_slice(), &["libfaketime.so.1"])
            .expect("direct lib should be discovered");

        assert_eq!(found, expected);
    }
}
