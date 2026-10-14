// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The `flush-mempool` command: settle the batch-submitter wallet nonce on demand
//! with the same flush startup recovery and `setup --recovery` run. It needs the
//! signing key and a completed setup, and never syncs, cascades or launches
//! workers (README "Running").

use super::load_setup_identity;
use crate::commands::config::FlushConfig;
use crate::commands::error::CommandError;
use crate::recovery::MempoolFlusher;
use crate::storage::{self, LifecycleCommand};

pub async fn flush_mempool(config: FlushConfig) -> Result<(), CommandError> {
    super::require_data_dir(&config.data_dir)?;
    // Exclusive process ownership: a flush must never broadcast beside a
    // live sequencer (or another flush) reading the same watermark.
    let _process_lock = crate::runtime::process_lock::ProcessLock::acquire(&config.data_dir)?;
    let db_path = config.db_path();

    super::preflight_lifecycle_command(&config.data_dir, LifecycleCommand::MaintenanceFlush)?;
    let identity = load_setup_identity(&config.data_dir)?;

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
    key: crate::l1::SubmitterKey,
) -> Result<(), CommandError> {
    let db_path = config.db_path();

    // The durable flush anchor: every slot we ever broadcast
    // must resolve at safe depth, regardless of the local pool's memory.
    let watermark = {
        let mut storage = storage::Storage::open_writer(&db_path)?;
        storage.wallet_nonce_watermark()?
    };

    // Chain-id check before signing: `create_verified_keyed_providers`.
    let providers = crate::l1::provider::create_verified_keyed_providers(
        &config.rpc.eth_rpc_url,
        key.expose_secret(),
        identity.chain_id,
        config.rpc.allow_insecure_rpc,
    )
    .await
    .map_err(CommandError::from)?;
    let safe_block = MempoolFlusher::flush_to_safe(
        providers,
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
