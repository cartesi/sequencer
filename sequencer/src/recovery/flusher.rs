// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Mempool flusher: submits no-op transactions to resolve pending wallet-nonce slots
//! before recovery runs.
//!
//! After a danger-zone detection, the sequencer goes offline and calls
//! [`MempoolFlusher::flush_and_wait`] to ensure every `w_nonce` slot is consumed
//! (either by its original batch transaction or by a replacement no-op). Once all
//! slots reach safe finality, the recovery procedure can read fully-finalized L1 state.

use alloy::network::TransactionBuilder;
use alloy::providers::{DynProvider, PendingTransactionConfig, PendingTransactionError, Provider};
use alloy::rpc::types::BlockNumberOrTag;
use alloy_primitives::{Address, B256, U256};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info};

/// Conservative timeout for waiting on tx inclusion.
/// Uses Ethereum's 12s block time as a worst-case heuristic.
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(10 * 12);

/// Sleep between outer-loop iterations to let the safe head advance.
const SAFE_HEAD_POLL_INTERVAL: Duration = Duration::from_secs(12);

#[derive(Debug, Error)]
pub enum FlushError {
    #[error("provider/transport: {0}")]
    Provider(String),
}

pub struct MempoolFlusher {
    provider: DynProvider,
    address: Address,
    confirmation_timeout: Duration,
    safe_poll_interval: Duration,
}

