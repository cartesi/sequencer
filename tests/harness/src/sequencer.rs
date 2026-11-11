// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy_primitives::{B256, U256};
use alloy_sol_types::SolEvent;
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;
use sequencer_rust_client::SequencerClient;
use tempfile::TempDir;
use tokio::process::{Child, Command};

use crate::HarnessResult;
use crate::default_private_keys;
use crate::paths;
use crate::rollups::{DEVNET_CHAIN_ID, DEVNET_SECONDS_PER_BLOCK, DevnetRollupsStack};
use crate::util::{
    Readiness, build_local_endpoint, io_other, path_as_str, send_graceful_terminate,
    timestamped_log_path, wait_for_http_readiness,
};
use crate::wallet::{TestSigner, WalletL1Client, WalletL2Client};
use crate::ws::WsClient;

/// A path under the data directory, with a file's length and keccak256, or
/// `None` for a directory.
pub type DataDirEntry = (String, Option<(u64, B256)>);

const DEFAULT_SEQUENCER_START_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// Cadence at which the harness mines an L1 block while a setup runs when
/// [`ManagedSequencer::set_mine_l1_during_setup`] is on. One block per
/// wall-clock second keeps the live-chain timestamp coupled while still
/// advancing Anvil's `safe` tag well within [`FLUSHING_SETUP_TIMEOUT`].
const SETUP_L1_MINE_INTERVAL: Duration = Duration::from_secs(1);
/// Budget for a setup that flushes stranded transactions: the flush polls
/// `get_transaction_count(Safe)` once per L1 block time (12 s here), so it
/// needs several poll cycles of slack.
const FLUSHING_SETUP_TIMEOUT: Duration = Duration::from_secs(45);
/// Budget for a setup from genesis: it syncs the chain and rebuilds the
/// history from its start, which on an aged devnet is hundreds of rows.
const GENESIS_SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Budget for a setup from a stored machine: it loads the machine into the
/// emulator and answers two inspect queries before it rebuilds the history.
const BASE_SETUP_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_SEQUENCER_RUST_LOG: &str = "info";
/// The data directory's name under the harness's root directory; each
/// [`ManagedSequencer::discard_data_dir`] moves on to the next numbered one.
const DATA_DIR_NAME: &str = "data";
/// Anvil account the batch submitter signs with (NOT the deployer/funder,
/// account 0). Setup's escape check refuses while the submitter's wallet is
/// unsettled, so the submitter must not share a key with the deployer or with
/// an e2e wallet whose transactions are in flight. Must match
/// `DEVNET_SEQUENCER_ADDRESS` (app-core), which every spawn asserts. Account 9
/// is the highest standard funded account, clear of the e2e wallets (0-3) and
/// the bench workload (indices 0..concurrency).
const DEVNET_SUBMITTER_ACCOUNT_INDEX: usize = 9;
/// The exit code of a command that must not be restarted (README "Running"):
/// among others, every start of `run` while a terminal fact holds.
pub const EXIT_TERMINAL: i32 = 30;
/// The signal a running sequencer raises on a terminal runtime fault, a
/// terminal fact included: it aborts instead of draining.
const SIGABRT: i32 = 6;
pub const DEFAULT_DEVNET_SEQUENCER_BIN: &str = "target/debug/wallet-sequencer-devnet";
pub const DEFAULT_TEST_LOGS_DIR: &str = "tests/e2e/results";

#[derive(Debug, Clone)]
pub struct ManagedSequencerConfig {
    pub sequencer_bin: PathBuf,
    /// Optional genesis tool invoked as `<binary> <absent-prefix> devnet` for
    /// each setup without a base, whose `--state-file` it supplies. Its output
    /// is deleted once setup ends, so `run` never depends on it.
    pub genesis_bin: Option<PathBuf>,
    pub log_prefix: String,
    pub logs_dir: PathBuf,
    /// When false, the child runs without libfaketime (for tests that never
    /// manipulate wall clock). Requires libfaketime in PATH when true.
    pub faketime: bool,
    /// Fixed fee-oracle exponent pinned by the `setup` phase for Anvil/devnet.
    pub fee_oracle_fixed_log_gas_price: Option<i16>,
}

/// Snapshot of the `batches` table. Returned by
/// [`ManagedSequencer::count_batches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchCounts {
    pub total: u64,
    pub sealed: u64,
}

/// Highest batch nonce sealed locally and highest accepted on safe L1.
/// Returned by [`ManagedSequencer::batch_nonce_frontier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchNonceFrontier {
    /// Highest nonce among sealed batches.
    pub sealed: Option<u64>,
    /// Highest accepted batch nonce (`batches.accepted_input` set).
    pub safe_accepted: Option<u64>,
}

/// The era a setup published, read from the `era` row. Returned by
/// [`ManagedSequencer::published_era`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedEra {
    /// The base's L2 tx count, where the era's history starts.
    pub base_count: u64,
    /// The stop block: where the rebuilt history ends.
    pub stop_block: u64,
    /// The L2 tx count at the end of the stop block.
    pub rebuilt_history_end: u64,
    /// InputBox index of the oldest direct still queued at the stop block,
    /// or the input count there when none is queued.
    pub first_queued_input: u64,
    /// The scheduler's next batch nonce at the stop block (the anchor).
    pub next_batch_nonce: u64,
}

/// The rows setup's rebuild stored, and how many carry a count. Only an
/// executed transaction is stored, with its count, so the two agree.
/// Returned by [`ManagedSequencer::rebuilt_history_rows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuiltHistoryRows {
    pub total: u64,
    pub executed: u64,
}

/// How a `setup` child ended. Setup prints its summary on stdout; the log
/// file gets stdout and stderr both.
#[derive(Debug)]
pub struct SetupOutcome {
    pub status: ExitStatus,
    pub stdout: String,
}

impl SetupOutcome {
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }

    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// How a start of `run` ended: it reached readiness, or the process exited
/// first. Returned by [`ManagedSequencer::try_respawn`].
#[derive(Debug)]
pub enum RunStart {
    Ready,
    Exited(ExitStatus),
}

/// Whether a running sequencer stopped on a terminal runtime fault: it
/// aborts (SIGABRT) instead of draining.
pub fn is_terminal_abort(status: &ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(SIGABRT)
}

/// The landing reading `GET /healthz` serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct LandingHealth {
    /// `H + 1 - s` for the oldest unaccepted batch: its age at the earliest
    /// block it can still land in, void at `max_wait_blocks`. `None` when
    /// every batch is accepted or before the first reading.
    pub landing_lag_blocks: Option<u64>,
    pub max_wait_blocks: u64,
    /// Wall clock minus the safe block's L1 timestamp; `None` before the
    /// first reading.
    pub safe_head_age_seconds: Option<u64>,
}

