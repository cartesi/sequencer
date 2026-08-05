// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The `setup` subcommand: establish everything timeless
//! and pin it in the DB, then mark setup complete so `run` will boot.
//!
//! Steps, in order — each individually idempotent so a crashed `setup`
//! re-runs cleanly, and the marker written *last* is the single
//! linearization point for "A finished":
//!
//! 1. Validate protocol timing; create the data dir + `dumps/`.
//! 2. Require L1: discover the InputBox address + app deployment block from the app
//!    contract, validate the RPC chain id. (No cached-identity fallback —
//!    this is first-boot; an unreachable L1 is a retryable refusal.)
//! 3. Pin the deployment identity (chain id, app address, InputBox address,
//!    app deployment block, batch-submitter **address** — `setup` never signs).
//! 4. Initial L1 sync: read all direct inputs up to the current safe head.
//! 5. Register the genesis application state as the finalized snapshot.
//! 6. Write the `setup_complete` marker.
//!
//! `setup` is L1-read-only: it takes the batch-submitter address (not the
//! key) and does no L1 writes.

use alloy_primitives::Address;
use sequencer_core::application::Application;
use sequencer_core::scheduler::{FoldInput, SchedulerConfig, fold_replay};

use super::config::{FeeOracleMode, SetupConfig};
use super::{
    BootstrapError, IdentityError, InputReaderExit, RunError, SetupRecoveryError, SetupRefuse,
    WorkerExit, ensure_deployment_identity, setup_fill, validate_rpc_chain_id,
};
use crate::ingress::inclusion_lane::dump_info;
use crate::l1::provider::VerifiedSignerProviderError;
use crate::l1::reader::{InputReader, InputReaderConfig, InputReaderError};
use crate::recovery::{MempoolFlusher, assert_resync_caught_up};
use crate::storage::{self, DeploymentIdentity, FeeOracleIdentity};

