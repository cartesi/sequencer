// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The `setup` command: start a new era in an empty data directory from a
//! base, genesis or a stored canonical machine. It reads the base, flushes the
//! submitter's wallet to pin the stop block, syncs L1 through it, checks for
//! an escape and checks the base's export, rebuilds and executes the history,
//! and publishes the era (`docs/recovery/cockroach.md`, "What setup does").
//! Every L1 check that can fail retryably runs before anything is written, so
//! a retry finds the directory empty.

use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use sequencer_core::application::{AppError, Application};
use sequencer_core::history::L2TxCount;
use sequencer_core::scheduler::SchedulerConfig;

pub(crate) mod base;
#[cfg(test)]
mod chain_tests;
pub(crate) mod fill;
pub(crate) mod rebuild;

use super::validate_rpc_chain_id;
use crate::commands::config::{FeeOracleMode, SetupConfig};
use crate::commands::error::{
    BootstrapError, CommandError, IdentityError, SetupError, SetupRefuse,
};
use crate::l1::flusher::MempoolFlusher;
use crate::l1::reader::{InputReader, InputReaderConfig, InputReaderError, sample_safe_block};
use crate::runtime::process_lock::ProcessLock;
use crate::storage::{DeploymentIdentity, FeeOracleIdentity, LifecycleCommand, NewEra, Storage};
use base::{Base, ReadMachineExport};

/// The process lock's anchor file. It survives every command, so a directory
/// holding only it is still empty: an exit 20 before any write leaves it.
const LOCK_FILE_NAME: &str = "sequencer.lock";

pub async fn setup<A, F>(config: SetupConfig, genesis_app: F) -> Result<(), CommandError>
where
    A: Application + 'static,
    F: FnOnce() -> Result<A, AppError>,
{
    let summary = run_setup(config, genesis_app, base::read_stored_machine()).await?;
    println!("{summary}");
    Ok(())
}

/// [`setup`] with the machine reader as a parameter, returning what it
/// prints. Tests supply the base's export and state bytes through it.
pub(crate) async fn run_setup<A, F>(
    config: SetupConfig,
    genesis_app: F,
    read_export: ReadMachineExport,
) -> Result<SetupSummary, CommandError>
where
    A: Application + 'static,
    F: FnOnce() -> Result<A, AppError>,
{
    if config.dry_run {
        return dry_run(&config, genesis_app, read_export).await;
    }
    let key = config.resolve_private_key()?;

    // Refuse a directory that holds anything before touching it, then once
    // more under the lock, which another setup may have raced to.
    require_empty_data_dir(&config.data_dir)?;
    std::fs::create_dir_all(&config.data_dir)
        .map_err(|err| super::io_error_at("data dir", &config.data_dir, err))?;
    let process_lock = ProcessLock::acquire(&config.data_dir)?;
    require_empty_data_dir(&config.data_dir)?;

    let db_path = config.db_path();
    let result = setup_locked(&config, key, genesis_app, read_export, process_lock).await;
    // A failure before the first write leaves no database to record into,
    // and creating one would make the directory non-empty.
    if let Err(error) = &result
        && Path::new(&db_path).exists()
    {
        crate::commands::record_terminal_error_best_effort(
            &db_path,
            LifecycleCommand::Setup,
            error,
        );
    }
    result
}

