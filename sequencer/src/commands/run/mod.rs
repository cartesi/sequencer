// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The `run` command: boot the sequencer from an already-set-up DB.
//!
//! Phases:
//!
//! 1. **Gate + identity**: refuse unless `setup` completed; read the pinned
//!    deployment identity from the DB (chain id / app address are no longer
//!    CLI args — they come from the identity).
//! 2. **Recovery reducer**: inspect one local fact set, execute at most one
//!    phase, and re-inspect until the reducer selects admission
//!    (`crate::recovery` owns the mechanism; this bracket only invokes it).
//! 3. **Prepare + admit + launch**: prepare every fallible runtime resource,
//!    re-run the reducer over one consistent fact set, then consume the
//!    single-use admission in one non-yielding worker launch (`workers`).
//!
//! Two senses of "admitted": phase 1 is the *lifecycle* gate (the
//! `_admitted` suffix all three command brackets share); the *runtime*
//! admission witness is minted later, in phase 3.

mod startup_hygiene;
mod workers;

use std::time::Duration;

use crate::commands::config::RunConfig;
use crate::commands::error::CommandError;
use crate::commands::{
    INPUT_READER_POLL_INTERVAL, load_setup_identity, preflight_lifecycle_command,
    record_terminal_fault_best_effort, verify_submitter_key,
};
use crate::l1::L1Config;
use crate::l1::reader::{InputReader, InputReaderConfig};
use crate::runtime::process_lock;
use crate::storage::{self, LifecycleCommand};
use sequencer_core::application::Application;

use workers::{PreparedRuntime, WorkersConfig};

/// Boot the sequencer from an already-set-up DB. Generic over the app type
/// (for the lane's `from_dump`, the egress state-file path, and the
/// max-payload bound) but takes no app *value* — `setup` already registered
/// the genesis snapshot, so the lane reloads via `A::from_dump`.
pub async fn run<A>(config: RunConfig) -> Result<(), CommandError>
where
    A: Application + Clone + Sync + 'static,
{
    // ── Gate + identity ──────────────────────────────────────
    std::fs::create_dir_all(&config.data_dir)?;
    // Exclusive process ownership, ahead of every read. The controller keeps
    // its own clone through durable settlement; runtime-owned tasks separately
    // retain clones until they stop.
    let process_lock = process_lock::ProcessLock::acquire(&config.data_dir)?;
    let db_path = config.db_path();
    let timing = config.protocol_timing()?;

    // Local absorbing facts are inspected before identity/key checks and any
    // RPC: canonical divergence and the two-sided setup-completion rule.
    // Divergence is never reinterpreted as a provider failure.
    preflight_lifecycle_command(&db_path, LifecycleCommand::Run)?;
    let identity = load_setup_identity(&db_path)?;

    // `run` holds the signing key (it submits). The key's address must match
    // the pinned submitter address — running with the wrong key against a DB
    // pinned to another submitter is a fail-loud identity mismatch.
    let key = verify_submitter_key(config.resolve_private_key()?, &identity)?;

    // The identity travels verbatim inside the L1 bundle from here on, so
    // exactly one route to the pinned values exists below this gate.
    let l1_config = L1Config {
        identity,
        eth_rpc_url: config.eth_rpc_url.clone(),
        batch_submitter_private_key: key,
        allow_insecure_rpc: config.allow_insecure_rpc,
    };

    let result = run_admitted::<A>(config, timing, l1_config, process_lock.clone()).await;
    // The Ok-path divergence fact check (run's counterpart of the one
    // `complete_setup` keeps): divergence persisted during this run must
    // exit terminal even through a clean drain — exit 0 is the one code
    // that breaks the supervisor's rediscovery chain (restart → preflight
    // refusal), and the detector's poll cadence leaves a window where a
    // clean shutdown outruns re-detection.
    let result = result.and_then(|()| refuse_divergence_on_clean_exit(&db_path));
    // Verdict-neutral black-box settlement: a terminal failure records
    // its cause best-effort — never changing the verdict — while a
    // panic/cancellation/SIGKILL writes nothing; the next boot proceeds and
    // re-derives everything from facts.
    record_terminal_fault_best_effort(&db_path, LifecycleCommand::Run, &result);
    result
}

fn refuse_divergence_on_clean_exit(db_path: &str) -> Result<(), CommandError> {
    let mut storage = storage::Storage::open_read_only(db_path)?;
    if let Some((nonce, _)) = storage.canonical_divergence()? {
        return Err(storage::LifecycleError::CanonicalDivergence { nonce }.into());
    }
    Ok(())
}