pub async fn setup<A>(config: SetupConfig, genesis_app: A) -> Result<(), RunError>
where
    A: Application + 'static,
{
    // Cross-field config validation (recovery vs the recovery-only args). A
    // misconfig is operator error — terminal, before any filesystem touch.
    config
        .validate()
        .map_err(|message| SetupRecoveryError::InvalidConfig { message })?;

    std::fs::create_dir_all(&config.data_dir)?;
    let dumps_dir = std::path::Path::new(&config.data_dir).join("dumps");
    std::fs::create_dir_all(&dumps_dir)?;
    let db_path = config.db_path();
    let timing = config.timing.protocol_timing()?;

    // The `setup_complete` marker gates re-invocation differently per mode:
    //   * plain `setup` is idempotent — a re-run on a complete DB is a no-op
    //     success (a half-finished setup has no marker and re-runs below);
    //   * `setup --recovery` is a strict one-shot on a freshly-wiped DB — a
    //     complete DB means recovery already ran (or this is a live deployment),
    //     and re-pointing it at a different checkpoint would strand its state,
    //     so refuse (terminal). A crash-*before*-marker recovery has no marker;
    //     its fill is not blindly idempotent either — the partial/residue cases
    //     are handled fail-loud by `setup_fill::fill_recovery_state` (see there).
    {
        let storage = storage::Storage::open(&db_path)?;
        if storage.is_setup_complete()? {
            if config.recovery {
                return Err(SetupRecoveryError::AlreadySetUp.into());
            }
            tracing::info!(data_dir = %config.data_dir, "setup already complete — nothing to do");
            return Ok(());
        }
    }

    // ── L1 discovery (required) ──────────────────────────────
    let input_reader_config = InputReaderConfig {
        rpc_url: config.eth_rpc_url.clone(),
        allow_insecure_rpc: config.allow_insecure_rpc,
        app_address: config.app_address,
        poll_interval: super::INPUT_READER_POLL_INTERVAL,
        long_block_range_error_codes: config.long_block_range_error_codes.clone(),
        // `validate_rpc_chain_id` below re-checks this against the live RPC; the
        // reader's own check then verifies every sync provider serves it too.
        expected_chain_id: config.chain_id,
    };
    let mut input_reader = match InputReader::new(
        db_path.clone(),
        input_reader_config,
        config.batch_submitter_address,
        timing,
    )
    .await
    {
        Ok(reader) => reader,
        Err(InputReaderError::Provider(e)) => {
            // First boot needs L1 to discover + pin identity. Retryable: the
            // operator brings L1 up and re-runs `setup`.
            tracing::error!(error = %e, "L1 unreachable during setup — cannot discover identity");
            return Err(IdentityError::FirstBootRequiresL1.into());
        }
        Err(source) => {
            return Err(RunError::Worker(WorkerExit::InputReader(
                InputReaderExit::Source(source),
            )));
        }
    };

    validate_rpc_chain_id(
        &config.eth_rpc_url,
        config.chain_id,
        config.allow_insecure_rpc,
    )
    .await?;

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
            log_gas_price: u16,
        },
        Uniswap {
            identity: FeeOracleIdentity,
            provider: alloy::providers::DynProvider,
            token: crate::l1::fee_oracle::UniswapV3PriceSource,
        },
    }
    let prepared_fee_oracle = match fee_oracle {
        FeeOracleMode::Fixed { log_gas_price } => PreparedFeeOracle::Fixed { log_gas_price },
        FeeOracleMode::Uniswap(uniswap) => {
            let provider = crate::l1::provider::create_provider(
                &config.eth_rpc_url,
                config.allow_insecure_rpc,
            )
            .map_err(|message| BootstrapError::FeeOracleMisconfig { message })?;
            let token =
                crate::l1::fee_oracle::UniswapV3PriceSource::connect(provider.clone(), uniswap)
                    .await
                    .map_err(|error| {
                        let (transient, message) =
                            crate::l1::fee_oracle::uniswap::bootstrap_price_source_error(error);
                        if transient {
                            BootstrapError::FeeOracleTransient { message }
                        } else {
                            BootstrapError::FeeOracleMisconfig { message }
                        }
                    })?;
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
            let max_price_age_ms = timing.l1_read_stale_after_secs().saturating_mul(1000);
            let oracle = crate::l1::fee_oracle::FeeOracle::new(
                db_path.clone(),
                crate::l1::fee_oracle::FeeOracle::DEFAULT_POLL_INTERVAL,
                max_price_age_ms,
                provider,
                Box::new(token),
            );
            oracle.refresh_once().await?;
        }
    }

    // ── Detection gate, step 0: checkpoint sanity ────────────
    // A checkpoint promotion cannot predate the application's deployment
    // block (the scan genesis — no input exists before it).
    // `B = 0` is the genesis bootstrap (no checkpoint) and is always valid.
    // (PR3 detects only; loading a non-genesis checkpoint machine and the
    // `A < B` check are `setup --recovery` / PR5.)
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
    // Idempotent: a retried setup resumes from the persisted safe head. A
    // transient sync failure leaves the marker unwritten, so a re-run resyncs
    // cleanly.
    //
    // Recovery rebuilds the batch tree from the checkpoint *after* this sync, so
    // the local tree is empty here. Disable frontier population for recovery's
    // syncs (this one and the post-flush re-sync): otherwise the
    // content-identity check would see every L1 batch as "foreign" and falsely
    // freeze the frontier, poisoning the rebuilt DB. `run`'s first sync
    // populates it correctly once the anchor = N' is set (I16).
    if config.recovery {
        input_reader.set_frontier_mode(storage::FrontierMode::DeferUntilAnchorSet);
    }
    input_reader
        .sync_to_current_safe_head()
        .await
        .map_err(|e| RunError::Worker(WorkerExit::InputReader(InputReaderExit::Source(e))))?;

    // ── Branch: recovery rebuild vs the genesis-style detect-and-refuse ──
    let mut storage = storage::Storage::open(&db_path)?;

    if config.recovery {
        // ── Recovery: flush → fold → fill. Replaces the
        // detection gate + genesis snapshot; the operator is acting *on* the
        // refusal the gate would otherwise raise.
        recover::<A>(
            &config,
            &identity,
            timing,
            &mut input_reader,
            &mut storage,
            &dumps_dir,
        )
        .await?;
    } else {
        // ── Detection gate, steps 1–2: refuse if a previous instance left work ──
        // Read-only: no key, no L1 write. Runs *after*
        // the sync so step 2 reads a `safe_inputs` table populated to the safe
        // head, and *before* the genesis snapshot / marker so a refusing setup
        // leaves no marker — a re-run (while still incomplete) re-detects
        // identically. (Once the marker *is* written, the idempotent early-return
        // above skips the gate; that is correct — the deployment is this
        // instance's own, and detecting a dirty chain is the job of a *fresh*
        // `setup` whose marker is absent.)
        //
        // Coherence: read the submitter's `safe` nonce at the **persisted** safe
        // block from the sync, not the live `Safe` tag. The head can advance
        // between the sync and this read; pinning step 1 to the same block step 2
        // scans (`safe_inputs` up to that block) closes the window where a batch
        // landing in between would be counted as settled by a live-tag read yet
        // be missing from the not-yet-resynced scan. `pending` stays live, so any
        // submitter activity past the synced head still trips `pending > safe`.
        //
        // F1: step 1 reads the LOCAL provider's pool view — a zombie tx dropped
        // from this pool but alive elsewhere evades it, bounded at runtime by the
        // content-identity check (CanonicalDivergence → cockroach recovery).
        let synced_safe_block = storage.current_safe_block()?;
        let nonce_views = read_submitter_nonce_views(
            &config.eth_rpc_url,
            config.batch_submitter_address,
            synced_safe_block,
            config.allow_insecure_rpc,
        )
        .await?;
        run_detection_gate(
            &mut storage,
            config.batch_submitter_address,
            config.checkpoint_block,
            nonce_views,
        )?;

        // Refuse to register genesis over leftover recovery state. A
        // `setup --recovery` that crashed before its marker leaves a non-zero
        // batch-tree anchor (and maybe a root tip); booting genesis-style over
        // it would root the tree at the recovery nonce instead of 0. Fail loud
        // (the marker is the only "this DB is mine" signal, and it's absent in
        // both the fresh-genesis and crashed-recovery cases — so we check the
        // anchor explicitly). Operator wipes the data dir and re-runs.
        let anchor = storage.batch_tree_anchor()?;
        if anchor != 0 {
            return Err(SetupRecoveryError::GenesisOverRecoveryResidue { anchor }.into());
        }

        // ── Genesis snapshot ─────────────────────────────────────
        setup_fill::register_genesis_finalized_snapshot::<A>(
            genesis_app,
            &mut storage,
            &dumps_dir,
        )?;
    }

    // ── Marker (the single linearization point for "setup finished") ──
    storage.mark_setup_complete()?;

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

