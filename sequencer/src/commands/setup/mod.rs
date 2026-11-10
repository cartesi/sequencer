// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The `setup` command: pin the deployment identity and fee source, sync L1 to
//! the safe head, and register the genesis baseline, committing
//! `setup_complete` last as its single completion point. Storage phases are
//! re-entry safe. `setup` reads L1 only and never signs.

use alloy_primitives::Address;
use sequencer_core::application::{AppError, Application};

pub(crate) mod fill;

use super::{ensure_deployment_identity, validate_rpc_chain_id};
use crate::commands::config::{FeeOracleMode, SetupConfig};
use crate::commands::error::{BootstrapError, CommandError, IdentityError, SetupRefuse};
use crate::l1::reader::{InputReader, InputReaderConfig, InputReaderError, sample_safe_block};
use crate::storage::{self, DeploymentIdentity, FeeOracleIdentity};

pub async fn setup<A, F>(config: SetupConfig, genesis_app: F) -> Result<(), CommandError>
where
    A: Application + 'static,
    F: FnOnce() -> Result<A, AppError>,
{
    std::fs::create_dir_all(&config.data_dir)
        .map_err(|err| super::io_error_at("data dir", &config.data_dir, err))?;
    // Exclusive process ownership before any read or mutation: setup rewrites
    // deployment state and must never run beside a live sequencer (or a
    // second setup) on the same data dir.
    let process_lock = crate::runtime::process_lock::ProcessLock::acquire(&config.data_dir)?;

    let db_path = config.db_path();
    config.timing.protocol_timing()?;
    let command = storage::LifecycleCommand::Setup;
    if let SetupAdmission::AlreadyComplete = admit_setup_lifecycle(&db_path, command)? {
        tracing::info!(data_dir = %config.data_dir, "setup already complete — nothing to do");
        return Ok(());
    }

    let result = setup_admitted(config, genesis_app, process_lock.clone()).await;
    settle_setup_lifecycle(&db_path, command, &result)?;
    result
}