async fn setup_locked<A, F>(
    config: &SetupConfig,
    key: crate::l1::SubmitterKey,
    genesis_app: F,
    read_export: ReadMachineExport,
    process_lock: ProcessLock,
) -> Result<SetupSummary, CommandError>
where
    A: Application + 'static,
    F: FnOnce() -> Result<A, AppError>,
{
    let db_path = config.db_path();

    // ── Before anything is written: every L1 check that can fail retryably.
    let mut input_reader = discover(config, &db_path, process_lock.clone()).await?;
    let fee_oracle = prepare_fee_oracle(config).await?;
    let identity = DeploymentIdentity {
        chain_id: config.chain_id,
        app_address: config.app_address,
        input_box_address: input_reader.input_box_address(),
        app_deployment_block: input_reader.app_deployment_block(),
        batch_submitter_address: config.batch_submitter_address,
        max_direct_payload_bytes: A::scheduler_limits().max_direct_payload_bytes,
        max_wait_blocks: A::scheduler_limits().max_wait_blocks,
        fee_oracle: fee_oracle.identity(),
    };
    // The flush settles the pinned account, so the key must sign for it.
    let key = super::verify_submitter_key(key, &identity)?;
    let providers = crate::l1::provider::create_verified_keyed_providers(
        &config.rpc.eth_rpc_url,
        key.expose_secret(),
        config.chain_id,
        config.rpc.allow_insecure_rpc,
    )
    .await?;
    let submitter = identity.batch_submitter_address;
    let scheduler_config = SchedulerConfig::for_application::<A>(submitter);

    // ── Step 1: the base. Its state bytes live in a scratch directory outside
    // the data directory, so a setup killed while it loads a large machine
    // leaves the data directory empty for the retry.
    let scratch = ScratchDir::create(&std::env::temp_dir())?;
    let base = base::load_base(
        config.base.as_deref(),
        genesis_app,
        read_export,
        scheduler_config,
        scratch.path(),
    )
    .await?;
    tracing::info!(
        base = ?config.base,
        l1_input_count = base.export.l1_input_count,
        l2_tx_count = base.export.l2_tx_count,
        "setup read its base"
    );

    // ── Writes begin: from here a failure leaves a directory to discard.
    let mut storage = Storage::initialize_for_command(&db_path, LifecycleCommand::Setup)?;
    storage.pin_deployment_identity(identity)?;
    match fee_oracle {
        PreparedFeeOracle::Fixed { log_gas_price } => storage.set_log_gas_price(log_gas_price)?,
        PreparedFeeOracle::Uniswap {
            provider, token, ..
        } => {
            crate::l1::fee_oracle::persist_first_price(
                db_path.clone(),
                provider,
                token,
                process_lock.clone(),
            )
            .await?
        }
    }

    // ── Step 2: flush, then pin the stop block at a safe block after it.
    let watermark = storage.wallet_nonce_watermark()?;
    let stop_block = MempoolFlusher::flush_to_safe(
        providers,
        submitter,
        config.timing.seconds_per_block,
        db_path.clone(),
        watermark,
    )
    .await?;
    tracing::info!(stop_block, "setup flushed the submitter's wallet");

    // ── Step 3: sync through the stop block. Before the era exists there
    // is no stop block or anchor to judge batches by, so storage leaves the
    // acceptance frontier empty; `run`'s first sync populates it.
    input_reader
        .sync_to_current_safe_head()
        .await
        .map_err(BootstrapError::SetupInputReader)?;
    let synced_safe_block = storage
        .current_safe_block()?
        .expect("a completed sync persists its safe head");
    let wallet = read_submitter_wallet_nonces(
        &config.rpc.eth_rpc_url,
        submitter,
        synced_safe_block,
        config.rpc.allow_insecure_rpc,
    )
    .await?;
    let summary = finish_setup(
        &mut storage,
        base,
        scheduler_config,
        Synced {
            stop_block,
            synced_safe_block,
            wallet,
        },
        Path::new(&config.data_dir),
    )?;
    tracing::info!(
        data_dir = %config.data_dir,
        chain_id = identity.chain_id,
        app_address = %identity.app_address,
        input_box_address = %identity.input_box_address,
        app_deployment_block = identity.app_deployment_block,
        batch_submitter_address = %identity.batch_submitter_address,
        "setup complete"
    );
    drop(scratch);
    Ok(summary)
}

/// Where step 3 left setup: the stop block the flush pinned, the safe head
/// the sync reached, and the submitter's `(pending, safe)` wallet nonces read
/// at that head.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Synced {
    pub(crate) stop_block: u64,
    pub(crate) synced_safe_block: u64,
    pub(crate) wallet: (u64, u64),
}

/// The rest of step 3 and steps 4 to 7, after the sync: the sync must reach
/// the stop block and the escape check must pass before the history is
/// rebuilt, executed and published with its dumps under `data_dir`.
pub(crate) fn finish_setup<A: Application>(
    storage: &mut Storage,
    base: Base<A>,
    scheduler_config: SchedulerConfig,
    synced: Synced,
    data_dir: &Path,
) -> Result<SetupSummary, CommandError> {
    let Synced {
        stop_block,
        synced_safe_block,
        wallet,
    } = synced;
    if synced_safe_block < stop_block {
        return Err(SetupError::SyncBehindStopBlock {
            synced_safe_block,
            stop_block,
        }
        .into());
    }
    check_escape(
        storage,
        scheduler_config.sequencer_address,
        stop_block,
        wallet,
    )?;

    let dumps_dir = data_dir.join("dumps");
    std::fs::create_dir_all(&dumps_dir)
        .map_err(|err| super::io_error_at("dumps dir", &dumps_dir, err))?;
    rebuild_and_publish(
        storage,
        base,
        scheduler_config,
        stop_block,
        &dumps_dir,
        wallet,
    )
}