impl MempoolFlusher {
    pub fn new(provider: DynProvider, address: Address) -> Self {
        Self {
            provider,
            address,
            confirmation_timeout: CONFIRMATION_TIMEOUT,
            safe_poll_interval: SAFE_HEAD_POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    fn with_timeouts(
        mut self,
        confirmation_timeout: Duration,
        safe_poll_interval: Duration,
    ) -> Self {
        self.confirmation_timeout = confirmation_timeout;
        self.safe_poll_interval = safe_poll_interval;
        self
    }

    /// Flush the mempool by submitting no-op transactions for all pending nonce slots,
    /// then waiting for safe finality on all of them.
    ///
    /// The loop runs until `get_transaction_count(Pending) <= get_transaction_count(Safe)`,
    /// meaning every slot has reached safe finality.
    ///
    /// At each iteration:
    /// 1. Submit 0-ETH self-transfers for nonces between `Latest` and `Pending`.
    ///    These compete with any batch transactions still in the mempool.
    /// 2. Watch each submitted tx for L1 inclusion (same pattern as batch poster).
    /// 3. Sleep to let the safe head advance, then re-check the loop condition.
    /// 4. If any watch times out, retry the outer loop (tx may have been dropped).
    pub async fn flush_and_wait(&self) -> Result<(), FlushError> {
        let mut attempt = 0u32;
        loop {
            let safe_nonce = self.nonce_at(BlockNumberOrTag::Safe).await?;
            let pending_nonce = self.nonce_at(BlockNumberOrTag::Pending).await?;

            if pending_nonce <= safe_nonce {
                info!(
                    safe_nonce,
                    "mempool flush complete — all slots reached safe finality"
                );
                return Ok(());
            }

            let unresolved = pending_nonce - safe_nonce;

            if attempt == 0 {
                info!(
                    safe_nonce,
                    pending_nonce,
                    unresolved,
                    "flushing mempool: submitting no-ops for unresolved w_nonce slots"
                );
            } else {
                // Retry after a previous timeout — re-print status so operators
                // see the current state without scrolling back.
                error!(
                    attempt,
                    safe_nonce,
                    pending_nonce,
                    unresolved,
                    "flush retry: previous attempt timed out, resubmitting"
                );
            }
            attempt += 1;

            // Submit no-ops for nonces between Latest and Pending.
            let latest_nonce = self.nonce_at(BlockNumberOrTag::Latest).await?;
            let tx_hashes = self.submit_noops(latest_nonce, pending_nonce).await?;

            // Watch each submitted tx for L1 inclusion.
            if !self.watch_txs(&tx_hashes).await? {
                continue;
            }

            // Sleep to let the safe head catch up before re-checking.
            tokio::time::sleep(self.safe_poll_interval).await;
        }
    }

    /// Submit 0-ETH self-transfers for nonces `from_nonce..to_nonce`.
    /// Returns the tx hashes of successfully submitted transactions.
    async fn submit_noops(&self, from_nonce: u64, to_nonce: u64) -> Result<Vec<B256>, FlushError> {
        if from_nonce >= to_nonce {
            return Ok(Vec::new());
        }

        let fees = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(|e| FlushError::Provider(e.to_string()))?;

        debug!(
            from_nonce,
            to_nonce,
            count = to_nonce - from_nonce,
            max_fee_per_gas = fees.max_fee_per_gas,
            max_priority_fee = fees.max_priority_fee_per_gas.saturating_mul(2).max(1),
            "submitting flush no-ops"
        );

        let mut tx_hashes = Vec::new();
        for nonce in from_nonce..to_nonce {
            let tx = alloy::rpc::types::TransactionRequest::default()
                .with_to(self.address)
                .with_value(U256::ZERO)
                .with_nonce(nonce)
                .with_max_fee_per_gas(fees.max_fee_per_gas)
                // Elevated tip to compete with batch txs in the mempool.
                .with_max_priority_fee_per_gas(
                    fees.max_priority_fee_per_gas.saturating_mul(2).max(1),
                );

            match self.provider.send_transaction(tx).await {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    debug!(nonce, %tx_hash, "flush no-op submitted");
                    tx_hashes.push(tx_hash);
                }
                Err(e) => {
                    // Nonce already consumed (tx confirmed between our read and submit).
                    // This is expected and safe to ignore.
                    debug!(nonce, error = %e, "flush no-op send failed (slot likely already consumed)");
                }
            }
        }

        Ok(tx_hashes)
    }

    /// Watch submitted transactions for L1 inclusion.
    /// Uses the same `PendingTransactionConfig::watch` pattern as the batch poster.
    /// Returns `true` if all txs confirmed, `false` on timeout.
    async fn watch_txs(&self, tx_hashes: &[B256]) -> Result<bool, FlushError> {
        for tx_hash in tx_hashes {
            let watch = PendingTransactionConfig::new(*tx_hash)
                .with_required_confirmations(1)
                .with_timeout(Some(self.confirmation_timeout))
                .with_provider(self.provider.root().clone());
            match watch.watch().await {
                Ok(_) => {
                    debug!(%tx_hash, "flush no-op included on L1");
                }
                Err(PendingTransactionError::TxWatcher(
                    alloy::providers::WatchTxError::Timeout,
                )) => {
                    // This should not happen during normal L1 operation.
                    // Possible causes: L1 congestion, tx dropped from mempool,
                    // gas price too low to compete.
                    error!(
                        %tx_hash,
                        timeout_secs = self.confirmation_timeout.as_secs(),
                        "flush no-op timed out waiting for L1 inclusion — will retry"
                    );
                    return Ok(false);
                }
                Err(err) => {
                    // Tx may have been replaced by the original batch tx winning the slot.
                    // This is expected — the slot is consumed either way.
                    debug!(%tx_hash, error = %err, "flush no-op watch ended (slot likely consumed by original batch)");
                }
            }
        }
        Ok(true)
    }

    async fn nonce_at(&self, block: BlockNumberOrTag) -> Result<u64, FlushError> {
        self.provider
            .get_transaction_count(self.address)
            .block_id(block.into())
            .await
            .map_err(|e| FlushError::Provider(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::network::TransactionBuilder;
    use alloy::node_bindings::Anvil;
    use alloy::providers::Provider;

    /// Verify that `anvil` is available. Panics with a clear message if not found.
    fn require_anvil() {
        assert!(
            std::process::Command::new("anvil")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok(),
            "anvil not found on PATH — install Foundry (https://getfoundry.sh)"
        );
    }

    /// Spawn Anvil with manual mining and fast safe-finality (2 slots/epoch).
    fn spawn_anvil() -> alloy::node_bindings::AnvilInstance {
        Anvil::default()
            .arg("--no-mining")
            .arg("--slots-in-an-epoch")
            .arg("2")
            .timeout(30_000)
            .spawn()
    }

    /// Create a signer provider from an Anvil private key.
    fn signer_provider(anvil: &alloy::node_bindings::AnvilInstance) -> DynProvider {
        let key_hex = alloy_primitives::hex::encode(anvil.first_key().to_bytes());
        crate::l1::provider::create_signer_provider(
            anvil.endpoint_url().as_str(),
            &format!("0x{key_hex}"),
        )
        .expect("create signer provider")
    }

    /// Mine blocks at a fixed interval until the token is dropped.
    fn start_miner(provider: DynProvider, interval: Duration) -> tokio::sync::oneshot::Sender<()> {
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = tokio::time::sleep(interval) => {
                        let _ = provider.raw_request::<_, serde_json::Value>(
                            "evm_mine".into(), ()).await;
                    }
                }
            }
        });
        stop_tx
    }