async fn setup_admitted<A, F>(
    config: SetupConfig,
    genesis_app: F,
    process_lock: crate::runtime::process_lock::ProcessLock,
) -> Result<(), CommandError>
where
    A: Application + 'static,
    F: FnOnce() -> Result<A, AppError>,
{
    let db_path = config.db_path();
    let timing = config.timing.protocol_timing()?;

    // Do not create auxiliary data-dir entries before durable lifecycle
    // admission. The lock file itself is the unavoidable ownership anchor.
    let dumps_dir = std::path::Path::new(&config.data_dir).join("dumps");
    std::fs::create_dir_all(&dumps_dir)
        .map_err(|err| super::io_error_at("dumps dir", &dumps_dir, err))?;

    // ── L1 discovery (required) ──────────────────────────────
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
        let reader = InputReader::new(
            db_path.clone(),
            input_reader_config,
            config.batch_submitter_address,
            timing,
            process_lock.clone(),
        )
        .await?;
        Ok::<_, InputReaderError>((safe_block, reader))
    }
    .await;
    let (safe_before_discovery, mut input_reader) = match discovered {
        Ok(discovered) => discovered,
        Err(InputReaderError::Provider(e)) => {
            // First boot needs L1 to discover + pin identity. Retryable: the
            // operator brings L1 up and re-runs `setup`.
            tracing::warn!(error = %e, "L1 unreachable during setup — cannot discover identity");
            return Err(IdentityError::FirstBootRequiresL1.into());
        }
        Err(source) => return Err(BootstrapError::SetupInputReader(source).into()),
    };
    require_deployment_safe(input_reader.app_deployment_block(), safe_before_discovery)?;

    // Resolve and validate the complete fee source while setup still owns all
    // address-bearing configuration. `run` later reads only this pinned tuple.
    // Both modes write the first `log_gas_price` here so `run` can always assume
    // a real price exists (Uniswap Tip never samples the migration default).
    let fee_oracle = config
        .fee_oracle
        .resolve(config.chain_id)
        .map_err(|message| BootstrapError::FeeOracleMisconfig { message })?;
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
    let prepared_fee_oracle = match fee_oracle {
        FeeOracleMode::Fixed { log_gas_price } => {
            check_fixed_log_gas_price(&db_path, log_gas_price)?;
            PreparedFeeOracle::Fixed { log_gas_price }
        }
        FeeOracleMode::Uniswap(uniswap) => {
            // Setup requires L1: transient and misconfig both abort before
            // anything pins.
            let (provider, token) = crate::l1::fee_oracle::connect_uniswap(
                &config.rpc.eth_rpc_url,
                config.rpc.allow_insecure_rpc,
                uniswap,
            )
            .await?;
            PreparedFeeOracle::Uniswap {
                identity: FeeOracleIdentity::Uniswap {
                    weth: uniswap.weth,
                    fee_token: uniswap.fee_token,
                    pool: uniswap.pool,
                    twap_window_secs: uniswap.twap_window_secs,
                },
                provider,
                token,
            }
        }
    };
    let pinned_fee_oracle = match &prepared_fee_oracle {
        PreparedFeeOracle::Fixed { log_gas_price } => FeeOracleIdentity::Fixed {
            log_gas_price: *log_gas_price,
        },
        PreparedFeeOracle::Uniswap { identity, .. } => *identity,
    };

    // ── Pin identity ─────────────────────────────────────────
    // INVARIANT: identity is pinned (this step) BEFORE the initial sync
    // (next step). The sync is the first writer of `safe_inputs` /
    // `l1_safe_head`, which `has_persisted_deployment_state` keys on. Pinning
    // first means a crash mid-sync re-runs cleanly; reordering would make a
    // crashed setup look like `OrphanedState` (DB has state, no identity) on
    // the retry. See `ensure_deployment_identity` + docs/invariants.md.
    let identity = DeploymentIdentity {
        chain_id: config.chain_id,
        app_address: config.app_address,
        input_box_address: input_reader.input_box_address(),
        app_deployment_block: input_reader.app_deployment_block(),
        batch_submitter_address: config.batch_submitter_address,
        fee_oracle: pinned_fee_oracle,
    };
    ensure_deployment_identity(&db_path, identity)?;

    // Persist the first price under setup's hard L1 requirement. Fixed writes
    // the configured exponent; Uniswap quotes once so Tip never samples empty.
    match prepared_fee_oracle {
        PreparedFeeOracle::Fixed { log_gas_price } => {
            storage::Storage::open(&db_path)?.set_log_gas_price(log_gas_price)?;
        }
        PreparedFeeOracle::Uniswap {
            provider, token, ..
        } => {
            crate::l1::fee_oracle::persist_first_price(
                db_path.clone(),
                provider,
                token,
                process_lock.clone(),
            )
            .await?;
        }
    }

    // ── Detection gate, step 0: checkpoint sanity ────────────
    // A checkpoint promotion cannot predate the application's deployment
    // block (the scan genesis — no input exists before it).
    // `B = 0` is the genesis bootstrap (no checkpoint) and is always valid.
    // Setup only detects here; it never loads a checkpoint machine.
    if config.checkpoint_block != 0 && config.checkpoint_block < input_reader.app_deployment_block()
    {
        return Err(BootstrapError::CheckpointBeforeAppDeployment {
            checkpoint_block: config.checkpoint_block,
            app_deployment_block: input_reader.app_deployment_block(),
        }
        .into());
    }

    // ── Initial L1 sync ──────────────────────────────────────
    // One pass reads every direct input up to the current safe head into
    // `safe_inputs` + `safe_accepted_batches` and persists `l1_safe_head`.
    // Re-entry-safe: a retry resumes from the persisted safe head. A
    // transient sync failure leaves setup incomplete; the operator simply
    // re-runs `setup`.
    input_reader
        .sync_to_current_safe_head()
        .await
        .map_err(BootstrapError::SetupInputReader)?;

    let mut storage = storage::Storage::open(&db_path)?;

    // ── Detection gate, steps 1–2: refuse if a previous instance left work ──
    // Read-only: no key, no L1 write. Runs *after*
    // the sync so step 2 reads a `safe_inputs` table populated to the safe
    // head, and *before* the genesis snapshot / completion transaction so
    // a refusing setup cannot complete. A retry while still incomplete
    // re-detects identically. Once setup is complete,
    // plain `setup` is a no-op; detecting a dirty chain is the job of a
    // fresh setup.
    //
    // Coherence: read the submitter's `safe` wallet nonce at the
    // **persisted** safe block from the sync, not the live `Safe` tag. The head can advance
    // between the sync and this read; pinning step 1 to the same block step 2
    // scans (`safe_inputs` up to that block) closes the window where a batch
    // landing in between would be counted as settled by a live-tag read yet
    // be missing from the not-yet-resynced scan. `pending` stays live, so any
    // submitter activity past the synced head still trips `pending > safe`.
    //
    // Step 1 reads the LOCAL provider's pool view — a zombie tx dropped
    // from this pool but alive elsewhere evades it, bounded at runtime by the
    // content-identity check (CanonicalDivergence → cockroach recovery).
    let synced_safe_block = storage.current_safe_block()?;
    let wallet_nonces = read_submitter_wallet_nonces(
        &config.rpc.eth_rpc_url,
        config.batch_submitter_address,
        synced_safe_block,
        config.rpc.allow_insecure_rpc,
    )
    .await?;
    run_detection_gate(
        &mut storage,
        config.batch_submitter_address,
        config.checkpoint_block,
        wallet_nonces,
    )?;

    // ── Genesis snapshot ─────────────────────────────────────
    // Construct only after the admission facts and every
    // detect-and-refuse gate. Factory errors follow normal setup settlement;
    // completed no-ops never construct genesis state.
    let genesis_app = genesis_app()?;
    fill::register_genesis_baseline::<A>(genesis_app, &mut storage, &dumps_dir)?;

    // Baseline registration committed setup_complete with the restore point.
    tracing::info!(
        data_dir = %config.data_dir,
        chain_id = identity.chain_id,
        app_address = %identity.app_address,
        input_box_address = %identity.input_box_address,
        app_deployment_block = identity.app_deployment_block,
        batch_submitter_address = %identity.batch_submitter_address,
        "setup complete"
    );
    Ok(())
}