/// Steps 4 to 7 over an input table synced through `stop_block`: check the
/// export, rebuild and execute the history, and publish the era.
pub(crate) fn rebuild_and_publish<A: Application>(
    storage: &mut Storage,
    base: Base<A>,
    scheduler_config: SchedulerConfig,
    stop_block: u64,
    dumps_dir: &Path,
    (pending_wallet_nonce, safe_wallet_nonce): (u64, u64),
) -> Result<SetupSummary, CommandError> {
    let Base { export, mut app } = base;
    let inputs_through_stop = storage.safe_input_count_through_block(stop_block)?;
    rebuild::check_export(storage, &export, inputs_through_stop, stop_block)?;
    let submitter_inputs_past_base = rebuild::count_submitter_inputs(
        storage,
        scheduler_config.sequencer_address,
        export.l1_input_count,
        inputs_through_stop,
    )?;
    let scheduler = rebuild::rebuild_history(
        storage,
        &mut app,
        scheduler_config,
        &export,
        inputs_through_stop,
        stop_block,
    )?;
    // The rebuild fed exactly the inputs through the stop block, so the era's
    // first queued input and anchor are the scheduler's there.
    assert_eq!(
        scheduler.l1_input_count, inputs_through_stop,
        "the rebuilt scheduler consumed every input through the stop block, and no other"
    );
    let era = fill::publish(
        &mut app,
        storage,
        dumps_dir,
        L2TxCount::new(export.l2_tx_count),
        stop_block,
        scheduler,
    )?;
    Ok(SetupSummary {
        base_l1_input_count: export.l1_input_count,
        base_l2_tx_count: export.l2_tx_count,
        submitter_inputs_past_base,
        unsettled_wallet_txs: pending_wallet_nonce.saturating_sub(safe_wallet_nonce),
        inputs_to_replay: inputs_through_stop - export.l1_input_count,
        through_block: stop_block,
        era: Some(era),
    })
}

/// The dry run: read the base, sync into a scratch directory with its own
/// lock, and check the export there. No flush and no key; the data directory
/// is never touched. The escape facts are findings here, not refusals.
async fn dry_run<A, F>(
    config: &SetupConfig,
    genesis_app: F,
    read_export: ReadMachineExport,
) -> Result<SetupSummary, CommandError>
where
    A: Application + 'static,
    F: FnOnce() -> Result<A, AppError>,
{
    let scratch = ScratchDir::create(&std::env::temp_dir())?;
    let scratch_dir = scratch.path().to_string_lossy().into_owned();
    let process_lock = ProcessLock::acquire(&scratch_dir)?;
    let db_path = crate::commands::config::db_path_in(&scratch_dir);

    let mut input_reader = discover(config, &db_path, process_lock).await?;
    let fee_oracle = resolve_fee_oracle(config)?;
    let identity = DeploymentIdentity {
        chain_id: config.chain_id,
        app_address: config.app_address,
        input_box_address: input_reader.input_box_address(),
        app_deployment_block: input_reader.app_deployment_block(),
        batch_submitter_address: config.batch_submitter_address,
        max_direct_payload_bytes: A::scheduler_limits().max_direct_payload_bytes,
        max_wait_blocks: A::scheduler_limits().max_wait_blocks,
        fee_oracle: fee_oracle_identity(&fee_oracle),
    };
    let submitter = identity.batch_submitter_address;
    let scheduler_config = SchedulerConfig::for_application::<A>(submitter);
    let base = base::load_base(
        config.base.as_deref(),
        genesis_app,
        read_export,
        scheduler_config,
        scratch.path(),
    )
    .await?;

    let mut storage = Storage::initialize_for_command(&db_path, LifecycleCommand::Setup)?;
    storage.pin_deployment_identity(identity)?;
    input_reader
        .sync_to_current_safe_head()
        .await
        .map_err(BootstrapError::SetupInputReader)?;
    let safe_block = storage
        .current_safe_block()?
        .expect("a completed sync persists its safe head");
    let (pending, safe) = read_submitter_wallet_nonces(
        &config.rpc.eth_rpc_url,
        submitter,
        safe_block,
        config.rpc.allow_insecure_rpc,
    )
    .await?;
    let synced_inputs = storage.safe_input_count_through_block(safe_block)?;
    rebuild::check_export(&mut storage, &base.export, synced_inputs, safe_block)?;
    // The scheduler's own resume checks are part of the export check. Feeding
    // never moves the clock past the block of the input that names it, so a
    // resumed clock past the safe head is one setup would refuse too.
    let resumed = rebuild::resume_from_export(&mut storage, scheduler_config, &base.export)?;
    rebuild::check_clock(&resumed.state(), safe_block)?;
    let submitter_inputs_past_base = rebuild::count_submitter_inputs(
        &mut storage,
        submitter,
        base.export.l1_input_count,
        synced_inputs,
    )?;
    drop(storage);
    drop(scratch);
    Ok(SetupSummary {
        base_l1_input_count: base.export.l1_input_count,
        base_l2_tx_count: base.export.l2_tx_count,
        submitter_inputs_past_base,
        unsettled_wallet_txs: pending.saturating_sub(safe),
        inputs_to_replay: synced_inputs - base.export.l1_input_count,
        through_block: safe_block,
        era: None,
    })
}

/// What setup and its dry run report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupSummary {
    pub(crate) base_l1_input_count: u64,
    pub(crate) base_l2_tx_count: u64,
    /// The submitter's inputs past the base: the old instance's batches.
    pub(crate) submitter_inputs_past_base: u64,
    /// The submitter's transactions not yet safe.
    pub(crate) unsettled_wallet_txs: u64,
    /// How many inputs past the base the rebuild feeds the scheduler.
    pub(crate) inputs_to_replay: u64,
    /// The stop block; for a dry run, the safe head it synced to.
    pub(crate) through_block: u64,
    /// The published era; `None` for a dry run.
    pub(crate) era: Option<NewEra>,
}

