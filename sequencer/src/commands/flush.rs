// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The `flush-mempool` subcommand: settle the batch-submitter
//! wallet nonce on demand — submit no-ops for every unresolved slot and wait
//! until `pending <= safe` and `safe >= watermark + 1`.
//!
//! Operator tooling (unstick a wedged nonce, prep before a decommission) and
//! the same flush `setup --recovery` will run internally. It is a keyed L1
//! write, so it needs the signing key; it reads the submitter address and the
//! wallet-nonce watermark from the DB, so it refuses unless `setup` completed.
//! Flush-only — it does not sync or cascade (those stay in normal-run
//! recovery), and it requires a completed setup. A successful wallet flush
//! proves nothing about the rest of the runtime and is never treated as if
//! it did.

use super::load_setup_identity;
use crate::commands::config::FlushConfig;
use crate::commands::error::CommandError;
use crate::recovery::MempoolFlusher;
use crate::storage::{self, LifecycleCommand};

pub async fn flush_mempool(config: FlushConfig) -> Result<(), CommandError> {
    std::fs::create_dir_all(&config.data_dir)?;
    // Exclusive process ownership: a flush must never broadcast beside a
    // live sequencer (or another flush) reading the same watermark.
    let _process_lock = crate::runtime::process_lock::ProcessLock::acquire(&config.data_dir)?;
    let db_path = config.db_path();

    super::preflight_lifecycle_command(&db_path, LifecycleCommand::MaintenanceFlush)?;
    let identity = load_setup_identity(&db_path)?;

    // The signing key must match the pinned submitter — flushing under the
    // wrong key would settle the wrong account's nonce.
    let key = super::verify_submitter_key(config.resolve_private_key()?, &identity)?;

    let result = flush_mempool_admitted(config, identity, key).await;
    // Verdict-neutral black-box settlement.
    super::record_terminal_fault_best_effort(&db_path, LifecycleCommand::MaintenanceFlush, &result);
    result
}

async fn flush_mempool_admitted(
    config: FlushConfig,
    identity: storage::DeploymentIdentity,
    key: String,
) -> Result<(), CommandError> {
    let db_path = config.db_path();

    // The durable flush anchor: every slot we ever broadcast
    // must resolve at safe depth, regardless of the local pool's memory.
    let watermark = {
        let mut storage = storage::Storage::open_writer(&db_path)?;
        storage.wallet_nonce_watermark()?
    };

    // Wrong-chain RPC guard: flush broadcasts keyed L1 txs, so —
    // like `setup` and `run` — it must confirm the RPC's chain id matches the
    // pinned one before signing, or it would burn submitter nonce slots on the
    // wrong chain. `create_verified_signer_provider` folds that check into the
    // signer build (the one guarded keyed-write entry point); a mismatch is
    // terminal (operator misconfig), an RPC error retryable (flush needs L1
    // reachable anyway).
    let provider = crate::l1::provider::create_verified_signer_provider(
        &config.eth_rpc_url,
        &key,
        identity.chain_id,
        config.allow_insecure_rpc,
    )
    .await
    .map_err(CommandError::from)?;
    let safe_block = MempoolFlusher::flush_to_safe(
        provider,
        identity.batch_submitter_address,
        config.seconds_per_block,
        db_path,
        watermark,
    )
    .await?;
    tracing::info!(
        safe_block,
        batch_submitter_address = %identity.batch_submitter_address,
        "flush-mempool complete — wallet nonce settled"
    );
    Ok(())
}