/// The trusted checkpoint a `setup --recovery` folds from — the machine state
/// `S` at block `B`, plus the two scalars the fold needs that the bare-metal app
/// cannot recompute: `A` (`S`'s last-executed safe block — the fridge is the
/// `(A, B]` directs) and `N` (the resume batch nonce at `B`, which rides in the
/// dump's `info.toml`). See the data dictionary in `docs/recovery/cockroach.md`.
struct Checkpoint<A> {
    /// `S` — the checkpoint app state at block `B`.
    app: A,
    /// `A` — `S`'s last-executed safe block.
    executed_safe_block: u64,
    /// `N` — the resume batch nonce at `B` (checkpoint metadata). Named to
    /// contrast with the fold's output `N'` (`resume_nonce` in `recover`).
    checkpoint_nonce: u64,
    /// `B` — the checkpoint's L1 inclusion block (`--checkpoint-block`).
    checkpoint_block: u64,
}

impl<A: Application> Checkpoint<A> {
    /// Load `S` from the dump dir, derive `A` and `N`, and enforce the load-time
    /// precondition `A < B` (else the `(A, B]` fridge range is ill-defined). All
    /// failures are terminal — the operator must supply a valid checkpoint.
    fn load(dir: &std::path::Path, checkpoint_block: u64) -> Result<Self, SetupRecoveryError> {
        let load_err = |message: String| SetupRecoveryError::CheckpointLoad {
            path: dir.display().to_string(),
            message,
        };
        let info = dump_info::read_info(dir).map_err(|e| load_err(e.to_string()))?;
        let app = A::from_dump(&dump_info::app_prefix(dir)).map_err(|e| load_err(e.to_string()))?;
        let executed_safe_block = app.last_executed_safe_block();
        if executed_safe_block >= checkpoint_block {
            return Err(SetupRecoveryError::CheckpointNotBeforeBlock {
                executed_safe_block,
                checkpoint_block,
            });
        }
        Ok(Self {
            app,
            executed_safe_block,
            checkpoint_nonce: info.next_batch_nonce,
            checkpoint_block,
        })
    }
}

