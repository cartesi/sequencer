// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::fs::{self, OpenOptions};
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
const DEFAULT_SEQUENCER_RUST_LOG: &str = "info";
pub const DEFAULT_DEVNET_SEQUENCER_BIN: &str = "target/debug/sequencer-devnet";
pub const DEFAULT_TEST_LOGS_DIR: &str = "tests/e2e/results";

#[derive(Debug, Clone)]
pub struct ManagedSequencerConfig {
    pub sequencer_bin: PathBuf,
    pub log_prefix: String,
    pub logs_dir: PathBuf,
}

/// Snapshot of the `batches` table. Returned by
/// [`ManagedSequencer::count_batches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchCounts {
    pub total: u64,
    pub sealed: u64,
    pub invalidated: u64,
}

/// Outcome of a single [`ManagedSequencer::respawn_and_watch`] attempt.
#[derive(Debug)]
pub enum RespawnAttemptOutcome {
    /// The child came up and stayed alive for the requested stabilization
    /// window.
    Stable,
    /// `respawn()` itself returned `Err` — the child exited during bootstrap
    /// before HTTP became ready. Typically surfaces
    /// `RecoveryError::StartupDangerZoneEstimate` from the startup
    /// fallback.
    RespawnFailed(String),
    /// `respawn()` returned `Ok` but the child exited within the
    /// stabilization window. Typically surfaces
    /// `BatchSubmitterError::DangerZone` from the submitter's first post-boot
    /// tick.
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
    /// (§8.2.1 / §8.3.1).
    chain_id_override: Option<u64>,
    /// Path to the file libfaketime re-reads for its offset, on every time
    /// call (combined with `FAKETIME_NO_CACHE=1`). Writing to this file
    /// shifts the sequencer's view of `SystemTime::now()` / `Instant::now()`
    /// immediately — no respawn needed.
    faketime_rc_path: PathBuf,
    /// Cached libfaketime dylib/so path (computed once on spawn).
    libfaketime_path: PathBuf,
    /// Internal cumulative forward-offset tracker for
    /// [`Self::advance_wall_and_mine`]. Not touched by
    /// [`Self::set_faketime_offset`].
    cumulative_offset_secs: u64,
}

pub fn default_devnet_sequencer_config(log_prefix: impl Into<String>) -> ManagedSequencerConfig {
    ManagedSequencerConfig {
        sequencer_bin: PathBuf::from(DEFAULT_DEVNET_SEQUENCER_BIN),
        log_prefix: log_prefix.into(),
        logs_dir: PathBuf::from(DEFAULT_TEST_LOGS_DIR),
    }
}