async fn run_admitted<A>(
    config: RunConfig,
    timing: sequencer_core::protocol::ProtocolTiming,
    l1_config: L1Config,
    process_lock: process_lock::ProcessLock,
) -> Result<(), CommandError>
where
    A: Application + Clone + Sync + 'static,
{
    let db_path = config.db_path();

    // `run` never re-discovers identity from L1 — it builds the reader from
    // the pinned InputBox address + app deployment block and syncs incrementally.
    let mut input_reader = InputReader::from_parts(
        InputReaderConfig {
            rpc_url: config.eth_rpc_url.clone(),
            allow_insecure_rpc: config.allow_insecure_rpc,
            app_address: l1_config.identity.app_address,
            poll_interval: INPUT_READER_POLL_INTERVAL,
            long_block_range_error_codes: config.long_block_range_error_codes.clone(),
            expected_chain_id: l1_config.identity.chain_id,
        },
        l1_config.identity.input_box_address,
        l1_config.identity.app_deployment_block,
        db_path.clone(),
        l1_config.identity.batch_submitter_address,
        timing,
        // Bootstrap syncs use nested blocking SQLite jobs. The reader takes
        // its retained lock clone at construction—not only after worker
        // admission—so cancellation of this async command cannot release
        // exclusivity beneath an orphaned DB write.
        process_lock.clone(),
    );

    tracing::info!(
        http_addr = %config.http_addr,
        data_dir = %config.data_dir,
        eth_rpc_url = %l1_config.eth_rpc_url,
        input_box_address = %l1_config.identity.input_box_address,
        app_deployment_block = l1_config.identity.app_deployment_block,
        chain_id = l1_config.identity.chain_id,
        app_address = %l1_config.identity.app_address,
        batch_submitter_address = %l1_config.identity.batch_submitter_address,
        max_wait_blocks = timing.max_wait_blocks,
        preemptive_margin_blocks = timing.preemptive_margin_blocks,
        danger_threshold = timing.danger_threshold(),
        "sequencer startup"
    );

    // ── Recovery reducer ─────────────────────────────────────
    // Local terminal facts are inspected before the first provider call. A
    // completed phase always returns through the same reducer before another
    // phase or admission.
    crate::recovery::run_startup_recovery(&db_path, &mut input_reader, &l1_config, &timing).await?;

    // Setup persisted a real first price, so run performs no synchronous
    // fee-source I/O and fee availability never precedes the reducer's local
    // terminal-fact inspection. The exhaustive identity match is the worker
    // launch decision: fixed mode has no task; Uniswap's supervised worker
    // performs the first runtime quote after admission.
    let fee_oracle = match l1_config.identity.fee_oracle {
        storage::FeeOracleIdentity::Fixed { .. } => None,
        storage::FeeOracleIdentity::Uniswap {
            weth,
            fee_token,
            pool,
            twap_window_secs,
        } => {
            let uniswap = crate::l1::fee_oracle::UniswapConfig {
                chain_id: l1_config.identity.chain_id,
                weth,
                fee_token,
                pool,
                twap_window_secs,
            };
            let provider = crate::l1::provider::create_provider(
                &config.eth_rpc_url,
                config.allow_insecure_rpc,
            )
            .map_err(crate::l1::fee_oracle::worker::FeeOracleError::Misconfig)?;
            let token = crate::l1::fee_oracle::UniswapV3PriceSource::from_setup_validated(
                provider.clone(),
                uniswap,
            );
            Some(crate::l1::fee_oracle::FeeOracle::new(
                db_path.clone(),
                Duration::from_millis(config.fee_oracle.poll_interval_ms),
                provider,
                Box::new(token),
                process_lock.clone(),
            ))
        }
    };

    // ── Prepare → admit → launch ─────────────────────────────
    let prepared = PreparedRuntime::<A>::prepare(WorkersConfig {
        run_config: config,
        l1_config,
        timing,
        input_reader,
        fee_oracle,
        process_lock,
    })
    .await?;

    // Preparation may take long enough for a clock/view refusal to arise.
    // Reinvoke the same reducer over one consistent fact set; workers are
    // never launched from an aged decision.
    let admission = crate::recovery::admit_runtime(&db_path, &timing)?;
    let mut workers = prepared.launch(admission);

    let first_exit = workers.select_first_exit().await;
    workers.finish(first_exit).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::{record_canonical_divergence, temp_db};

    #[test]
    fn clean_exit_over_persisted_divergence_is_terminal() {
        // The one exit code that breaks the supervisor's rediscovery chain
        // is 0: divergence recorded during the run must survive a clean
        // drain as a terminal verdict, not a normal shutdown.
        let db = temp_db("run-clean-exit-divergence");
        let mut storage =
            storage::Storage::initialize_for_command(&db.path, LifecycleCommand::Setup)
                .expect("initialize");
        record_canonical_divergence(&mut storage, 7, 0);
        drop(storage);

        let error = refuse_divergence_on_clean_exit(&db.path)
            .expect_err("a clean drain over divergence must not exit 0");
        assert_eq!(
            error.exit_code(),
            crate::commands::error::EXIT_TERMINAL,
            "divergence on the clean path pages"
        );

        let clean = temp_db("run-clean-exit-clean");
        storage::Storage::initialize_for_command(&clean.path, LifecycleCommand::Setup)
            .expect("initialize clean");
        refuse_divergence_on_clean_exit(&clean.path).expect("no divergence passes clean");
    }
}