/// Source the fold inputs from the synced `safe_inputs`: the
/// `(A, B]` direct **seeds** and the full `(B, C]` **replay** stream.
///
/// Seeds must be directs only — the fold enqueues them straight into the fridge
/// (`enqueue_direct`, no classification). The `sender != submitter` filter IS
/// the scheduler's own sender-based classification: the fold's `process_input`
/// routes `sender == sequencer_address` to the batch path and everything else to
/// a direct, so a dropped `sender == submitter` row is never a direct-with-
/// effect — it is a batch already folded into `S` (every valid batch with
/// inclusion `≤ B` is in `S` by the operator's checkpoint, the same trust
/// boundary as the resume nonce `N`) or an undecodable scheduler no-op.
///
/// Replay is the full stream; the fold classifies each input itself, so no
/// pre-filter.
fn source_fold_inputs<A>(
    storage: &mut storage::Storage,
    checkpoint: &Checkpoint<A>,
    stop_block: u64,
    submitter: Address,
) -> Result<(Vec<FoldInput>, Vec<FoldInput>), RunError> {
    let seeds = storage
        .safe_inputs_in_block_range(checkpoint.executed_safe_block, checkpoint.checkpoint_block)?
        .into_iter()
        .filter(|input| input.sender != submitter)
        .map(to_fold_input)
        .collect();
    let replay = storage
        .safe_inputs_in_block_range(checkpoint.checkpoint_block, stop_block)?
        .into_iter()
        .map(to_fold_input)
        .collect();
    Ok((seeds, replay))
}

