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
//! Flush-only — it does not cascade (that stays in preemptive recovery).

use super::config::FlushConfig;
use super::{BootstrapError, RunError, load_setup_identity};
use crate::l1::provider::VerifiedSignerProviderError;
use crate::recovery::MempoolFlusher;
use crate::storage;

pub async fn flush_mempool(config: FlushConfig) -> Result<(), RunError> {
    let db_path = config.db_path();

    // Gate on a completed setup and read the pinned submitter address.
    let identity = load_setup_identity(&db_path)?;

    // The signing key must match the pinned submitter — flushing under the
    // wrong key would settle the wrong account's nonce.
    let key = super::verify_submitter_key(config.resolve_private_key()?, &identity)?;

    // The durable flush anchor (review R1a): every slot we ever broadcast
    // must resolve at safe depth, regardless of the local pool's memory.
    let watermark = {
        let mut storage = storage::Storage::open(&db_path)?;
        storage.wallet_nonce_watermark()?
    };

    // Wrong-chain RPC guard (review F6): flush broadcasts keyed L1 txs, so —
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