impl SetupSummary {
    /// Recovery is needed exactly when L1 shows the old submitter's activity
    /// past the base; otherwise the rebuild only moves directs.
    fn needs_recovery(&self) -> bool {
        self.submitter_inputs_past_base > 0 || self.unsettled_wallet_txs > 0
    }
}

impl std::fmt::Display for SetupSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "base: {} L1 inputs, L2 tx count {}",
            self.base_l1_input_count, self.base_l2_tx_count
        )?;
        writeln!(
            f,
            "submitter inputs past the base: {}",
            self.submitter_inputs_past_base
        )?;
        writeln!(
            f,
            "unsettled submitter transactions: {}",
            self.unsettled_wallet_txs
        )?;
        writeln!(
            f,
            "inputs the rebuild replays: {} (through block {})",
            self.inputs_to_replay, self.through_block
        )?;
        if self.needs_recovery() {
            write!(
                f,
                "the rebuild re-judges the old instance's batches past the base"
            )?;
        } else {
            write!(
                f,
                "no submitter activity past the base: the rebuild only moves directs"
            )?;
        }
        if let Some(era) = &self.era {
            write!(
                f,
                "\nstop block: {}\nL2 tx count at its end: {}",
                era.stop_block,
                era.rebuilt_end.get()
            )?;
        }
        Ok(())
    }
}

/// Setup's L1 discovery, which writes nothing: the chain id, then a safe head
/// sampled before the deployment block is read, then that block safe.
async fn discover(
    config: &SetupConfig,
    db_path: &str,
    process_lock: ProcessLock,
) -> Result<InputReader, CommandError> {
    // The chain check comes first, so a wrong-chain endpoint is reported as
    // such rather than as a missing contract.
    validate_rpc_chain_id(
        &config.rpc.eth_rpc_url,
        config.chain_id,
        config.rpc.allow_insecure_rpc,
    )
    .await?;
    let input_reader_config = InputReaderConfig {
        rpc_url: config.rpc.eth_rpc_url.clone(),
        allow_insecure_rpc: config.rpc.allow_insecure_rpc,
        app_address: config.app_address,
        poll_interval: super::INPUT_READER_POLL_INTERVAL,
        // `validate_rpc_chain_id` above checked this against the live RPC; the
        // reader's own check then verifies every sync provider serves it too.
        expected_chain_id: config.chain_id,
    };
    // Safe is sampled before discovery reads the deployment block at latest: a
    // later sample could approve a deployment that a reorg in between moved.
    let discovered = async {
        let safe_block =
            sample_safe_block(&config.rpc.eth_rpc_url, config.rpc.allow_insecure_rpc).await?;
        let reader =
            InputReader::new(db_path.to_owned(), input_reader_config, process_lock).await?;
        Ok::<_, InputReaderError>((safe_block, reader))
    }
    .await;
    let (safe_before_discovery, input_reader) = match discovered {
        Ok(discovered) => discovered,
        Err(InputReaderError::Provider(e)) => {
            // Retryable: the operator brings L1 up and re-runs `setup`.
            tracing::warn!(error = %e, "L1 unreachable during setup — cannot discover identity");
            return Err(IdentityError::FirstBootRequiresL1.into());
        }
        Err(source) => return Err(BootstrapError::SetupInputReader(source).into()),
    };
    require_deployment_safe(input_reader.app_deployment_block(), safe_before_discovery)?;
    Ok(input_reader)
}

/// The fee source, resolved and, for Uniswap, connected and validated, and a
/// fixed price held to the fee floor, before anything is written. `run` later
/// reads only the pinned tuple.
enum PreparedFeeOracle {
    Fixed {
        log_gas_price: i16,
    },
    Uniswap {
        identity: FeeOracleIdentity,
        provider: alloy::providers::DynProvider,
        token: crate::l1::fee_oracle::UniswapV3PriceSource,
    },
}

impl PreparedFeeOracle {
    fn identity(&self) -> FeeOracleIdentity {
        match self {
            Self::Fixed { log_gas_price } => FeeOracleIdentity::Fixed {
                log_gas_price: *log_gas_price,
            },
            Self::Uniswap { identity, .. } => *identity,
        }
    }
}

fn resolve_fee_oracle(config: &SetupConfig) -> Result<FeeOracleMode, CommandError> {
    config
        .fee_oracle
        .resolve(config.chain_id)
        .map_err(|message| BootstrapError::FeeOracleMisconfig { message }.into())
}

fn fee_oracle_identity(mode: &FeeOracleMode) -> FeeOracleIdentity {
    match mode {
        FeeOracleMode::Fixed { log_gas_price } => FeeOracleIdentity::Fixed {
            log_gas_price: *log_gas_price,
        },
        FeeOracleMode::Uniswap(uniswap) => FeeOracleIdentity::Uniswap {
            weth: uniswap.weth,
            fee_token: uniswap.fee_token,
            pool: uniswap.pool,
            twap_window_secs: uniswap.twap_window_secs,
        },
    }
}