/// The oldest batch L1 has not accepted, closed or open. Returned by
/// [`ManagedSequencer::oldest_unaccepted_batch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnacceptedBatch {
    pub nonce: u64,
    /// Its first frame's safe block: the batch is stale on chain once it
    /// lands this many blocks plus the window later.
    pub first_frame_safe_block: u64,
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
    genesis_bin: Option<PathBuf>,
    log_prefix: String,
    logs_dir: PathBuf,
    /// Holds the data directories and the faketime rc file. The data
    /// directory is a child of it, never the root itself, because setup
    /// refuses a directory that holds anything.
    _root_dir: TempDir,
    data_dir_path: PathBuf,
    /// Data directories discarded so far; names the next one.
    discarded_data_dirs: u32,
    /// When `true`, the next [`Self::respawn`] runs `setup` before `run`: the
    /// data directory is new. Cleared once a setup succeeds there.
    setup_pending: bool,
    /// The `--base` machine directory setup starts from; `None` is genesis.
    /// See [`Self::set_setup_base`].
    setup_base: Option<PathBuf>,
    endpoint: String,
    internal_endpoint: String,
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
    /// The cumulative forward offset [`Self::advance_wall_and_mine`] wrote.
    cumulative_offset_secs: u64,
    /// When `true`, every setup mines L1 blocks while it runs so its flush
    /// can make progress. See [`Self::set_mine_l1_during_setup`].
    mine_l1_during_setup: bool,
    /// The submitter's balance while [`Self::hold_batch_submissions`] holds
    /// it at zero; restored by [`Self::release_batch_submissions`].
    held_submitter_balance: Option<U256>,
    /// When `Some(secs)`, passes `--max-batch-open-seconds` to the `run` phase
    /// so the inclusion lane seals the open batch promptly (the default is 2h).
    /// Used by tests that need a batch to close + be promoted. Persists across
    /// respawns. See [`Self::set_max_batch_open_seconds`].
    max_batch_open_seconds: Option<u64>,
    /// Fixed fee-oracle exponent forwarded to each `setup` spawn. See
    /// [`ManagedSequencerConfig::fee_oracle_fixed_log_gas_price`].
    fee_oracle_fixed_log_gas_price: Option<i16>,
}

pub fn default_devnet_sequencer_config(log_prefix: impl Into<String>) -> ManagedSequencerConfig {
    ManagedSequencerConfig {
        sequencer_bin: paths::resolve_devnet_sequencer_bin(),
        genesis_bin: None,
        log_prefix: log_prefix.into(),
        logs_dir: PathBuf::from(DEFAULT_TEST_LOGS_DIR),
        faketime: true,
        fee_oracle_fixed_log_gas_price: None,
    }
}