enum SetupAdmission {
    Proceed,
    AlreadyComplete,
}

/// Refuse a fixed price the fee-floor CHECK would reject, before it pins an
/// identity whose first price write could never succeed.
fn check_fixed_log_gas_price(db_path: &str, log_gas_price: i16) -> Result<(), CommandError> {
    let floor = storage::Storage::open(db_path)?.log_gas_price_floor()?;
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

fn admit_setup_lifecycle(
    db_path: &str,
    command: storage::LifecycleCommand,
) -> Result<SetupAdmission, CommandError> {
    let exists = std::path::Path::new(db_path)
        .try_exists()
        .map_err(|err| super::io_error_at("database", db_path, err))?;
    let populated = exists && !existing_sqlite_schema_is_empty(db_path)?;
    if !populated {
        storage::Storage::initialize_for_command(db_path, command)?;
        return Ok(SetupAdmission::Proceed);
    }

    // Admission facts only: divergence is absorbing; a completed setup
    // is once-per-database (an already-complete setup is a no-op); a crashed
    // prior attempt left only idempotent residue behind — the retry proceeds
    // fresh over it.
    let mut storage = storage::Storage::open_read_only(db_path)?;
    if let Some((nonce, _)) = storage.canonical_divergence()? {
        return Err(storage::LifecycleError::CanonicalDivergence { nonce }.into());
    }
    if storage.is_setup_complete()? {
        return Ok(SetupAdmission::AlreadyComplete);
    }
    Ok(SetupAdmission::Proceed)
}

fn settle_setup_lifecycle(
    db_path: &str,
    command: storage::LifecycleCommand,
    result: &Result<(), CommandError>,
) -> Result<(), CommandError> {
    match result {
        Ok(()) => {
            let storage = storage::Storage::open_read_only(db_path)?;
            if !storage.is_setup_complete()? {
                return Err(storage::LifecycleError::Malformed(
                    "setup returned without its complete baseline".to_string(),
                )
                .into());
            }
            Ok(())
        }
        Err(_) => {
            // Verdict-neutral black-box settlement: the write never
            // replaces the command's own error.
            crate::commands::record_terminal_fault_best_effort(db_path, command, result);
            Ok(())
        }
    }
}

/// The read-only detection gate, separated
/// from the L1/RPC I/O so its logic is unit-testable against a seeded DB.
///
/// `wallet_nonces` is the submitter's `(pending, safe)` wallet nonce read from
/// L1; `storage` holds the safe inputs synced to the safe head. The two steps
/// are ordered on purpose: step 1 (wallet nonce settled?) gates step 2 (scan).
/// An unsettled wallet nonce refuses *without* scanning — unsafe batches are not yet in
/// `safe_inputs`, so the scan would be incomplete and could false-negative.
fn run_detection_gate(
    storage: &mut storage::Storage,
    batch_submitter: Address,
    checkpoint_block: u64,
    (pending_wallet_nonce, safe_wallet_nonce): (u64, u64),
) -> Result<(), CommandError> {
    // Step 1 — `pending > safe` means a previous instance left in-flight
    // (pending or mined-but-unsafe) batch txs.
    if pending_wallet_nonce > safe_wallet_nonce {
        return Err(SetupRefuse::WalletNonceUnsettled {
            pending: pending_wallet_nonce,
            safe: safe_wallet_nonce,
        }
        .into());
    }
    // Step 2 — settled ⟹ nothing of ours sits unsafe, so the synced
    // `safe_inputs` already reflect every previous batch (scanning them is
    // equivalent to scanning `(B, safe]`). Refuse if any batch-submitter tx
    // landed strictly past the checkpoint block. The query reads the
    // reader-synced table, inheriting the reader's range-completeness
    // hardening — see `first_batch_submitter_input_after_block`.
    if let Some((safe_input_index, found_block)) =
        storage.first_batch_submitter_input_after_block(batch_submitter, checkpoint_block)?
    {
        return Err(SetupRefuse::BatchPastCheckpoint {
            checkpoint_block,
            found_block,
            safe_input_index,
        }
        .into());
    }
    Ok(())
}

/// Read the batch-submitter wallet nonce at the pending tag and at `safe_block`
/// (read-only): returns `(pending, safe)`. `pending > safe` means a previous
/// instance left batch txs past the synced safe head — the step-1 signal.
///
/// `safe_block` is the safe head the preceding sync persisted; the `safe` nonce
/// is read at exactly that block (`Number(safe_block)`) so step 1 is coherent
/// with step 2's scan of `safe_inputs` synced to the same block — not the live
/// `Safe` tag, which can advance between the sync and this read and reintroduce
/// a TOCTOU gap. `None` (no safe head persisted — not expected after a
/// successful sync) falls back to the `Safe` tag.
///
/// Builds a provider from the RPC URL via the same `create_read_provider` the input
/// reader uses (the reader holds no long-lived provider — it builds one per
/// pass), so no second persistent connection is introduced. Both reads are the
/// batch-submitter's `eth_getTransactionCount`; an RPC failure here is
/// transient ([`BootstrapError::DetectionWalletNonceRead`]) — the prior sync already
/// proved L1 reachable.
async fn read_submitter_wallet_nonces(
    eth_rpc_url: &str,
    batch_submitter: Address,
    safe_block: Option<u64>,
    allow_insecure: bool,
) -> Result<(u64, u64), BootstrapError> {
    use alloy::providers::Provider;
    use alloy::rpc::types::BlockNumberOrTag;

    let provider = crate::l1::provider::create_read_provider(eth_rpc_url, allow_insecure)
        .map_err(|message| BootstrapError::DetectionWalletNonceRead { message })?;
    let pending = provider
        .get_transaction_count(batch_submitter)
        .block_id(BlockNumberOrTag::Pending.into())
        .await
        .map_err(|e| BootstrapError::DetectionWalletNonceRead {
            message: crate::l1::provider::error_chain(&e),
        })?;
    let safe_tag = match safe_block {
        Some(block) => BlockNumberOrTag::Number(block),
        None => BlockNumberOrTag::Safe,
    };
    let safe = provider
        .get_transaction_count(batch_submitter)
        .block_id(safe_tag.into())
        .await
        .map_err(|e| BootstrapError::DetectionWalletNonceRead {
            message: crate::l1::provider::error_chain(&e),
        })?;
    Ok((pending, safe))
}

/// A first `Connection::open` can create the DB file before the transactional
/// baseline migration begins. That exact empty-schema crash state contains no
/// possible DB verdict and may resume setup. Any object at all makes the DB
/// non-empty; malformed/populated databases then take the strict read-only
/// lifecycle gate and are never migrated speculatively.
fn existing_sqlite_schema_is_empty(
    db_path: &str,
) -> Result<bool, crate::commands::error::CommandError> {
    let metadata =
        std::fs::metadata(db_path).map_err(|err| super::io_error_at("database", db_path, err))?;
    if metadata.len() == 0 {
        return Ok(true);
    }
    let connection = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let object_count: i64 =
        connection.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))?;
    Ok(object_count == 0)
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
    use crate::storage::Storage;
    use crate::storage::StoredSafeInput;
    use crate::storage::test_helpers::{SENDER_A, default_protocol_timing};
    use crate::test_support::temp_db;

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

    #[test]
    fn completed_plain_setup_is_a_noop_and_writes_nothing() {
        let db = temp_db("setup-noop-preserves-recovery");
        let mut storage =
            Storage::initialize_for_command(db.path.as_str(), storage::LifecycleCommand::Setup)
                .expect("initialize");
        storage
            .complete_baseline_setup(
                &db._dir.path().join("baseline"),
                sequencer_core::history::ExecutedInputCount::ZERO,
                0,
                0,
                false,
            )
            .expect("complete setup");
        storage
            .record_terminal_fault(storage::LifecycleCommand::Run, "prior terminal death")
            .expect("record prior fault");
        let before = storage
            .latest_terminal_fault()
            .expect("read")
            .expect("recorded fault");
        drop(storage);

        assert!(matches!(
            admit_setup_lifecycle(db.path.as_str(), storage::LifecycleCommand::Setup)
                .expect("plain setup no-op"),
            SetupAdmission::AlreadyComplete
        ));
        let after = Storage::open_read_only(db.path.as_str())
            .expect("reopen")
            .latest_terminal_fault()
            .expect("read")
            .expect("still recorded");
        assert_eq!(after, before, "plain setup must write nothing");
    }

    /// Seed `safe_inputs` with one batch-submitter (`SENDER_A`) input per block
    /// in `blocks`, synced up to the max block. Payloads are junk (scheduler
    /// no-ops) — detection scans raw rows, not the accepted frontier.
    fn seed_submitter_inputs(storage: &mut Storage, blocks: &[u64]) {
        let protocol = default_protocol_timing();
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
            .append_safe_inputs(safe_block, inputs.as_slice(), SENDER_A, &protocol)
            .expect("seed safe inputs");
    }

    #[test]
    fn detection_passes_when_settled_and_no_batch_past_checkpoint() {
        let db = temp_db("detect-pass");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        // Settled (pending == safe) and no submitter inputs at all.
        assert!(run_detection_gate(&mut storage, SENDER_A, 0, (5, 5)).is_ok());
    }

    #[test]
    fn detection_refuses_unsettled_wallet_nonce_before_scanning() {
        let db = temp_db("detect-unsettled");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        // Step 1 gates step 2: `pending > safe` refuses regardless of inputs
        // (and the DB is empty here, proving step 2 never ran).
        match run_detection_gate(&mut storage, SENDER_A, 0, (14, 13)) {
            Err(CommandError::Bootstrap(BootstrapError::SetupRefuse(
                SetupRefuse::WalletNonceUnsettled { pending, safe },
            ))) => assert_eq!((pending, safe), (14, 13)),
            other => panic!("expected WalletNonceUnsettled, got {other:?}"),
        }
    }

    #[test]
    fn detection_refuses_batch_past_checkpoint_when_settled() {
        let db = temp_db("detect-batch-past");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        seed_submitter_inputs(&mut storage, &[18]);
        match run_detection_gate(&mut storage, SENDER_A, 0, (1, 1)) {
            Err(CommandError::Bootstrap(BootstrapError::SetupRefuse(
                SetupRefuse::BatchPastCheckpoint {
                    checkpoint_block,
                    found_block,
                    ..
                },
            ))) => {
                assert_eq!(checkpoint_block, 0);
                assert_eq!(found_block, 18);
            }
            other => panic!("expected BatchPastCheckpoint, got {other:?}"),
        }
    }

    #[test]
    fn detection_passes_when_batch_at_or_below_checkpoint() {
        let db = temp_db("detect-batch-below");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        seed_submitter_inputs(&mut storage, &[18]);
        // A batch at block 18 is part of `S` for a checkpoint at 18 (not
        // strictly past it), so a settled deployment proceeds.
        assert!(run_detection_gate(&mut storage, SENDER_A, 18, (1, 1)).is_ok());
    }

    /// A fixed price below the fee floor would pin an identity whose first
    /// price write the schema refuses; setup refuses it first.
    #[test]
    fn fixed_price_below_the_floor_is_refused() {
        let db = crate::test_support::temp_db("fixed-price-floor");
        drop(storage::Storage::open(db.path.as_str()).expect("initialize schema"));

        let err = check_fixed_log_gas_price(&db.path, -1357)
            .expect_err("one below the floor must be refused");
        assert!(
            matches!(
                err,
                CommandError::Bootstrap(BootstrapError::FeeOracleMisconfig { .. })
            ),
            "{err:?}"
        );
        check_fixed_log_gas_price(&db.path, -1356).expect("the floor itself is accepted");
    }

    /// Setup's L1 discovery uses the startup polarity: a URL the client refuses
    /// re-fails identically, so it is terminal, and so is an endpoint that
    /// redirects the chain-id read; an unreachable RPC fails at the chain-id
    /// read (`ChainIdRpc`) and is retried. The fee-source connect follows the
    /// runtime classification with setup's messages.
    #[tokio::test]
    async fn setup_l1_failures_exit_by_startup_phase() {
        use crate::commands::error::{EXIT_RESTART_TRANSIENT, EXIT_TERMINAL};
        use crate::harness::{Cli, Command};
        use clap::Parser;

        async fn setup_with(rpc_url: &str) -> CommandError {
            let data_dir = tempfile::tempdir().expect("data dir");
            let cli = Cli::try_parse_from([
                "sequencer",
                "setup",
                "--data-dir",
                data_dir.path().to_str().expect("utf8 path"),
                "--eth-rpc-url",
                rpc_url,
                "--chain-id",
                "31337",
                "--app-address",
                "0x1111111111111111111111111111111111111111",
                "--batch-submitter-address",
                "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
            ])
            .expect("parse setup");
            let Command::Setup(config) = cli.command else {
                panic!("expected setup subcommand");
            };
            setup(
                *config,
                || -> Result<crate::test_support::StubApp, AppError> {
                    unreachable!("setup fails before building the genesis app")
                },
            )
            .await
            .expect_err("setup cannot discover L1")
        }

        let malformed = setup_with("not a url").await;
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

        let unreachable = setup_with("http://127.0.0.1:1").await;
        assert_eq!(
            unreachable.exit_code(),
            EXIT_RESTART_TRANSIENT,
            "{unreachable:?}"
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
}