async fn prepare_fee_oracle(config: &SetupConfig) -> Result<PreparedFeeOracle, CommandError> {
    let mode = resolve_fee_oracle(config)?;
    let identity = fee_oracle_identity(&mode);
    Ok(match mode {
        FeeOracleMode::Fixed { log_gas_price } => {
            check_fixed_log_gas_price(log_gas_price)?;
            PreparedFeeOracle::Fixed { log_gas_price }
        }
        FeeOracleMode::Uniswap(uniswap) => {
            let (provider, token) = crate::l1::fee_oracle::connect_uniswap(
                &config.rpc.eth_rpc_url,
                config.rpc.allow_insecure_rpc,
                uniswap,
            )
            .await?;
            PreparedFeeOracle::Uniswap {
                identity,
                provider,
                token,
            }
        }
    })
}

/// Refuse a fixed price the fee-floor CHECK would reject, before it pins an
/// identity whose first price write could never succeed. The floor is read
/// from the schema alone, so the data directory is still untouched.
fn check_fixed_log_gas_price(log_gas_price: i16) -> Result<(), CommandError> {
    let floor = Storage::schema_only()?.log_gas_price_floor()?;
    if log_gas_price < floor {
        return Err(BootstrapError::FeeOracleMisconfig {
            message: format!(
                "fixed log gas price {log_gas_price} is below {floor}, which would make \
                 the recommended fee negative"
            ),
        }
        .into());
    }
    Ok(())
}

/// Setup runs only in an empty data directory: absent, empty, or holding only
/// the lock file. Nothing else there can be trusted to fit this run.
fn require_empty_data_dir(data_dir: &str) -> Result<(), CommandError> {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(super::io_error_at("data dir", data_dir, err)),
    };
    for entry in entries {
        let entry = entry.map_err(|err| super::io_error_at("data dir", data_dir, err))?;
        if entry.file_name() != LOCK_FILE_NAME {
            return Err(SetupError::DataDirNotEmpty {
                path: data_dir.to_owned(),
                entry: entry.file_name().to_string_lossy().into_owned(),
            }
            .into());
        }
    }
    Ok(())
}

/// A directory removed when dropped, for the base's state bytes and the dry
/// run's database.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn create(parent: &Path) -> Result<Self, CommandError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos();
        let path = parent.join(format!(
            "sequencer-setup-scratch-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&path)
            .map_err(|err| super::io_error_at("setup scratch dir", &path, err))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(path = %self.0.display(), %error, "could not remove setup's scratch directory");
        }
    }
}

/// Setup's escape check, after the flush and the sync through the stop
/// block. An unsettled wallet refuses before the scan: the old instance's
/// unsafe batches are not in the synced table yet.
fn check_escape(
    storage: &mut Storage,
    batch_submitter: Address,
    stop_block: u64,
    (pending_wallet_nonce, safe_wallet_nonce): (u64, u64),
) -> Result<(), CommandError> {
    if pending_wallet_nonce > safe_wallet_nonce {
        return Err(SetupRefuse::WalletUnsettledAfterFlush {
            pending: pending_wallet_nonce,
            safe: safe_wallet_nonce,
        }
        .into());
    }
    // The scan reads the reader-synced table, which carries the reader's
    // completeness witness; see `first_batch_submitter_input_after_block`.
    if let Some(found) =
        storage.first_batch_submitter_input_after_block(batch_submitter, stop_block)?
    {
        return Err(SetupRefuse::SubmitterInputPastStop {
            stop_block,
            found_block: found.block_number,
            safe_input_index: found.safe_input_index,
        }
        .into());
    }
    Ok(())
}

/// The submitter's wallet nonce at the pending tag and at `safe_block`, the
/// safe head the sync persisted, so the settled check agrees with the table
/// the escape scan reads. Returns `(pending, safe)`.
async fn read_submitter_wallet_nonces(
    eth_rpc_url: &str,
    batch_submitter: Address,
    safe_block: u64,
    allow_insecure: bool,
) -> Result<(u64, u64), BootstrapError> {
    use alloy::providers::Provider;
    use alloy::rpc::types::BlockNumberOrTag;

    let provider = crate::l1::provider::create_read_provider(eth_rpc_url, allow_insecure)
        .map_err(|message| BootstrapError::WalletNonceRead { message })?;
    let read = |tag: BlockNumberOrTag| {
        let provider = &provider;
        async move {
            provider
                .get_transaction_count(batch_submitter)
                .block_id(tag.into())
                .await
                .map_err(|e| BootstrapError::WalletNonceRead {
                    message: crate::l1::provider::error_chain(&e),
                })
        }
    };
    let pending = read(BlockNumberOrTag::Pending).await?;
    let safe = read(BlockNumberOrTag::Number(safe_block)).await?;
    Ok((pending, safe))
}