pub fn default_c_wallet_sequencer_config(log_prefix: impl Into<String>) -> ManagedSequencerConfig {
    ManagedSequencerConfig {
        sequencer_bin: paths::resolve_c_wallet_sequencer_bin(),
        genesis_bin: Some(paths::resolve_c_wallet_genesis_bin()),
        ..default_devnet_sequencer_config(log_prefix)
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
        let genesis_bin = config
            .genesis_bin
            .as_ref()
            .map(paths::resolve_from_workspace);
        let log_prefix = config.log_prefix;
        let rollups = DevnetRollupsStack::spawn(log_prefix.as_str(), logs_dir.as_path()).await?;

        fs::create_dir_all(logs_dir.as_path())?;
        let root_dir = TempDir::new()
            .map_err(|err| io_other(format!("failed to create temp data dir: {err}")))?;
        // Setup creates the data directory itself.
        let data_dir_path = root_dir.path().join(DATA_DIR_NAME);

        let (libfaketime_path, faketime_rc_path) = if config.faketime {
            let libfaketime_path = find_libfaketime()?;
            let faketime_rc_path = root_dir.path().join("faketime.rc");
            fs::write(faketime_rc_path.as_path(), "+0\n")
                .map_err(|err| io_other(format!("create faketime rc file: {err}")))?;
            (Some(libfaketime_path), Some(faketime_rc_path))
        } else {
            (None, None)
        };

        let child_env = ChildEnv {
            bin: sequencer_bin.as_path(),
            data_dir: data_dir_path.as_path(),
            rollups: &rollups,
            eth_rpc_url: rollups.l1_endpoint(),
            faketime: libfaketime_path.as_deref().zip(faketime_rc_path.as_deref()),
        };
        let log_path = timestamped_log_path(logs_dir.as_path(), log_prefix.as_str());
        let setup = SetupRequest {
            chain_id: DEVNET_CHAIN_ID,
            base: None,
            dry_run: false,
            genesis_bin: genesis_bin.as_deref(),
            fee_oracle_fixed_log_gas_price: config.fee_oracle_fixed_log_gas_price,
        };
        // A first setup from genesis finds a settled wallet, so its flush
        // never waits on L1.
        require_setup_success(
            run_setup_process(&child_env, &setup, &log_path, false).await?,
            &log_path,
        )?;
        let SpawnedSequencerProcess {
            child,
            endpoint,
            internal_endpoint,
        } = require_ready(
            // The default batch-open deadline on first boot.
            spawn_run_process(&child_env, &log_path, None).await?,
            &log_path,
        )?;

        Ok(Self {
            rollups,
            child,
            shutdown_timeout: DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT,
            sequencer_bin,
            genesis_bin,
            log_prefix,
            logs_dir,
            _root_dir: root_dir,
            data_dir_path,
            discarded_data_dirs: 0,
            setup_pending: false,
            setup_base: None,
            endpoint,
            internal_endpoint,
            log_path,
            l1_endpoint_override: None,
            chain_id_override: None,
            faketime_rc_path,
            libfaketime_path,
            cumulative_offset_secs: 0,
            mine_l1_during_setup: false,
            held_submitter_balance: None,
            max_batch_open_seconds: None,
            fee_oracle_fixed_log_gas_price: config.fee_oracle_fixed_log_gas_price,
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
    /// `--chain-id` flows only to `setup`, which runs only in a new data
    /// directory ([`Self::discard_data_dir`]); `run` validates against the
    /// pinned chain id, so on a set-up directory the override is inert. Does
    /// not affect a running process.
    pub fn set_chain_id_override(&mut self, chain_id: Option<u64>) {
        self.chain_id_override = chain_id;
    }

    /// The machine directory the next setup starts from (`setup --base`);
    /// `None` is genesis. Takes effect on the next setup only, which runs in a
    /// new data directory ([`Self::discard_data_dir`]). Persists until changed.
    pub fn set_setup_base(&mut self, base: Option<PathBuf>) {
        self.setup_base = base;
    }

    /// Make every setup (until cleared) mine L1 blocks while it runs.
    ///
    /// Setup flushes the submitter's wallet and waits until the flushed
    /// slots are *safe*. Auto-mining lands a flush transaction but mints no
    /// further blocks, so on an on-demand-mining devnet Anvil's `safe` tag
    /// never passes it; a live chain keeps producing blocks. Only a setup
    /// that follows an instance with stranded submissions needs this: mining
    /// during an unrelated setup would perturb block-sensitive assertions.
    pub fn set_mine_l1_during_setup(&mut self, enabled: bool) {
        self.mine_l1_during_setup = enabled;
    }

    /// Move to a new, empty data directory, as an operator does for a new
    /// deployment, after a failed setup, or for a recovery: the next
    /// [`Self::respawn`] runs `setup` there before `run`. Call while the
    /// sequencer is stopped. The old directory is left as it was, and nothing
    /// runs against it again; it is removed when the harness drops.
    ///
    /// Setup refuses a directory that holds anything, so this is the only way
    /// to set up again: clearing files inside a used directory would leave
    /// whatever the clearing missed for setup to refuse.
    pub fn discard_data_dir(&mut self) {
        self.discarded_data_dirs += 1;
        self.data_dir_path = self
            ._root_dir
            .path()
            .join(format!("{DATA_DIR_NAME}-{}", self.discarded_data_dirs));
        self.setup_pending = true;
    }

    /// Run `setup` in the current data directory without starting `run`, and
    /// report how it ended instead of failing on a non-zero exit. A success
    /// clears the pending setup, so the next [`Self::respawn`] only runs. Used
    /// to assert setup's exit codes, such as its refusal of a set-up directory.
    pub async fn run_setup(&mut self) -> HarnessResult<SetupOutcome> {
        let log_path = timestamped_log_path(self.logs_dir.as_path(), self.log_prefix.as_str());
        let outcome = run_setup_process(
            &self.child_env(),
            &self.setup_request(false),
            &log_path,
            self.mine_l1_during_setup,
        )
        .await?;
        if outcome.success() {
            self.setup_pending = false;
        }
        Ok(outcome)
    }

    /// `setup --dry-run` against the configured base: it needs no key and
    /// never touches the data directory, so it may run at any time, with the
    /// sequencer up or down.
    pub async fn setup_dry_run(&self) -> HarnessResult<SetupOutcome> {
        let log_path = timestamped_log_path(self.logs_dir.as_path(), self.log_prefix.as_str());
        run_setup_process(
            &self.child_env(),
            &self.setup_request(true),
            &log_path,
            false,
        )
        .await
    }

    fn child_env(&self) -> ChildEnv<'_> {
        ChildEnv {
            bin: self.sequencer_bin.as_path(),
            data_dir: self.data_dir_path.as_path(),
            rollups: &self.rollups,
            eth_rpc_url: self
                .l1_endpoint_override
                .as_deref()
                .unwrap_or_else(|| self.rollups.l1_endpoint()),
            faketime: self
                .libfaketime_path
                .as_deref()
                .zip(self.faketime_rc_path.as_deref()),
        }
    }

    fn setup_request(&self, dry_run: bool) -> SetupRequest<'_> {
        SetupRequest {
            chain_id: self.chain_id_override.unwrap_or(DEVNET_CHAIN_ID),
            base: self.setup_base.as_deref(),
            dry_run,
            genesis_bin: self.genesis_bin.as_deref(),
            fee_oracle_fixed_log_gas_price: self.fee_oracle_fixed_log_gas_price,
        }
    }

    /// Pass `--max-batch-open-seconds` to the `run` phase on the next (and
    /// subsequent) respawns so the inclusion lane seals the open batch after
    /// `secs` instead of the 2h default. `None` restores the default. Lets a
    /// test drive a batch to close + promotion without a 2h clock advance.
    pub fn set_max_batch_open_seconds(&mut self, secs: Option<u64>) {
        self.max_batch_open_seconds = secs;
    }

    /// The currently exportable canonical checkpoint's L1 block. In a new era
    /// it is the baseline's stop block until the era's first batch is
    /// accepted.
    pub async fn finalized_inclusion_block(&self) -> HarnessResult<Option<u64>> {
        let response = reqwest::Client::new()
            .get(format!(
                "{}/finalized_state/inclusion_block",
                self.internal_endpoint()
            ))
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        #[derive(serde::Deserialize)]
        struct Metadata {
            inclusion_block: u64,
        }
        let metadata: Metadata = response.error_for_status()?.json().await?;
        Ok(Some(metadata.inclusion_block))
    }

    /// Read the first frame's persisted fee. This observes startup directly,
    /// before any deposit can rotate the frame.
    pub fn first_frame_fee(&self) -> HarnessResult<u16> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;
        let fee: i64 = conn
            .query_row(
                "SELECT fee FROM frames ORDER BY batch_index, frame_in_batch LIMIT 1",
                [],
                |row| row.get(0),
            )
            .map_err(|err| io_other(format!("read first frame fee: {err}")))?;
        u16::try_from(fee)
            .map_err(|_| io_other(format!("first frame fee out of range: {fee}")).into())
    }

    /// Return `(open_frame_safe_block, persisted_l1_safe_head)` from one
    /// read-only SQLite snapshot. This observes the two sides of the live
    /// frame-clock condition without relying on incidental Anvil transaction
    /// counts.
    pub fn frame_clock_observation(&self) -> HarnessResult<(u64, u64)> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;
        let (frame_safe_block, persisted_safe_head): (i64, i64) = conn
            .query_row(
                "SELECT f.safe_block, h.block_number \
                 FROM open_batch b \
                 JOIN frames f ON f.batch_index = b.batch_index \
                 JOIN l1_safe_head h ON h.singleton_id = 0 \
                 ORDER BY f.frame_in_batch DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|err| io_other(format!("read frame-clock observation: {err}")))?;
        let frame_safe_block = u64::try_from(frame_safe_block).map_err(|_| {
            io_other(format!(
                "open-frame safe block is negative: {frame_safe_block}"
            ))
        })?;
        let persisted_safe_head = u64::try_from(persisted_safe_head).map_err(|_| {
            io_other(format!(
                "persisted L1 safe head is negative: {persisted_safe_head}"
            ))
        })?;
        Ok((frame_safe_block, persisted_safe_head))
    }

    /// The oldest batch L1 has not accepted, closed or open, and its first
    /// frame's safe block, from one read-only snapshot; `None` when every
    /// batch is accepted.
    pub fn oldest_unaccepted_batch(&self) -> HarnessResult<Option<UnacceptedBatch>> {
        use rusqlite::OptionalExtension;
        let conn = self.read_only_connection()?;
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT b.nonce, \
                        (SELECT f.safe_block FROM frames f \
                          WHERE f.batch_index = b.batch_index \
                          ORDER BY f.frame_in_batch LIMIT 1) \
                 FROM batches b \
                 WHERE b.accepted_input IS NULL \
                 ORDER BY b.nonce LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|err| io_other(format!("read the oldest unaccepted batch: {err}")))?;
        row.map(|(nonce, first_frame_safe_block)| {
            Ok(UnacceptedBatch {
                nonce: u64::try_from(nonce)
                    .map_err(|_| io_other(format!("negative batch nonce {nonce}")))?,
                first_frame_safe_block: u64::try_from(first_frame_safe_block).map_err(|_| {
                    io_other(format!(
                        "negative first-frame safe block {first_frame_safe_block}"
                    ))
                })?,
            })
        })
        .transpose()
    }

    /// The accepted batches as `(nonce, accepted safe input)`, by nonce.
    pub fn accepted_batches(&self) -> HarnessResult<Vec<(u64, u64)>> {
        let conn = self.read_only_connection()?;
        let mut statement = conn
            .prepare(
                "SELECT nonce, accepted_input FROM batches \
                 WHERE accepted_input IS NOT NULL ORDER BY nonce",
            )
            .map_err(|err| io_other(format!("prepare accepted batches: {err}")))?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .map_err(|err| io_other(format!("read accepted batches: {err}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|err| io_other(format!("read accepted batches: {err}")))?;
        Ok(rows
            .into_iter()
            .map(|(nonce, input)| (nonce as u64, input as u64))
            .collect())
    }

    /// The causes the terminal-fault black box recorded, oldest first. A
    /// start refused on a terminal fact records one; a runtime abort records
    /// none.
    pub fn terminal_fault_causes(&self) -> HarnessResult<Vec<String>> {
        let conn = self.read_only_connection()?;
        let mut statement = conn
            .prepare("SELECT cause FROM terminal_faults ORDER BY fault_id")
            .map_err(|err| io_other(format!("prepare terminal faults: {err}")))?;
        let causes = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|err| io_other(format!("read terminal faults: {err}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|err| io_other(format!("read terminal faults: {err}")))?;
        Ok(causes)
    }

    /// The landing reading `GET /healthz` serves on the internal listener,
    /// whatever its status code.
    pub async fn landing_health(&self) -> HarnessResult<LandingHealth> {
        let response = reqwest::Client::new()
            .get(format!("{}/healthz", self.internal_endpoint()))
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        Ok(response.json().await?)
    }

    /// Read-only; safe to call while the sequencer is running.
    pub fn batch_nonce_frontier(&self) -> HarnessResult<BatchNonceFrontier> {
        let conn = rusqlite::Connection::open_with_flags(
            self.db_path().as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;

        let sealed: Option<i64> = conn
            .query_row("SELECT MAX(nonce) FROM closed_batches", [], |row| {
                row.get(0)
            })
            .map_err(|err| io_other(format!("max sealed batch nonce: {err}")))?;
        let safe_accepted: Option<i64> = conn
            .query_row(
                "SELECT MAX(nonce) FROM batches WHERE accepted_input IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(|err| io_other(format!("max safe-accepted batch nonce: {err}")))?;
        Ok(BatchNonceFrontier {
            sealed: sealed.map(|n| n as u64),
            safe_accepted: safe_accepted.map(|n| n as u64),
        })
    }

    /// The era's anchor nonce (`anchor_batch_nonce`), read from the run DB
    /// read-only: the resume nonce of a rebuilt deployment, `0` for a genesis
    /// one. The batch nonces are contiguous from it (I16). Fails when the era
    /// is absent.
    pub fn batch_tree_anchor(&self) -> HarnessResult<u64> {
        let db_path = self.data_dir_path.join("sequencer.db");
        let conn = rusqlite::Connection::open_with_flags(
            db_path.as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")))?;
        let anchor: i64 = conn
            .query_row(
                "SELECT anchor_batch_nonce FROM era WHERE singleton_id = 0",
                [],
                |row| row.get(0),
            )
            .map_err(|err| io_other(format!("read the era's anchor nonce: {err}")))?;
        Ok(anchor as u64)
    }

    /// The era setup published, from one read-only snapshot.
    pub fn published_era(&self) -> HarnessResult<PublishedEra> {
        let conn = self.read_only_connection()?;
        let (base_count, stop_block, rebuilt_history_end, first_queued_input, next_batch_nonce): (
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT base_l2_tx_count, stop_block, rebuilt_history_end, \
                        first_queued_input, anchor_batch_nonce \
                 FROM era WHERE singleton_id = 0",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .map_err(|err| io_other(format!("read the published era: {err}")))?;
        let unsigned = |name: &str, value: i64| {
            u64::try_from(value).map_err(|_| io_other(format!("{name} is negative: {value}")))
        };
        Ok(PublishedEra {
            base_count: unsigned("base count", base_count)?,
            stop_block: unsigned("stop block", stop_block)?,
            rebuilt_history_end: unsigned("rebuilt history end", rebuilt_history_end)?,
            first_queued_input: unsigned("first queued input", first_queued_input)?,
            next_batch_nonce: unsigned("next batch nonce", next_batch_nonce)?,
        })
    }

    /// How many rows setup's rebuild stored, and how many carry a count.
    pub fn rebuilt_history_rows(&self) -> HarnessResult<RebuiltHistoryRows> {
        let conn = self.read_only_connection()?;
        let (total, executed): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(l2_tx_count) FROM rebuilt_history",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|err| io_other(format!("count rebuilt history rows: {err}")))?;
        Ok(RebuiltHistoryRows {
            total: total as u64,
            executed: executed as u64,
        })
    }

    fn read_only_connection(&self) -> HarnessResult<rusqlite::Connection> {
        rusqlite::Connection::open_with_flags(
            self.db_path().as_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|err| io_other(format!("open DB read-only: {err}")).into())
    }

    /// The canonical-divergence marker (I9/I15) from the run DB, or `None`
    /// when the frontier is healthy: its `kind`, `foreign` or `mismatch`. A
    /// rebuilt era asserts `None` to prove the content-identity check did not
    /// spuriously freeze the frontier against its rebuilt history.
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

    /// Snapshot of the `batches` table: `(total, sealed)`.
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
        Ok(BatchCounts {
            total: total as u64,
            sealed: sealed as u64,
        })
    }

    /// Assert the schema-level batch invariants on the sequencer's DB. Runs
    /// against the DB file read-only; safe to call whether the sequencer is
    /// running or stopped (SQLite WAL + read-only flag handles concurrent
    /// writers).
    ///
    /// Invariants checked:
    ///   1. At most one `open_batch` row (partial unique index
    ///      `ux_single_open_batch` should guarantee this structurally —
    ///      we verify it in case the index ever regressed).
    ///   2. In `batch_index` order, each batch's nonce is the era's anchor
    ///      plus its position (I16): unique and contiguous.
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

        // The era's anchor: the first batch's nonce (0 for a genesis
        // deployment, N' for a cockroach-recovered one). Mirrors
        // `trg_batch_insert`. Reuses the `batch_tree_anchor()`
        // accessor (its own short-lived read-only handle) rather than
        // re-issuing the query against `conn`.
        let anchor = self.batch_tree_anchor()? as i64;

        // 1. At most one open batch.
        let open_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM open_batch", [], |row| row.get(0))
            .map_err(|err| io_other(format!("count open_batch: {err}")))?;
        if open_count > 1 {
            panic!("schema invariant: more than one open batch ({open_count} rows)");
        }

        // 2. The nonces follow the batches from the anchor.
        let mut stmt = conn
            .prepare("SELECT nonce FROM batches ORDER BY batch_index ASC")
            .map_err(|err| io_other(format!("prepare batch nonces: {err}")))?;
        let nonces: Vec<i64> = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|err| io_other(format!("query batch nonces: {err}")))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|err| io_other(format!("collect batch nonces: {err}")))?;
        for (i, &n) in nonces.iter().enumerate() {
            if n != anchor + i as i64 {
                panic!(
                    "schema invariant: batch nonces are not the anchor {anchor} plus their \
                     position: {nonces:?}"
                );
            }
        }

        Ok(())
    }

    /// The public listener: `/tx`, `/fee`, `/nonce`, `/domain`.
    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    /// The internal listener: subscriptions, history, snapshots and health.
    pub fn internal_endpoint(&self) -> &str {
        self.internal_endpoint.as_str()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn log_path(&self) -> &Path {
        self.log_path.as_path()
    }

    pub fn anvil_log_path(&self) -> &Path {
        self.rollups.anvil_log_path()
    }

    pub fn data_dir(&self) -> &Path {
        self.data_dir_path.as_path()
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir_path.join("sequencer.db")
    }

    /// The names in the data directory, sorted; empty while it does not
    /// exist. Setup's refusals before its first write leave at most the lock
    /// file here.
    pub fn data_dir_entries(&self) -> HarnessResult<Vec<String>> {
        let entries = match fs::read_dir(self.data_dir_path.as_path()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(io_other(format!("read data dir: {err}")).into()),
        };
        let mut names = entries
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        names.sort();
        Ok(names)
    }

    /// Every file and directory under the data directory by relative path,
    /// sorted, a file with its length and the keccak256 of its bytes; empty
    /// while the directory does not exist. Two equal readings mean nothing was
    /// added, removed or rewritten.
    pub fn data_dir_contents(&self) -> HarnessResult<Vec<DataDirEntry>> {
        let root = self.data_dir_path.as_path();
        let mut contents = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let entries = match fs::read_dir(dir.as_path()) {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound && dir == root => {
                    return Ok(Vec::new());
                }
                Err(err) => {
                    return Err(io_other(format!("read {}: {err}", dir.display())).into());
                }
            };
            for entry in entries {
                let path = entry?.path();
                let name = path
                    .strip_prefix(root)
                    .map_err(|err| io_other(format!("{}: {err}", path.display())))?
                    .to_string_lossy()
                    .into_owned();
                if path.is_dir() {
                    contents.push((name, None));
                    pending.push(path);
                    continue;
                }
                let bytes = fs::read(path.as_path())
                    .map_err(|err| io_other(format!("read {}: {err}", path.display())))?;
                let len =
                    u64::try_from(bytes.len()).map_err(|_| io_other("file length overflows"))?;
                contents.push((name, Some((len, alloy_primitives::keccak256(&bytes)))));
            }
        }
        contents.sort();
        Ok(contents)
    }

    /// Live bytes in the database: pages in use, including pages still in the
    /// WAL and excluding free pages. File sizes show neither. Read while the
    /// sequencer runs, so the WAL index is there for the read-only connection.
    pub fn logical_db_bytes(&self) -> HarnessResult<u64> {
        logical_db_bytes_at(self.db_path().as_path())
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

    /// L1 blocks of the application's InputBox inputs, in input order.
    pub async fn input_blocks(&self) -> HarnessResult<Vec<u64>> {
        Ok(self
            .raw_inputs()
            .await?
            .into_iter()
            .map(|(block, _)| block)
            .collect())
    }

    /// The application's InputBox inputs, in input order: each one's L1 block
    /// and its `EvmAdvance` encoding, the bytes a machine receives.
    pub async fn raw_inputs(&self) -> HarnessResult<Vec<(u64, Vec<u8>)>> {
        let provider = ProviderBuilder::new()
            .connect(self.l1_endpoint())
            .await
            .map_err(|err| io_other(format!("failed to connect L1 provider: {err}")))?;
        let filter = Filter::new()
            .address(self.input_box_address())
            .event_signature(InputAdded::SIGNATURE_HASH)
            .topic1(self.app_address().into_word())
            .from_block(0);
        let logs = provider
            .get_logs(&filter)
            .await
            .map_err(|err| io_other(format!("failed to read InputAdded logs: {err}")))?;
        // Callers index inputs by position, so the log order must be the
        // InputBox order.
        (0_u64..)
            .zip(&logs)
            .map(|(position, log)| {
                let block = log
                    .block_number
                    .ok_or_else(|| io_other("InputAdded log without a block number"))?;
                let event = InputAdded::decode_log_data(&log.inner.data)
                    .map_err(|err| io_other(format!("failed to decode InputAdded: {err}")))?;
                if event.index != U256::from(position) {
                    return Err(io_other(format!(
                        "InputAdded log {position} carries index {}",
                        event.index
                    ))
                    .into());
                }
                Ok((block, event.input.to_vec()))
            })
            .collect()
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

    /// Mine outage-style L1 progress at 12 seconds per block. Pair large
    /// advances with faketime, or use [`Self::mine_live_l1_blocks`] for
    /// ordinary liveness and finality polling.
    pub async fn mine_l1_blocks(&self, block_count: u64) -> HarnessResult<()> {
        self.rollups.mine_l1_blocks(block_count).await
    }

    /// Mine ordinary live-chain progress at one second per block.
    pub async fn mine_live_l1_blocks(&self, block_count: u64) -> HarnessResult<()> {
        self.rollups.mine_live_l1_blocks(block_count).await
    }

    /// Mine one block at Anvil's own clock; see
    /// [`DevnetRollupsStack::mine_l1_block_at_wall_clock`](crate::DevnetRollupsStack::mine_l1_block_at_wall_clock).
    pub async fn mine_l1_block_at_wall_clock(&self) -> HarnessResult<()> {
        self.rollups.mine_l1_block_at_wall_clock().await
    }

    pub async fn l1_safe_block_number(&self) -> HarnessResult<u64> {
        self.rollups.l1_safe_block_number().await
    }

    /// Toggle Anvil's auto-mining mode. When disabled, txs accumulate in
    /// the mempool until an explicit mine or re-enable: a sequencer's
    /// batch-submission tx waits there, where [`Self::drop_all_pending_txs`]
    /// can lose it. `anvil_mine` includes pending txs, so hold submissions
    /// across mining with [`Self::hold_batch_submissions`] instead.
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

    /// Wait until the sequencer's batch submitter has a transaction in the
    /// mempool. With auto-mining off, that is the batch submission held out
    /// of the chain.
    pub async fn wait_for_pending_batch_submission(&self) -> HarnessResult<()> {
        // Several idle ticks of the submitter (5 s by default).
        const TIMEOUT: Duration = Duration::from_secs(30);
        self.rollups
            .wait_for_pending_tx_from(app_core::application::DEVNET_SEQUENCER_ADDRESS, TIMEOUT)
            .await
    }

    /// Advance both the sequencer's wall clock and the L1 chain by `duration`,
    /// maintaining the block-time coupling invariant
    /// ([`DEVNET_SECONDS_PER_BLOCK`], which the harness passes to the sequencer).
    ///
    /// This is the primary tool for simulating elapsed outage time. Effective
    /// **immediately** — works whether the sequencer is running or stopped:
    ///   - The faketime rc file is updated; the running sequencer's next time
    ///     call (or a post-respawn first call) sees the shifted clock.
    ///   - Anvil mines `duration.as_secs() / DEVNET_SECONDS_PER_BLOCK` blocks.
    ///
    /// **Cumulative**: calling with `1h` twice totals `+2h`, not `+1h`.
    pub async fn advance_wall_and_mine(&mut self, duration: Duration) -> HarnessResult<()> {
        let secs = duration.as_secs();
        let blocks = secs / DEVNET_SECONDS_PER_BLOCK;
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
    ///   elapses. The internal `wait()` future is dropped, so a later
    ///   [`Self::wait_for_exit`] still works.
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
    /// Used by tests that expect the sequencer to stop on its own, such as
    /// on a terminal fact. After this returns, call [`Self::respawn`] or
    /// [`Self::try_respawn`] to start a fresh process.
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

    /// Kill the sequencer process. Anvil stays running, so `mine_l1_blocks()` still works.
    pub async fn stop(&mut self) -> HarnessResult<()> {
        self.shutdown_child().await
    }

    /// Stop a *healthy* sequencer with SIGTERM and assert the exit-code
    /// contract's clean class: a graceful operator shutdown is exit 0, the one
    /// code that tells a supervisor nothing needs doing. Opt-in, because many
    /// scenarios stop a process that has already exited with a failure class
    /// on purpose. The literal `0` is deliberate: it pins the wire value, not
    /// a constant.
    pub async fn stop_expecting_clean_exit(&mut self) -> HarnessResult<()> {
        let status = self.drain_child_status().await?;
        if status.code() != Some(0) {
            return Err(io_other(format!(
                "a healthy sequencer must exit 0 on SIGTERM, got {status:?}"
            ))
            .into());
        }
        Ok(())
    }

    /// Respawn the sequencer process using the same data directory and Anvil instance.
    ///
    /// Honors any `l1_endpoint_override` set via [`Self::set_l1_endpoint_override`]
    /// and the faketime offset [`Self::advance_wall_and_mine`] wrote.
    ///
    /// Runs `setup` first only when the data directory is new
    /// ([`Self::discard_data_dir`]): setup refuses a directory that holds
    /// anything, so a set-up directory goes straight to `run`. A failed setup
    /// stays pending, and its retry meets whatever the failure left behind,
    /// as an operator's would. A `run` that exits before readiness is an
    /// error; [`Self::try_respawn`] reports its status instead.
    pub async fn respawn(&mut self) -> HarnessResult<()> {
        match self.try_respawn().await? {
            RunStart::Ready => Ok(()),
            RunStart::Exited(status) => Err(io_other(format!(
                "sequencer exited before readiness: status={status} (see {})",
                self.log_path.display()
            ))
            .into()),
        }
    }

    /// [`Self::respawn`], reporting a `run` that exits before readiness by
    /// its exit status, so a refused start is asserted by its code. The
    /// exited process becomes the managed child.
    pub async fn try_respawn(&mut self) -> HarnessResult<RunStart> {
        let log_path = timestamped_log_path(self.logs_dir.as_path(), self.log_prefix.as_str());
        if self.setup_pending {
            require_setup_success(
                run_setup_process(
                    &self.child_env(),
                    &self.setup_request(false),
                    &log_path,
                    self.mine_l1_during_setup,
                )
                .await?,
                &log_path,
            )?;
            self.setup_pending = false;
        }
        let started =
            spawn_run_process(&self.child_env(), &log_path, self.max_batch_open_seconds).await?;
        self.log_path = log_path;
        match started {
            RunProcess::Ready(SpawnedSequencerProcess {
                child,
                endpoint,
                internal_endpoint,
            }) => {
                self.child = child;
                self.endpoint = endpoint;
                self.internal_endpoint = internal_endpoint;
                Ok(RunStart::Ready)
            }
            RunProcess::Exited(child, status) => {
                self.child = child;
                Ok(RunStart::Exited(status))
            }
        }
    }

    /// Hold the batch submitter's transactions back while the sequencer
    /// runs: its balance goes to zero, so every send is refused for
    /// insufficient funds and the poster retries on its next tick. Nothing
    /// reaches the mempool, so mining does not land it either.
    pub async fn hold_batch_submissions(&mut self) -> HarnessResult<()> {
        if self.held_submitter_balance.is_some() {
            return Err(io_other("batch submissions are already held").into());
        }
        let submitter = app_core::application::DEVNET_SEQUENCER_ADDRESS;
        let balance = self.rollups.balance_of(submitter).await?;
        self.rollups.set_balance(submitter, U256::ZERO).await?;
        self.held_submitter_balance = Some(balance);
        Ok(())
    }

    /// Restore the balance [`Self::hold_batch_submissions`] took away.
    pub async fn release_batch_submissions(&mut self) -> HarnessResult<()> {
        let balance = self
            .held_submitter_balance
            .take()
            .ok_or_else(|| io_other("batch submissions are not held"))?;
        self.rollups
            .set_balance(app_core::application::DEVNET_SEQUENCER_ADDRESS, balance)
            .await
    }

    /// Broadcast `payload` to the application's InputBox from `signer` without
    /// waiting for it to be mined; returns its hash. With auto-mining off it
    /// stays in the mempool, as a zombie of an old instance would.
    pub async fn broadcast_input_as(
        &self,
        signer: &TestSigner,
        payload: Vec<u8>,
    ) -> HarnessResult<B256> {
        use cartesi_rollups_contracts::input_box::InputBox;
        let provider = signer.connect_l1_provider(self.l1_endpoint()).await?;
        let pending = InputBox::new(self.input_box_address(), &provider)
            .addInput(self.app_address(), payload.into())
            .send()
            .await
            .map_err(|err| io_other(format!("failed to send InputBox input: {err}")))?;
        Ok(*pending.tx_hash())
    }

    /// Whether the transaction `hash` is in a block.
    pub async fn transaction_mined(&self, hash: B256) -> HarnessResult<bool> {
        let provider = ProviderBuilder::new()
            .connect(self.l1_endpoint())
            .await
            .map_err(|err| io_other(format!("failed to connect L1 provider: {err}")))?;
        let receipt = provider
            .get_transaction_receipt(hash)
            .await
            .map_err(|err| io_other(format!("failed to read receipt {hash}: {err}")))?;
        Ok(receipt.is_some())
    }

    /// Send `payload` to the application's InputBox from `signer`, as any
    /// account can; returns the block that holds it. Inputs from the batch
    /// submitter's key are batches, whatever process signs them.
    pub async fn add_input_as(&self, signer: &TestSigner, payload: Vec<u8>) -> HarnessResult<u64> {
        use cartesi_rollups_contracts::input_box::InputBox;
        let provider = signer.connect_l1_provider(self.l1_endpoint()).await?;
        let receipt = InputBox::new(self.input_box_address(), &provider)
            .addInput(self.app_address(), payload.into())
            .send()
            .await
            .map_err(|err| io_other(format!("failed to send InputBox input: {err}")))?
            .get_receipt()
            .await
            .map_err(|err| io_other(format!("failed to confirm InputBox input: {err}")))?;
        if !receipt.status() {
            return Err(io_other("InputBox input reverted").into());
        }
        receipt
            .block_number
            .ok_or_else(|| io_other("InputBox receipt has no block number").into())
    }

    pub async fn restart(&mut self) -> HarnessResult<()> {
        self.stop().await?;
        self.respawn().await
    }

    /// Read the current sequencer log file contents.
    pub fn read_log_contents(&self) -> HarnessResult<String> {
        std::fs::read_to_string(&self.log_path).map_err(Into::into)
    }

    /// Test-only fresh replay claim. Reconnection tests retain their original claim explicitly.
    pub fn history_claim(
        &self,
        next_input: u64,
    ) -> HarnessResult<sequencer_rust_client::HistoryClaim> {
        let conn = rusqlite::Connection::open_with_flags(
            self.data_dir_path.join("sequencer.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let (era, generation): (Vec<u8>, i64) = conn.query_row(
            "SELECT era_id, \
                    (SELECT COALESCE(MAX(recovery_generation), 0) FROM history_generation_cuts) \
               FROM era WHERE singleton_id = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(sequencer_rust_client::HistoryClaim {
            version: sequencer_rust_client::HistoryVersion {
                era_id: sequencer_core::history::EraId::try_from(era.as_slice())?,
                recovery_generation: sequencer_core::history::RecoveryGeneration::new(
                    u64::try_from(generation)?,
                ),
            },
            next_input: sequencer_rust_client::L2TxCount::new(next_input),
        })
    }

    pub async fn ws(&self, next_input: u64) -> HarnessResult<WsClient> {
        let client = self.sequencer_client()?;
        WsClient::connect(&client, self.history_claim(next_input)?).await
    }

    /// Subscribe as a client of an older era holding state at `count` does
    /// after a recovery: adopt the current era at its own count through the
    /// SDK, then follow the feed from there.
    pub async fn ws_adopting(&self, count: u64) -> HarnessResult<WsClient> {
        let client = self.sequencer_client()?;
        let claim = client
            .adopt_current_era(sequencer_rust_client::L2TxCount::new(count))
            .await?;
        WsClient::connect(&client, claim).await
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

    /// A client for the internal listener, where the feed and `/history` live.
    pub fn sequencer_client(&self) -> HarnessResult<SequencerClient> {
        SequencerClient::new_with_timeout(self.internal_endpoint.clone(), Duration::from_secs(5))
            .map_err(|err| io_other(format!("failed to create sequencer client: {err}")).into())
    }

    async fn shutdown_child(&mut self) -> HarnessResult<()> {
        let _ = self.drain_child_status().await?;
        Ok(())
    }

    /// Send SIGTERM and wait for the child to drain, returning its exit
    /// status. A drain that outlives `shutdown_timeout` is force-killed and
    /// reported as an error.
    async fn drain_child_status(&mut self) -> HarnessResult<std::process::ExitStatus> {
        send_graceful_terminate(&mut self.child).await;
        match tokio::time::timeout(self.shutdown_timeout, self.child.wait()).await {
            Ok(wait_result) => Ok(wait_result?),
            Err(_) => {
                self.child.start_kill()?;
                let _ = self.child.wait().await;
                Err(io_other(format!(
                    "sequencer did not drain within {:?}; forced kill was required",
                    self.shutdown_timeout
                ))
                .into())
            }
        }
    }
}

use alloy_primitives::Address;

/// The block time the harness mines outages at, passed explicitly so an
/// exported `CARTESI_SEQUENCER_*` variable cannot shift it.
fn timing_args() -> [String; 2] {
    [
        "--seconds-per-block".into(),
        DEVNET_SECONDS_PER_BLOCK.to_string(),
    ]
}

/// The batch submitter's signer (Anvil account 9). A transaction it signs to
/// the InputBox is a batch, whichever process sends it.
pub fn batch_submitter_signer() -> HarnessResult<TestSigner> {
    TestSigner::from_private_key_hex(&devnet_submitter()?.0)
}

/// A batch's InputBox payload: its SSZ encoding, with no envelope or tag.
pub fn encode_batch(batch: &sequencer_core::batch::Batch) -> Vec<u8> {
    ssz::Encode::as_ssz_bytes(batch)
}

struct SpawnedSequencerProcess {
    child: Child,
    endpoint: String,
    internal_endpoint: String,
}

/// How a `run` child's start ended.
enum RunProcess {
    Ready(SpawnedSequencerProcess),
    /// The child exited before readiness, with this status.
    Exited(Child, ExitStatus),
}

/// A start a spawn depends on must reach readiness.
fn require_ready(started: RunProcess, log_path: &Path) -> HarnessResult<SpawnedSequencerProcess> {
    match started {
        RunProcess::Ready(process) => Ok(process),
        RunProcess::Exited(_, status) => Err(io_other(format!(
            "sequencer exited before readiness: status={status} (see {})",
            log_path.display()
        ))
        .into()),
    }
}

/// `(page_count - freelist_count) * page_size`, read in one statement so all
/// three come from the same snapshot.
fn logical_db_bytes_at(db_path: &Path) -> HarnessResult<u64> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|err| io_other(format!("open DB read-only: {err}")))?;
    let bytes: i64 = conn
        .query_row(
            "SELECT (page_count - freelist_count) * page_size \
             FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )
        .map_err(|err| io_other(format!("read logical DB size: {err}")))?;
    u64::try_from(bytes).map_err(|_| io_other(format!("negative logical DB size: {bytes}")).into())
}

/// What every child the harness starts shares: the binary, the stack it
/// talks to, its data directory and its clock.
struct ChildEnv<'a> {
    bin: &'a Path,
    data_dir: &'a Path,
    rollups: &'a DevnetRollupsStack,
    eth_rpc_url: &'a str,
    /// The libfaketime library and the rc file it re-reads; `None` runs on
    /// the real clock.
    faketime: Option<(&'a Path, &'a Path)>,
}

impl ChildEnv<'_> {
    fn command(&self) -> HarnessResult<Command> {
        let mut command = Command::new(self.bin);
        // `kill_on_drop`: a `ManagedSequencer` normally reaps its child via
        // `shutdown()` / `stop()`, but an error path that drops the struct
        // without either (e.g. a benchmark returning early on a misconfigured
        // run) would otherwise leave the child running while the owning
        // `TempDir` deletes the data dir — the child's next DB open then
        // fails `ENOENT` and it exits with a confusing `unable to open
        // database file` that masks the real failure. `child` is declared
        // before `_root_dir`, so on drop the SIGKILL lands before the dir is
        // removed.
        command
            .kill_on_drop(true)
            .env_remove("CARTESI_SEQUENCER_STATE_FILE")
            .env("RUST_LOG", DEFAULT_SEQUENCER_RUST_LOG);
        // libfaketime is applied via env vars (not the `faketime` wrapper
        // binary), which the file-based FAKETIME_TIMESTAMP_FILE mechanism
        // reads on every time call (FAKETIME_NO_CACHE=1) so tests can shift
        // the clock at runtime.
        if let Some((lib, rc)) = self.faketime {
            apply_faketime_env(&mut command, lib, rc)?;
        }
        Ok(command)
    }
}

/// One `setup` invocation: the base it starts from and whether it only
/// reports.
struct SetupRequest<'a> {
    chain_id: u64,
    /// The stored machine to rebuild from; `None` is genesis.
    base: Option<&'a Path>,
    dry_run: bool,
    /// The C host's genesis tool, run for a setup from genesis.
    genesis_bin: Option<&'a Path>,
    fee_oracle_fixed_log_gas_price: Option<i16>,
}

/// The harness's batch submitter: its private key (hex) and address.
fn devnet_submitter() -> HarnessResult<(String, Address)> {
    let key = default_private_keys()[DEVNET_SUBMITTER_ACCOUNT_INDEX].clone();
    let address = TestSigner::from_private_key_hex(&key)?.address();
    // `assert_eq!`, not `debug_assert_eq!`: benches build the harness
    // `--release`, where a debug-assert is a no-op — drift between app-core
    // and the harness submitter must fail fast in every build.
    assert_eq!(
        address,
        app_core::application::DEVNET_SEQUENCER_ADDRESS,
        "harness submitter (anvil account {DEVNET_SUBMITTER_ACCOUNT_INDEX}) must equal \
         DEVNET_SEQUENCER_ADDRESS — keep app-core and the harness in sync"
    );
    Ok((key, address))
}

/// The arguments of one `setup` run. Setup takes the submitter key, the same
/// way `run` does, because it flushes the submitter's wallet; the dry run
/// flushes nothing and gets no key. The genesis state goes only to a setup
/// without a base.
fn setup_args(
    request: &SetupRequest<'_>,
    data_dir: &Path,
    eth_rpc_url: &str,
    app_address: Address,
    submitter: (&str, Address),
    genesis_state: Option<&Path>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if request.base.is_none()
        && let Some(state) = genesis_state
    {
        args.extend(["--state-file".into(), state.into()]);
    }
    args.extend([
        "setup".into(),
        "--data-dir".into(),
        data_dir.into(),
        "--eth-rpc-url".into(),
        eth_rpc_url.into(),
        "--chain-id".into(),
        request.chain_id.to_string().into(),
        "--app-address".into(),
        app_address.to_string().into(),
        "--batch-submitter-address".into(),
        submitter.1.to_string().into(),
    ]);
    args.extend(timing_args().map(OsString::from));
    if let Some(base) = request.base {
        args.extend(["--base".into(), base.into()]);
    }
    if request.dry_run {
        args.push("--dry-run".into());
    } else {
        args.extend(["--batch-submitter-private-key".into(), submitter.0.into()]);
    }
    args
}

/// Run `setup` to completion and report how it ended. Its summary (stdout)
/// is returned and, with stderr, appended to `log_path`. With
/// `mine_l1_during_setup`, L1 advances while setup runs, so a flush of an
/// old instance's stranded transactions can reach safe depth on an
/// on-demand-mining devnet.
async fn run_setup_process(
    env: &ChildEnv<'_>,
    request: &SetupRequest<'_>,
    log_path: &Path,
    mine_l1_during_setup: bool,
) -> HarnessResult<SetupOutcome> {
    let (submitter_key, submitter_address) = devnet_submitter()?;
    // The genesis tool runs only for a setup from genesis; its output is
    // removed when setup ends, so neither `run` nor a later recovery may
    // depend on it.
    let genesis_dir = match (request.base, request.genesis_bin) {
        (None, Some(genesis_bin)) => Some(run_genesis_tool(genesis_bin).await?),
        _ => None,
    };
    let genesis_state = genesis_dir.as_ref().map(|dir| dir.path().join("state"));

    let mut command = env.command()?;
    command
        .args(setup_args(
            request,
            env.data_dir,
            env.eth_rpc_url,
            env.rollups.app_address(),
            (submitter_key.as_str(), submitter_address),
            genesis_state.as_deref(),
        ))
        // Unknown chains require an explicit setup-time source. Normal Anvil
        // e2e pins zero; the dedicated fee test overrides it.
        .env(
            "CARTESI_SEQUENCER_FEE_ORACLE_FIXED_LOG_GAS_PRICE",
            request
                .fee_oracle_fixed_log_gas_price
                .unwrap_or(0)
                .to_string(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let timeout = if request.base.is_some() {
        BASE_SETUP_TIMEOUT
    } else if mine_l1_during_setup {
        FLUSHING_SETUP_TIMEOUT
    } else {
        GENESIS_SETUP_TIMEOUT
    };

    // Dropping the output future on timeout kills the child (`kill_on_drop`).
    let output = command.output();
    let output = if mine_l1_during_setup {
        let miner = mine_forever(env.rollups);
        tokio::select! {
            output = tokio::time::timeout(timeout, output) => output,
            _ = miner => unreachable!("the setup miner loops forever; only setup resolves"),
        }
    } else {
        tokio::time::timeout(timeout, output).await
    };
    let output = output
        .map_err(|_| {
            io_other(format!(
                "sequencer `setup` timed out after {timeout:?} (see {})",
                log_path.display()
            ))
        })?
        .map_err(|err| {
            io_other(format!(
                "failed to run `setup` for '{}': {err}",
                env.bin.display()
            ))
        })?;

    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    // Tracing writes to stdout; stderr holds what escaped it, such as clap's
    // usage errors.
    log.write_all(&output.stdout)?;
    log.write_all(&output.stderr)?;
    if let Some(dir) = genesis_dir {
        dir.close()
            .map_err(|err| io_other(format!("remove initial genesis after setup: {err}")))?;
    }
    Ok(SetupOutcome {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

/// A setup that a spawn depends on must succeed.
fn require_setup_success(outcome: SetupOutcome, log_path: &Path) -> HarnessResult<()> {
    if outcome.success() {
        return Ok(());
    }
    Err(io_other(format!(
        "sequencer `setup` failed: status={} (see {})",
        outcome.status,
        log_path.display()
    ))
    .into())
}

/// Write the C host's genesis state into a new temporary directory.
async fn run_genesis_tool(genesis_bin: &Path) -> HarnessResult<TempDir> {
    let dir = TempDir::new()?;
    let mut command = Command::new(genesis_bin);
    command
        .kill_on_drop(true)
        .arg(dir.path().join("state"))
        .arg("devnet");
    let output = tokio::time::timeout(DEFAULT_SEQUENCER_START_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            io_other(format!(
                "genesis tool '{}' timed out",
                genesis_bin.display()
            ))
        })?
        .map_err(|err| {
            io_other(format!(
                "failed to run genesis tool '{}': {err}",
                genesis_bin.display()
            ))
        })?;
    if !output.status.success() {
        return Err(io_other(format!(
            "genesis tool '{}' failed: status={}: {}",
            genesis_bin.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(dir)
}

/// Mine one block per [`SETUP_L1_MINE_INTERVAL`] until dropped, as a live
/// chain advances while setup flushes. Transient RPC hiccups are non-fatal —
/// the next tick retries; a genuine mining failure shows as the caller's
/// timeout.
async fn mine_forever(rollups: &DevnetRollupsStack) {
    loop {
        tokio::time::sleep(SETUP_L1_MINE_INTERVAL).await;
        let _ = rollups.mine_l1_block_at_wall_clock().await;
    }
}

/// Start `run` on a set-up data directory and wait for its public listener,
/// or for the process to exit first. Its output is appended to `log_path`.
async fn spawn_run_process(
    env: &ChildEnv<'_>,
    log_path: &Path,
    max_batch_open_seconds: Option<u64>,
) -> HarnessResult<RunProcess> {
    let (endpoint, http_addr) = build_local_endpoint()?;
    let (internal_endpoint, internal_http_addr) = build_local_endpoint()?;
    let (submitter_key, _) = devnet_submitter()?;

    let run_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let run_log_err = run_log.try_clone()?;
    let mut run_cmd = env.command()?;
    run_cmd
        .arg("run")
        .arg("--http-addr")
        .arg(http_addr)
        .arg("--internal-http-addr")
        .arg(internal_http_addr)
        .arg("--data-dir")
        .arg(env.data_dir)
        .arg("--eth-rpc-url")
        .arg(env.eth_rpc_url)
        .arg("--batch-submitter-private-key")
        .arg(&submitter_key)
        .args(timing_args());
    if let Some(secs) = max_batch_open_seconds {
        run_cmd
            .arg("--max-batch-open-seconds")
            .arg(secs.to_string());
    }
    run_cmd
        .stdout(Stdio::from(run_log))
        .stderr(Stdio::from(run_log_err));
    let mut child = run_cmd.spawn().map_err(|err| {
        io_other(format!(
            "failed to spawn sequencer binary '{}': {err}",
            env.bin.display()
        ))
    })?;

    match wait_for_http_readiness(
        endpoint.as_str(),
        &mut child,
        DEFAULT_SEQUENCER_START_TIMEOUT,
    )
    .await?
    {
        Readiness::Ready => Ok(RunProcess::Ready(SpawnedSequencerProcess {
            child,
            endpoint,
            internal_endpoint,
        })),
        Readiness::Exited(status) => Ok(RunProcess::Exited(child, status)),
    }
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

    use std::ffi::OsString;
    use std::path::Path;

    use alloy_primitives::Address;

    use super::{
        SetupRequest, candidate_libfaketime_dirs, find_libfaketime_in_dirs, logical_db_bytes_at,
        setup_args,
    };

    fn args_of(base: Option<&Path>, dry_run: bool, genesis_state: Option<&Path>) -> Vec<String> {
        let request = SetupRequest {
            chain_id: 31337,
            base,
            dry_run,
            genesis_bin: None,
            fee_oracle_fixed_log_gas_price: None,
        };
        setup_args(
            &request,
            Path::new("/data"),
            "http://127.0.0.1:8545",
            Address::repeat_byte(0xaa),
            ("0xkey", Address::repeat_byte(0x99)),
            genesis_state,
        )
        .into_iter()
        .map(|arg: OsString| arg.into_string().expect("utf-8 arg"))
        .collect()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let at = args.iter().position(|arg| arg == flag)?;
        args.get(at + 1).map(String::as_str)
    }

    /// Setup flushes the submitter's wallet, so it gets the key `run` gets;
    /// the genesis state is the host's option, before the subcommand.
    #[test]
    fn a_genesis_setup_takes_the_key_and_the_genesis_state() {
        let args = args_of(None, false, Some(Path::new("/genesis/state")));
        assert_eq!(&args[..3], ["--state-file", "/genesis/state", "setup"]);
        assert_eq!(
            value_after(&args, "--batch-submitter-private-key"),
            Some("0xkey")
        );
        assert_eq!(value_after(&args, "--data-dir"), Some("/data"));
        assert!(!args.iter().any(|arg| arg == "--base" || arg == "--dry-run"));
    }

    /// A setup from a stored machine builds its application from the
    /// machine's state, so the genesis state is not passed.
    #[test]
    fn a_base_setup_names_the_machine_and_no_genesis_state() {
        let args = args_of(
            Some(Path::new("/machines/base")),
            false,
            Some(Path::new("/genesis/state")),
        );
        assert_eq!(args[0], "setup");
        assert_eq!(value_after(&args, "--base"), Some("/machines/base"));
        assert!(
            args.iter()
                .any(|arg| arg == "--batch-submitter-private-key")
        );
        assert!(!args.iter().any(|arg| arg == "--state-file"));
    }

    /// The dry run flushes nothing, so it never sees the key.
    #[test]
    fn a_dry_run_gets_no_key() {
        let args = args_of(Some(Path::new("/machines/base")), true, None);
        assert!(args.iter().any(|arg| arg == "--dry-run"));
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("private-key") || arg == "0xkey")
        );
    }

    /// The logical size counts pages still in the WAL and drops freed pages,
    /// so it falls after a DELETE while the file, never vacuumed, does not.
    #[test]
    fn logical_db_size_counts_wal_pages_and_excludes_free_pages() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let path = temp.path().join("logical.db");
        let conn = rusqlite::Connection::open(&path).expect("open DB");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("WAL mode");
        conn.execute_batch(
            "CREATE TABLE t(b BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
             INSERT INTO t SELECT zeroblob(1000) FROM n;",
        )
        .expect("fill table");

        // Under the autocheckpoint size, so the rows live only in the WAL.
        let in_wal = logical_db_bytes_at(&path).expect("logical size");
        assert!(in_wal >= 2000 * 1000, "WAL pages are counted: {in_wal}");
        assert!(
            fs::metadata(&path).expect("stat").len() < in_wal,
            "the main file does not hold the rows yet"
        );

        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");
        let logical = logical_db_bytes_at(&path).expect("logical size");
        let file = fs::metadata(&path).expect("stat").len();

        conn.execute_batch("DELETE FROM t; PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("delete and checkpoint");
        let logical_after = logical_db_bytes_at(&path).expect("logical size");
        assert!(
            logical_after < logical / 4,
            "freed pages are excluded: {logical_after} vs {logical}"
        );
        assert!(
            fs::metadata(&path).expect("stat").len() >= file,
            "the file does not shrink without VACUUM"
        );
    }

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