impl ManagedSequencer {
    pub async fn spawn(config: ManagedSequencerConfig) -> HarnessResult<Self> {
        let logs_dir = paths::resolve_from_workspace(&config.logs_dir);
        let sequencer_bin = paths::resolve_from_workspace(&config.sequencer_bin);
        let log_prefix = config.log_prefix;
        let rollups = DevnetRollupsStack::spawn(log_prefix.as_str(), logs_dir.as_path()).await?;

        fs::create_dir_all(logs_dir.as_path())?;
        let data_dir = TempDir::new()
            .map_err(|err| io_other(format!("failed to create temp data dir: {err}")))?;
        let data_dir_path = data_dir.path().to_path_buf();

        // Set up faketime: locate libfaketime + create the rc file. Initial
        // content `+0` means no offset; tests can overwrite with a new offset
        // at any time and the running sequencer will see it on its next
        // `SystemTime::now()` / `Instant::now()` call (FAKETIME_NO_CACHE=1).
        let libfaketime_path = find_libfaketime()?;
        let faketime_rc_path = data_dir_path.join("faketime.rc");
        fs::write(faketime_rc_path.as_path(), "+0\n")
            .map_err(|err| io_other(format!("create faketime rc file: {err}")))?;

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
            libfaketime_path.as_path(),
            faketime_rc_path.as_path(),
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

    /// Override the `--chain-id` argument the sequencer is spawned with on
    /// the next [`Self::respawn`]. When `None`, defaults to the devnet
    /// chain id (matches Anvil).
    ///
    /// Used by §8.2.1 / §8.3.1 to inject a mismatched chain id and assert
    /// that bootstrap returns `RunError::ChainIdMismatch` instead of
    /// silently writing a wrong-chain bootstrap cache. Does not affect
    /// the currently-running sequencer process.
    pub fn set_chain_id_override(&mut self, chain_id: Option<u64>) {
        self.chain_id_override = chain_id;
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
        let s = offset.as_deref().unwrap_or("+0");
        fs::write(self.faketime_rc_path.as_path(), format!("{s}\n"))
            .map_err(|err| io_other(format!("write faketime rc file: {err}")))?;
        self.cumulative_offset_secs = 0;
        Ok(())
    }

    /// Delete the row in `l1_bootstrap_cache`, simulating a DB that has
    /// never successfully completed bootstrap discovery (no cached
    /// `input_box_address` / `genesis_block` / `chain_id`). Call while the
    /// sequencer is stopped.
    ///
    /// Used by §8.1.2: with no cache and L1 unreachable, the bootstrap
    /// path returns the "L1 required for first startup" error before any
    /// recovery logic can run.
    pub fn clear_l1_bootstrap_cache(&self) -> HarnessResult<()> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open(db_path.as_path())
            .map_err(|err| io_other(format!("open DB: {err}")))?;
        conn.execute("DELETE FROM l1_bootstrap_cache", [])
            .map_err(|err| io_other(format!("clear l1_bootstrap_cache: {err}")))?;
        Ok(())
    }