/// Setup pins the application's deployment block, the reader's scan floor, only
/// once it is at or below a safe head sampled before the block was read. Until
/// then a reorg could move the deployment earlier, and the reader would advance
/// its safe head over blocks below the pinned floor without reading the inputs
/// they hold, which the scheduler then drains: divergence. Assumes safe blocks
/// are permanent and that `block.number` is the RPC block number
/// (`docs/threat-model/README.md`, L1 chain semantics).
fn require_deployment_safe(
    app_deployment_block: u64,
    safe_block: u64,
) -> Result<(), IdentityError> {
    if app_deployment_block > safe_block {
        return Err(IdentityError::DeploymentNotYetSafe {
            app_deployment_block,
            safe_block,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StoredSafeInput;
    use crate::storage::test_helpers::SENDER_A;
    use crate::test_support::temp_db;

    const ANVIL_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    pub(super) fn setup_config(data_dir: &str, rpc_url: &str, extra: &[&str]) -> SetupConfig {
        use crate::harness::{Cli, Command};
        use clap::Parser;
        let mut args = vec![
            "sequencer",
            "setup",
            "--data-dir",
            data_dir,
            "--eth-rpc-url",
            rpc_url,
            "--chain-id",
            "31337",
            "--app-address",
            "0x1111111111111111111111111111111111111111",
            "--batch-submitter-address",
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
            "--batch-submitter-private-key",
            ANVIL_KEY,
        ];
        args.extend_from_slice(extra);
        let Command::Setup(config) = Cli::try_parse_from(args).expect("parse setup").command else {
            panic!("expected setup subcommand");
        };
        *config
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn setup_pins_only_a_safe_deployment() {
        assert!(matches!(
            require_deployment_safe(10, 9),
            Err(IdentityError::DeploymentNotYetSafe {
                app_deployment_block: 10,
                safe_block: 9
            })
        ));
        require_deployment_safe(10, 10).expect("a deployment at the safe head is final");
        require_deployment_safe(10, 11).expect("a deployment below the safe head is final");
    }

    /// The emptiness rule skips the lock file by name, so the name must be
    /// the one the lock creates.
    #[test]
    fn only_the_lock_file_counts_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        require_empty_data_dir(&dir.path().join("absent").to_string_lossy()).unwrap();
        require_empty_data_dir(path).unwrap();
        drop(ProcessLock::acquire(path).unwrap());
        assert_eq!(entries(dir.path()), vec![LOCK_FILE_NAME.to_owned()]);
        require_empty_data_dir(path).unwrap();
        std::fs::create_dir(dir.path().join("dumps")).unwrap();
        let err = require_empty_data_dir(path).expect_err("a directory with a dump is not empty");
        assert!(
            matches!(
                &err,
                CommandError::Bootstrap(BootstrapError::Setup(error))
                    if matches!(&**error, SetupError::DataDirNotEmpty { entry, .. } if entry == "dumps")
            ),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
    }

    /// A completed (or failed) setup's directory is refused with exit 30
    /// before L1, the key's account or the genesis factory are touched, and
    /// it is left exactly as it was.
    #[tokio::test]
    async fn a_non_empty_directory_is_refused_untouched() {
        let data_dir = tempfile::tempdir().unwrap();
        let db = data_dir.path().join("sequencer.db");
        let mut storage =
            Storage::initialize_for_command(db.to_str().unwrap(), LifecycleCommand::Setup).unwrap();
        storage
            .publish_era(&data_dir.path().join("baseline"), &NewEra::EMPTY_GENESIS)
            .unwrap();
        drop(storage);
        let before = entries(data_dir.path());
        let modified = std::fs::metadata(&db).unwrap().modified().unwrap();

        let err = run_setup(
            setup_config(data_dir.path().to_str().unwrap(), "http://127.0.0.1:1", &[]),
            || -> Result<crate::test_support::StubApp, AppError> {
                unreachable!("a refused directory never builds the genesis app")
            },
            Box::new(|_, _| unreachable!("a refused directory never reads a machine")),
        )
        .await
        .expect_err("a non-empty directory is refused");
        assert!(
            matches!(
                &err,
                CommandError::Bootstrap(BootstrapError::Setup(error))
                    if matches!(**error, SetupError::DataDirNotEmpty { .. })
            ),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
        assert_eq!(
            entries(data_dir.path()),
            before,
            "not even a lock file appears"
        );
        assert_eq!(
            std::fs::metadata(&db).unwrap().modified().unwrap(),
            modified
        );
        assert!(
            Storage::open_read_only(db.to_str().unwrap())
                .unwrap()
                .latest_terminal_fault()
                .unwrap()
                .is_none(),
            "the refusal records nothing in another setup's black box"
        );
    }

    /// Seed `safe_inputs` with one batch-submitter (`SENDER_A`) input per block
    /// in `blocks`, synced up to the max block.
    fn seed_submitter_inputs(storage: &mut Storage, blocks: &[u64]) {
        let inputs: Vec<StoredSafeInput> = blocks
            .iter()
            .map(|&block_number| StoredSafeInput {
                sender: SENDER_A,
                payload: vec![0x01],
                block_number,
            })
            .collect();
        let safe_block = blocks.iter().copied().max().unwrap_or(0);
        storage
            .append_safe_inputs(safe_block, &inputs, SENDER_A)
            .expect("seed safe inputs");
    }

    #[test]
    fn escape_check_passes_a_settled_wallet_with_nothing_past_the_stop_block() {
        let db = temp_db("escape-pass");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        seed_submitter_inputs(&mut storage, &[18]);
        // A batch at the stop block is part of the rebuilt history.
        check_escape(&mut storage, SENDER_A, 18, (5, 5)).unwrap();
    }

    #[test]
    fn escape_check_refuses_an_unsettled_wallet_before_scanning() {
        let db = temp_db("escape-unsettled");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        // The table is empty, so only the wallet can refuse.
        match check_escape(&mut storage, SENDER_A, 0, (14, 13)) {
            Err(
                error @ CommandError::Bootstrap(BootstrapError::SetupRefuse(
                    SetupRefuse::WalletUnsettledAfterFlush {
                        pending: 14,
                        safe: 13,
                    },
                )),
            ) => {
                assert_eq!(error.exit_code(), crate::commands::error::EXIT_SETUP_ESCAPE);
                assert!(error.to_string().contains("stop the old instance"));
            }
            other => panic!("expected WalletUnsettledAfterFlush, got {other:?}"),
        }
    }

    #[test]
    fn escape_check_refuses_a_submitter_input_past_the_stop_block() {
        let db = temp_db("escape-past-stop");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        seed_submitter_inputs(&mut storage, &[10, 18]);
        match check_escape(&mut storage, SENDER_A, 17, (1, 1)) {
            Err(CommandError::Bootstrap(BootstrapError::SetupRefuse(
                SetupRefuse::SubmitterInputPastStop {
                    stop_block: 17,
                    found_block: 18,
                    safe_input_index: 1,
                },
            ))) => {}
            other => panic!("expected SubmitterInputPastStop, got {other:?}"),
        }
    }

    /// A fixed price below the fee floor would pin an identity whose first
    /// price write the schema refuses; setup refuses it first.
    #[test]
    fn fixed_price_below_the_floor_is_refused() {
        let err =
            check_fixed_log_gas_price(-1357).expect_err("one below the floor must be refused");
        assert!(
            matches!(
                err,
                CommandError::Bootstrap(BootstrapError::FeeOracleMisconfig { .. })
            ),
            "{err:?}"
        );
        check_fixed_log_gas_price(-1356).expect("the floor itself is accepted");
    }

    #[test]
    fn summary_says_which_case_it_found() {
        let mut summary = SetupSummary {
            base_l1_input_count: 8,
            base_l2_tx_count: 7,
            submitter_inputs_past_base: 0,
            unsettled_wallet_txs: 0,
            inputs_to_replay: 3,
            through_block: 120,
            era: None,
        };
        assert!(summary.to_string().contains("only moves directs"));
        summary.unsettled_wallet_txs = 1;
        assert!(summary.to_string().contains("re-judges"));
        summary.era = Some(NewEra {
            stop_block: 130,
            rebuilt_end: L2TxCount::new(9),
            ..NewEra::EMPTY_GENESIS
        });
        let text = summary.to_string();
        assert!(text.contains("stop block: 130"), "{text}");
        assert!(text.contains("L2 tx count at its end: 9"), "{text}");
    }

    /// Setup's L1 discovery uses the startup polarity: a URL the client refuses
    /// re-fails identically, so it is terminal, and so is an endpoint that
    /// redirects the chain-id read; an unreachable RPC fails at the chain-id
    /// read (`ChainIdRpc`) and is retried, leaving the directory empty for the
    /// retry. The fee-source connect follows the runtime classification with
    /// setup's messages.
    #[tokio::test]
    async fn setup_l1_failures_exit_by_startup_phase() {
        use crate::commands::error::{EXIT_RESTART_TRANSIENT, EXIT_TERMINAL};

        async fn setup_with(rpc_url: &str) -> (CommandError, Vec<String>) {
            let data_dir = tempfile::tempdir().expect("data dir");
            let error = run_setup(
                setup_config(data_dir.path().to_str().expect("utf8 path"), rpc_url, &[]),
                || -> Result<crate::test_support::StubApp, AppError> {
                    unreachable!("setup fails before building the genesis app")
                },
                base::read_stored_machine(),
            )
            .await
            .expect_err("setup cannot discover L1");
            (error, entries(data_dir.path()))
        }

        let (malformed, _) = setup_with("not a url").await;
        assert!(
            matches!(
                malformed,
                CommandError::Bootstrap(BootstrapError::SetupInputReader(
                    InputReaderError::Bootstrap(_)
                ))
            ),
            "{malformed:?}"
        );
        assert_eq!(malformed.exit_code(), EXIT_TERMINAL);

        let (unreachable, left) = setup_with("http://127.0.0.1:1").await;
        assert_eq!(
            unreachable.exit_code(),
            EXIT_RESTART_TRANSIENT,
            "{unreachable:?}"
        );
        assert_eq!(
            left,
            vec![LOCK_FILE_NAME.to_owned()],
            "an exit 20 leaves the directory empty for the retry"
        );

        // Called directly: clap would read CARTESI_SEQUENCER_ALLOW_INSECURE_RPC
        // from the environment.
        let plaintext_remote =
            crate::commands::validate_rpc_chain_id("http://example.invalid:8545", 31337, false)
                .await
                .expect_err("plaintext to a remote host needs the opt-in");
        assert!(
            matches!(
                plaintext_remote,
                CommandError::Bootstrap(BootstrapError::SetupInputReader(
                    InputReaderError::Bootstrap(_)
                ))
            ),
            "{plaintext_remote:?}"
        );
        assert_eq!(plaintext_remote.exit_code(), EXIT_TERMINAL);

        // The client follows no redirect, so a retry would get the same 308.
        let endpoint = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind endpoint");
        let endpoint_address = endpoint.local_addr().expect("endpoint address");
        tokio::spawn(crate::test_support::answer_once(
            endpoint,
            "HTTP/1.1 308 Permanent Redirect\r\nLocation: http://127.0.0.1:1/\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
        ));
        let redirect = crate::commands::validate_rpc_chain_id(
            &format!("http://{endpoint_address}/"),
            31337,
            false,
        )
        .await
        .expect_err("a redirect is not followed");
        assert!(
            matches!(
                redirect,
                CommandError::Bootstrap(BootstrapError::RpcRedirect { status: 308 })
            ),
            "{redirect:?}"
        );
        assert_eq!(redirect.exit_code(), EXIT_TERMINAL);

        use crate::l1::fee_oracle::UniswapConfig;
        use crate::l1::fee_oracle::uniswap::{PriceSourceError, UniswapV3PriceSource};
        let uniswap = |twap_window_secs| UniswapConfig {
            chain_id: 31337,
            weth: alloy_primitives::Address::repeat_byte(1),
            fee_token: alloy_primitives::Address::repeat_byte(2),
            pool: alloy_primitives::Address::repeat_byte(3),
            twap_window_secs,
        };
        async fn connect_with(url: &str, uniswap: UniswapConfig) -> CommandError {
            match crate::l1::fee_oracle::connect_uniswap(url, false, uniswap).await {
                Ok(_) => panic!("no pool behind {url}"),
                Err(error) => error.into(),
            }
        }

        let bad_url = connect_with("not a url", uniswap(1_800)).await;
        let expected = crate::l1::provider::create_read_provider("not a url", false)
            .expect_err("the client refuses it");
        assert!(
            matches!(
                &bad_url,
                CommandError::Bootstrap(BootstrapError::FeeOracleMisconfig { message })
                    if *message == expected
            ),
            "{bad_url:?}"
        );
        assert_eq!(bad_url.exit_code(), EXIT_TERMINAL);

        let zero_window = connect_with("http://127.0.0.1:1", uniswap(0)).await;
        assert!(
            matches!(
                &zero_window,
                CommandError::Bootstrap(BootstrapError::FeeOracleMisconfig { message })
                    if *message == PriceSourceError::ZeroTwapWindow.to_string()
            ),
            "{zero_window:?}"
        );
        assert_eq!(zero_window.exit_code(), EXIT_TERMINAL);

        let unavailable = connect_with("http://127.0.0.1:1", uniswap(1_800)).await;
        let provider = crate::l1::provider::create_read_provider("http://127.0.0.1:1", false)
            .expect("provider construction is local");
        let Err(PriceSourceError::Provider(expected)) =
            UniswapV3PriceSource::connect(provider, uniswap(1_800)).await
        else {
            panic!("an unreachable RPC is a provider error");
        };
        // The provider's text, without the runtime variant's prefix.
        assert!(
            matches!(
                &unavailable,
                CommandError::Bootstrap(BootstrapError::FeeOracleTransient { message })
                    if *message == expected
            ),
            "{unavailable:?}"
        );
        assert_eq!(unavailable.exit_code(), EXIT_RESTART_TRANSIENT);
    }

    #[tokio::test]
    async fn a_genesis_application_must_start_at_zero() {
        let scratch = tempfile::tempdir().unwrap();
        let error = base::load_base(
            None,
            || Ok(crate::test_support::StubApp::with_progress(1)),
            base::read_stored_machine(),
            SchedulerConfig::for_application::<crate::test_support::StubApp>(SENDER_A),
            scratch.path(),
        )
        .await
        .err()
        .expect("a nonzero genesis is refused");
        assert!(
            matches!(error, CommandError::AppBootstrap(AppError::Internal { .. })),
            "{error:?}"
        );
        assert!(entries(scratch.path()).is_empty());
    }
}