    /// Send a 0-ETH self-transfer at a specific nonce (without waiting for inclusion).
    async fn send_tx_at_nonce(provider: &DynProvider, addr: Address, nonce: u64) {
        let fees = provider
            .estimate_eip1559_fees()
            .await
            .expect("estimate fees");
        let tx = alloy::rpc::types::TransactionRequest::default()
            .with_to(addr)
            .with_value(U256::ZERO)
            .with_nonce(nonce)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        let _ = provider.send_transaction(tx).await.expect("send tx");
    }

    #[tokio::test]
    async fn flush_is_noop_when_no_pending_nonces() {
        require_anvil();

        let anvil = spawn_anvil();
        let provider = signer_provider(&anvil);
        let addr = anvil.addresses()[0];

        // Mine a few blocks so safe head advances past genesis.
        for _ in 0..4 {
            let _: serde_json::Value = provider
                .raw_request("evm_mine".into(), ())
                .await
                .expect("mine");
        }

        let flusher = MempoolFlusher::new(provider, addr);
        // No pending txs — should return immediately.
        flusher.flush_and_wait().await.expect("flush");
    }

    #[tokio::test]
    async fn flush_resolves_pending_nonces_to_safe() {
        require_anvil();

        let anvil = spawn_anvil();
        let provider = signer_provider(&anvil);
        let addr = anvil.addresses()[0];

        // Send 3 txs into the mempool (unmined).
        for nonce in 0..3 {
            send_tx_at_nonce(&provider, addr, nonce).await;
        }

        // Verify: pending=3, safe=0.
        let pending = provider
            .get_transaction_count(addr)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        assert_eq!(pending, 3);

        let safe = provider
            .get_transaction_count(addr)
            .block_id(BlockNumberOrTag::Safe.into())
            .await
            .expect("safe nonce");
        assert_eq!(safe, 0);

        // Start a background miner so blocks are produced.
        let _miner = start_miner(provider.clone(), Duration::from_millis(100));

        // Run the flusher — it should resolve all 3 nonces to safe.
        let flusher = MempoolFlusher::new(provider.clone(), addr)
            .with_timeouts(Duration::from_secs(5), Duration::from_millis(200));
        tokio::time::timeout(Duration::from_secs(10), flusher.flush_and_wait())
            .await
            .expect("flush should complete within timeout")
            .expect("flush should succeed");

        // Verify: safe nonce caught up.
        let safe_after = provider
            .get_transaction_count(addr)
            .block_id(BlockNumberOrTag::Safe.into())
            .await
            .expect("safe nonce after flush");
        assert!(
            safe_after >= 3,
            "safe nonce should be >= 3 after flush, got {safe_after}"
        );
    }

    #[tokio::test]
    async fn flush_handles_already_mined_but_not_safe() {
        require_anvil();

        let anvil = spawn_anvil();
        let provider = signer_provider(&anvil);
        let addr = anvil.addresses()[0];

        // Send 2 txs and mine them (latest but not safe).
        for nonce in 0..2 {
            send_tx_at_nonce(&provider, addr, nonce).await;
        }
        let _: serde_json::Value = provider
            .raw_request("evm_mine".into(), ())
            .await
            .expect("mine");

        let latest = provider
            .get_transaction_count(addr)
            .block_id(BlockNumberOrTag::Latest.into())
            .await
            .expect("latest nonce");
        assert_eq!(latest, 2, "txs should be mined");

        let safe = provider
            .get_transaction_count(addr)
            .block_id(BlockNumberOrTag::Safe.into())
            .await
            .expect("safe nonce");
        assert_eq!(safe, 0, "txs should not be safe yet");

        // Start miner to advance safe head.
        let _miner = start_miner(provider.clone(), Duration::from_millis(100));

        // Flusher should wait for safe finality (no new txs to submit).
        let flusher = MempoolFlusher::new(provider.clone(), addr)
            .with_timeouts(Duration::from_secs(5), Duration::from_millis(200));
        tokio::time::timeout(Duration::from_secs(10), flusher.flush_and_wait())
            .await
            .expect("flush should complete within timeout")
            .expect("flush should succeed");

        let safe_after = provider
            .get_transaction_count(addr)
            .block_id(BlockNumberOrTag::Safe.into())
            .await
            .expect("safe nonce after flush");
        assert!(
            safe_after >= 2,
            "safe nonce should be >= 2 after flush, got {safe_after}"
        );
    }
}