    /// Rewrite `l1_safe_head.synced_at_ms` to `0`, simulating a DB that has
    /// never successfully synced from L1. Call while the sequencer is
    /// stopped.
    ///
    /// Used by §7.8.2: the wall-clock fallback treats `synced_at_ms == 0`
    /// as "first boot, L1 required" and refuses to proceed if L1 is
    /// unreachable. Setting this field while the bootstrap cache is
    /// populated lets us hit that branch without losing the cached chain
    /// ID / InputBox address (which would fail earlier in bootstrap, not
    /// in the wall-clock fallback).
    pub fn reset_l1_safe_head_synced_at_ms(&self) -> HarnessResult<()> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open(db_path.as_path())
            .map_err(|err| io_other(format!("open DB: {err}")))?;
        conn.execute(
            "UPDATE l1_safe_head SET synced_at_ms = 0 WHERE singleton_id = 0",
            [],
        )
        .map_err(|err| io_other(format!("reset synced_at_ms: {err}")))?;
        Ok(())
    }

    /// Read-only snapshot of the `safe_accepted_batches` view: rows
    /// recovered from the L1-side scheduler frontier (i.e., batches the
    /// sequencer has *observed accepted on chain*). Returns `(count,
    /// min_nonce)` — count is the row count, min_nonce is `MIN(nonce)` or
    /// `None` if empty.
    ///
    /// Used by §7.5.2 to confirm a recovery batch (which reuses nonce 0)
    /// actually lands and gets accepted on L1 — proving the
    /// `populate_safe_accepted_batches_inner` cursor handles
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
    /// See `tests/TEST_PLAN.md` §12.5.3 for the design rationale — this is
    /// a harness-only check (no sequencer changes) that catches regressions
    /// which slip past user-visible e2e assertions.
    pub fn assert_schema_invariants(&self) -> HarnessResult<()> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;

        // 1. At most one valid open batch.
        let open_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM valid_open_batch", [], |row| {
                row.get(0)
            })
            .map_err(|err| io_other(format!("count valid_open_batch: {err}")))?;
        if open_count > 1 {
            panic!("schema invariant: more than one valid Tip ({open_count} rows)");
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
                    if *nonce != 0 {
                        panic!(
                            "schema invariant: batch {bi} has NULL parent but nonce {nonce} (expected 0)"
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
            if n != i as i64 {
                panic!("schema invariant: valid nonces not contiguous: {valid_nonces:?}");
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
    /// Assumes `SEQ_SECONDS_PER_BLOCK = 12`. If a test changes that via env,
    /// this helper's block count will be wrong — prefer the direct dials in
    /// that case.
    pub async fn advance_wall_and_mine(&mut self, duration: Duration) -> HarnessResult<()> {
        const SECONDS_PER_BLOCK: u64 = 12;
        let secs = duration.as_secs();
        let blocks = secs / SECONDS_PER_BLOCK;
        self.mine_l1_blocks(blocks).await?;
        self.cumulative_offset_secs = self.cumulative_offset_secs.saturating_add(secs);
        fs::write(
            self.faketime_rc_path.as_path(),
            format!("+{}s\n", self.cumulative_offset_secs),
        )
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
    ///     `RecoveryError::StartupDangerZoneEstimate` from the startup
    ///     fallback when L1 is unreachable.
    ///   - The child comes up (HTTP ready, bootstrap passed), then one of
    ///     the internal tasks returns a fatal error and the process exits.
    ///     Canonical cause: `BatchSubmitterError::DangerZone` when the first
    ///     submitter tick after boot sees a closed batch past
    ///     `danger_threshold`.
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
    /// batch is in the danger zone, so the submitter exits with `DangerZone`.
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
            self.libfaketime_path.as_path(),
            self.faketime_rc_path.as_path(),
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

#[allow(clippy::too_many_arguments)]
async fn spawn_sequencer_process(
    sequencer_bin: &Path,
    log_prefix: &str,
    logs_dir: &Path,
    data_dir: &Path,
    rollups: &DevnetRollupsStack,
    l1_endpoint_override: Option<&str>,
    chain_id_override: Option<u64>,
    libfaketime_path: &Path,
    faketime_rc_path: &Path,
) -> HarnessResult<SpawnedSequencerProcess> {
    let (endpoint, http_addr) = build_local_endpoint()?;
    let log_path = timestamped_log_path(logs_dir, log_prefix);
    let stdout_log = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(log_path.as_path())?;
    let stderr_log = stdout_log.try_clone()?;

    let batch_submitter_key = default_private_keys().first().cloned().unwrap_or_else(|| {
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
    });
    let eth_rpc_url = l1_endpoint_override.unwrap_or_else(|| rollups.l1_endpoint());

    // Set up libfaketime via env vars (not the `faketime` wrapper binary).
    // The wrapper sets the FAKETIME env var, which has priority over
    // FAKETIME_TIMESTAMP_FILE — bypassing it lets the file-based mechanism
    // work. The file's contents are re-read on every `SystemTime::now()` /
    // `Instant::now()` call thanks to FAKETIME_NO_CACHE=1, so tests can
    // shift the clock dynamically during a run.
    let mut cmd = Command::new(path_as_str(sequencer_bin)?);
    apply_faketime_env(&mut cmd, libfaketime_path, faketime_rc_path)?;

    let chain_id = chain_id_override.unwrap_or(DEVNET_CHAIN_ID);
    let mut child = cmd
        .arg("--http-addr")
        .arg(http_addr)
        .arg("--data-dir")
        .arg(path_as_str(data_dir)?)
        .arg("--eth-rpc-url")
        .arg(eth_rpc_url)
        .arg("--chain-id")
        .arg(chain_id.to_string())
        .arg("--app-address")
        .arg(rollups.app_address().to_string())
        .arg("--batch-submitter-private-key")
        .arg(&batch_submitter_key)
        .env("RUST_LOG", DEFAULT_SEQUENCER_RUST_LOG)
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .map_err(|err| {
            io_other(format!(
                "failed to spawn sequencer binary '{}': {err}",
                sequencer_bin.display()
            ))
        })?;

    wait_for_http_readiness(
        endpoint.as_str(),
        &mut child,
        DEFAULT_SEQUENCER_START_TIMEOUT,
    )
    .await?;

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
    let lib_dir = prefix.join("lib").join("faketime");
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["libfaketime.1.dylib", "libfaketime.dylib"]
    } else {
        &["libfaketime.so.1", "libfaketime.so"]
    };
    for name in candidates {
        let p = lib_dir.join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(io_other(format!(
        "libfaketime not found under {lib_dir:?} (tried {candidates:?})"
    ))
    .into())
}