/// The `setup --recovery` procedure: rebuild a freshly-wiped DB from
/// a trusted checkpoint instead of refusing. Runs after the shared prefix
/// (identity pinned, initial sync done); replaces the detection gate + genesis
/// snapshot. Distinct, terminal error type ([`SetupRecoveryError`]) from
/// `run`'s recovery — operator-driven, one-shot.
///
/// The `flush → fold → fill` steps are enumerated authoritatively in
/// **[`docs/recovery/cockroach.md`](../../../docs/recovery/cockroach.md)** (spec,
/// data dictionary `A`/`B`/`C`/`N`/`N'`, and code map) and anchored inline below
/// (`// 1.`…`// 6.`). Read the doc before editing this function.
///
/// `N` (and the whole checkpoint tuple `S`/`A`/`B`/`N`) is **trusted** metadata
/// from the sequencer-produced finalized dump; recovery does not independently
/// re-verify it (there is no local oracle without replaying the scheduler from
/// genesis). See cockroach.md "Data dictionary" for the trust boundary.
async fn recover<A>(
    config: &SetupConfig,
    identity: &DeploymentIdentity,
    timing: sequencer_core::protocol::ProtocolTiming,
    input_reader: &mut InputReader,
    storage: &mut storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<(), RunError>
where
    A: Application + 'static,
{
    // 1. Load the trusted checkpoint (S, A, N, B); require A < B.
    let checkpoint_dir = std::path::Path::new(
        config
            .checkpoint_dump_dir
            .as_deref()
            .expect("validated: recovery requires a checkpoint dump dir"),
    );
    let checkpoint = Checkpoint::<A>::load(checkpoint_dir, config.checkpoint_block)?;

    // 2. Flush the previous instance's batch txs → C, the post-flush safe head.
    let stop_block =
        flush_wallet_nonce(config, identity, timing.seconds_per_block, storage).await?;

    // 3. Re-sync through C; F2 coherence. Frontier population stays OFF (the
    //    caller disabled it before the initial sync): the tree is rebuilt in
    //    step 6, so a frontier built here against an empty tree would falsely
    //    diverge. `run`'s first sync populates it correctly once anchor = N'.
    tracing::info!("re-syncing L1 safe head after flush");
    input_reader
        .sync_to_current_safe_head()
        .await
        .map_err(|e| RunError::Worker(WorkerExit::InputReader(InputReaderExit::Source(e))))?;
    let resynced_safe_block = storage.current_safe_block()?.unwrap_or(0);
    assert_resync_caught_up(resynced_safe_block, stop_block)?;

    // 4. Source the (A, B] direct seeds + the (B, C] replay stream.
    let submitter = identity.batch_submitter_address;
    let (seeds, replay) = source_fold_inputs(storage, &checkpoint, stop_block, submitter)?;

    // 5. Fold (S, N) over the inputs → (S', N').
    let Checkpoint {
        app,
        executed_safe_block,
        checkpoint_nonce,
        checkpoint_block,
    } = checkpoint;
    let domain = sequencer_core::build_input_domain(identity.chain_id, identity.app_address);
    let scheduler_config = SchedulerConfig::new(submitter);
    let (recovered_app, resume_nonce) = fold_replay(
        app,
        checkpoint_nonce,
        scheduler_config,
        domain,
        seeds,
        replay,
        stop_block,
    );

    // 6. Fill the DB: finalized S', tree anchored at N', cursor past the ≤C
    //    directs (already in S'). run boots from this state, and its first sync
    //    populates the gold frontier from L1 with the anchor = N' (so the folded
    //    `< N'` batches are skipped as trusted collapsed history, not foreign).
    setup_fill::fill_recovery_state(recovered_app, resume_nonce, stop_block, storage, dumps_dir)?;

    tracing::info!(
        executed_safe_block,
        checkpoint_block,
        stop_block,
        resume_nonce,
        "recovery complete — DB rebuilt from checkpoint"
    );
    Ok(())
}

/// Recovery step 2: flush the previous instance's stranded batch txs and wait
/// for the wallet nonce to settle, returning `C` — the post-flush safe head at
/// which every prior batch is resolved at safe depth. This is `setup`'s only
/// L1-signing action (it otherwise takes the submitter *address*, not the key).
///
/// The flush-acquire core (flusher + watermark sink + `flush_and_wait`) is
/// shared with the runtime danger path and `flush-mempool` via
/// [`MempoolFlusher::flush_to_safe`]; this site supplies the recovery key, the
/// pinned submitter address, and the watermark from the open `storage`.
async fn flush_wallet_nonce(
    config: &SetupConfig,
    identity: &DeploymentIdentity,
    seconds_per_block: u64,
    storage: &mut storage::Storage,
) -> Result<u64, RunError> {
    // The recovery key must match the pinned batch-submitter address — flushing
    // under a different account's key would settle the wrong wallet's nonce (and
    // burn its gas) while recovery never settles `identity.batch_submitter_address`.
    // Gate before signing, exactly as `flush-mempool` does.
    let key = super::verify_submitter_key(config.resolve_recovery_key()?, identity)?;
    // Keyed-write chain-id gate: `setup --recovery`'s flush signs L1 no-op txs,
    // so re-confirm the RPC serves the pinned chain right before building the
    // signer (the earlier `validate_rpc_chain_id` may have gone stale). Same
    // guarded constructor the runtime flush and `flush-mempool` use.
    let provider = crate::l1::provider::create_verified_signer_provider(
        &config.eth_rpc_url,
        &key,
        config.chain_id,
        config.allow_insecure_rpc,
    )
    .await
    .map_err(|e| match e {
        VerifiedSignerProviderError::ChainIdMismatch { rpc, expected } => {
            RunError::Bootstrap(BootstrapError::ChainIdMismatch {
                rpc,
                config: expected,
            })
        }
        VerifiedSignerProviderError::ChainIdRpc(message) => {
            RunError::Bootstrap(BootstrapError::ChainIdRpc { message })
        }
        VerifiedSignerProviderError::Create(msg) => RunError::Io(std::io::Error::other(msg)),
    })?;
    let watermark = storage.wallet_nonce_watermark()?;
    let stop_block = MempoolFlusher::flush_to_safe(
        provider,
        identity.batch_submitter_address,
        seconds_per_block,
        config.db_path(),
        watermark,
    )
    .await?;
    Ok(stop_block)
}

/// Map a synced `safe_inputs` row to a fold input. The EIP-712 domain is a
/// deployment-wide constant the engine clones onto each, so it is not stored
/// per row.
fn to_fold_input(input: crate::storage::StoredSafeInput) -> FoldInput {
    FoldInput {
        sender: input.sender,
        inclusion_block: input.block_number,
        payload: input.payload,
    }
}

/// The read-only detection gate, separated
/// from the L1/RPC I/O so its logic is unit-testable against a seeded DB.
///
/// `nonce_views` is the submitter's `(pending, safe)` wallet nonce read from
/// L1; `storage` holds the safe inputs synced to the safe head. The two steps
/// are ordered on purpose: step 1 (nonce settled?) gates step 2 (scan). An
/// unsettled nonce refuses *without* scanning — unsafe batches are not yet in
/// `safe_inputs`, so the scan would be incomplete and could false-negative.
fn run_detection_gate(
    storage: &mut storage::Storage,
    batch_submitter: Address,
    checkpoint_block: u64,
    (pending_nonce, safe_nonce): (u64, u64),
) -> Result<(), RunError> {
    // Step 1 — `pending > safe` means a previous instance left in-flight
    // (pending or mined-but-unsafe) batch txs.
    if pending_nonce > safe_nonce {
        return Err(SetupRefuse::WalletNonceUnsettled {
            pending: pending_nonce,
            safe: safe_nonce,
        }
        .into());
    }
    // Step 2 — settled ⟹ nothing of ours sits unsafe, so the synced
    // `safe_inputs` already reflect every previous batch (scanning them is
    // equivalent to scanning `(B, safe]`). Refuse if any batch-submitter tx
    // landed strictly past the checkpoint block. The query reads the
    // reader-synced table, inheriting the reader's range-completeness hardening
    // (F5) — see `first_batch_submitter_input_after_block`.
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
/// Builds a provider from the RPC URL via the same `create_provider` the input
/// reader uses (the reader holds no long-lived provider — it builds one per
/// pass), so no second persistent connection is introduced. Both reads are the
/// batch-submitter's `eth_getTransactionCount`; an RPC failure here is
/// transient ([`BootstrapError::DetectionNonceRead`]) — the prior sync already
/// proved L1 reachable.
async fn read_submitter_nonce_views(
    eth_rpc_url: &str,
    batch_submitter: Address,
    safe_block: Option<u64>,
    allow_insecure: bool,
) -> Result<(u64, u64), BootstrapError> {
    use alloy::providers::Provider;
    use alloy::rpc::types::BlockNumberOrTag;

    let provider = crate::l1::provider::create_provider(eth_rpc_url, allow_insecure)
        .map_err(|message| BootstrapError::DetectionNonceRead { message })?;
    let pending = provider
        .get_transaction_count(batch_submitter)
        .block_id(BlockNumberOrTag::Pending.into())
        .await
        .map_err(|e| BootstrapError::DetectionNonceRead {
            message: e.to_string(),
        })?;
    let safe_tag = match safe_block {
        Some(block) => BlockNumberOrTag::Number(block),
        None => BlockNumberOrTag::Safe,
    };
    let safe = provider
        .get_transaction_count(batch_submitter)
        .block_id(safe_tag.into())
        .await
        .map_err(|e| BootstrapError::DetectionNonceRead {
            message: e.to_string(),
        })?;
    Ok((pending, safe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use crate::storage::StoredSafeInput;
    use crate::storage::test_helpers::{SENDER_A, default_protocol_timing, temp_db};

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
    fn detection_refuses_unsettled_nonce_before_scanning() {
        let db = temp_db("detect-unsettled");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        // Step 1 gates step 2: `pending > safe` refuses regardless of inputs
        // (and the DB is empty here, proving step 2 never ran).
        match run_detection_gate(&mut storage, SENDER_A, 0, (14, 13)) {
            Err(RunError::Bootstrap(BootstrapError::SetupRefuse(
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
            Err(RunError::Bootstrap(BootstrapError::SetupRefuse(
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

    /// `setup --recovery`'s flush signs L1 no-ops with the recovery key, so it
    /// must match the pinned batch-submitter — otherwise it would settle some
    /// other account's nonce while recovery never settles the intended wallet.
    /// The guard is pure and runs first, so a wrong key is refused before any L1
    /// contact (the RPC here is unreachable) or DB write.
    #[tokio::test]
    async fn flush_wallet_nonce_refuses_mismatched_recovery_key() {
        use crate::harness::{Cli, Command};
        use crate::storage::DeploymentIdentity;
        use clap::Parser;

        // A recovery key whose address is NOT the pinned submitter below.
        const OTHER_KEY: &str =
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let pinned_submitter: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            .parse()
            .unwrap();

        let cli = Cli::try_parse_from([
            "sequencer",
            "setup",
            "--eth-rpc-url",
            "http://127.0.0.1:1", // unreachable — the verify fails before any contact
            "--chain-id",
            "31337",
            "--app-address",
            "0x1111111111111111111111111111111111111111",
            "--batch-submitter-address",
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
            "--recovery",
            "--checkpoint-block",
            "10",
            "--checkpoint-dump-dir",
            "/nonexistent/checkpoint", // never read — refusal precedes the load
            "--batch-submitter-private-key",
            OTHER_KEY,
        ])
        .expect("parse setup --recovery");
        let config = match cli.command {
            Command::Setup(c) => *c,
            other => panic!("expected setup subcommand, got {other:?}"),
        };

        let identity = DeploymentIdentity {
            chain_id: 31337,
            app_address: "0x1111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
            input_box_address: "0x2222222222222222222222222222222222222222"
                .parse()
                .unwrap(),
            app_deployment_block: 0,
            batch_submitter_address: pinned_submitter,
            fee_oracle: FeeOracleIdentity::Fixed { log_gas_price: 0 },
        };
        let db = temp_db("recovery-wrong-key");
        let mut storage = Storage::open(db.path.as_str()).expect("open");

        let result = flush_wallet_nonce(&config, &identity, 12, &mut storage).await;
        match result {
            Err(RunError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch {
                fields,
                ..
            }))) => assert_eq!(fields, "batch_submitter_address"),
            other => panic!("expected batch_submitter_address mismatch, got: {other:?}"),
        }
    }

    #[test]
    fn source_fold_inputs_splits_seeds_and_replay_dropping_submitter_batches() {
        use crate::storage::test_helpers::SENDER_B;
        let db = temp_db("source-fold-inputs");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let protocol = default_protocol_timing();
        let submitter = SENDER_A;
        let direct = SENDER_B;

        // A=5, B=20, C=40. (A,B] directs are seeds (submitter batches dropped);
        // (B,C] is the full replay stream.
        let mk = |sender, block, marker| StoredSafeInput {
            sender,
            payload: vec![marker],
            block_number: block,
        };
        let inputs = vec![
            mk(direct, 10, 0x10),    // (A,B] direct  → seed
            mk(submitter, 15, 0x11), // (A,B] batch   → dropped (already in S)
            mk(direct, 20, 0x12),    // (A,B] direct@B → seed
            mk(submitter, 25, 0x13), // (B,C] batch   → replay
            mk(direct, 30, 0x14),    // (B,C] direct  → replay
        ];
        storage
            .append_safe_inputs(40, &inputs, submitter, &protocol)
            .expect("seed");

        let checkpoint = Checkpoint::<()> {
            app: (),
            executed_safe_block: 5,
            checkpoint_nonce: 0,
            checkpoint_block: 20,
        };
        let (seeds, replay) =
            source_fold_inputs(&mut storage, &checkpoint, 40, submitter).expect("source");

        assert_eq!(
            seeds.iter().map(|s| s.inclusion_block).collect::<Vec<_>>(),
            vec![10, 20],
            "seeds are the (A,B] directs; the submitter batch @15 is dropped"
        );
        assert_eq!(
            replay.iter().map(|r| r.inclusion_block).collect::<Vec<_>>(),
            vec![25, 30],
            "replay is the full (B,C] stream (batch + direct)"
        );
    }

    #[test]
    fn source_fold_inputs_replay_upper_bound_stays_at_c_when_resync_overshoots() {
        // The recovery resync runs to the live head H1 > C, so safe_inputs holds
        // directs past C. The fold's replay range must stay (B, C]: a direct in
        // (C, H1] must NOT leak into replay (it would be folded into S' AND then
        // left undrained for run = double execution — the (C, H1] bug class). Unit
        // twin of the recovery e2e, pinning the boundary the bug lived at.
        use crate::storage::test_helpers::SENDER_B;
        let db = temp_db("source-fold-overshoot");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let protocol = default_protocol_timing();
        let submitter = SENDER_A;
        let direct = SENDER_B;

        let mk = |block, marker| StoredSafeInput {
            sender: direct,
            payload: vec![marker],
            block_number: block,
        };
        // A=5, B=20, C=40, H1=50: a direct at block 50 is in the (C, H1] overshoot.
        let inputs = vec![
            mk(10, 0x10), // (A,B]  → seed
            mk(30, 0x14), // (B,C]  → replay
            mk(50, 0x20), // (C,H1] → must NOT appear in replay
        ];
        storage
            .append_safe_inputs(50, &inputs, submitter, &protocol)
            .expect("seed");

        let checkpoint = Checkpoint::<()> {
            app: (),
            executed_safe_block: 5,
            checkpoint_nonce: 0,
            checkpoint_block: 20,
        };
        let (seeds, replay) =
            source_fold_inputs(&mut storage, &checkpoint, 40, submitter).expect("source");

        assert_eq!(
            seeds.iter().map(|s| s.inclusion_block).collect::<Vec<_>>(),
            vec![10]
        );
        assert_eq!(
            replay.iter().map(|r| r.inclusion_block).collect::<Vec<_>>(),
            vec![30],
            "the (C, H1] direct @50 must be excluded from replay (upper bound is C, not H1)"
        );
    }
}
