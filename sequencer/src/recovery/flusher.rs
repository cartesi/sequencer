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
use alloy::providers::{
    DynProvider, PendingTransactionConfig, PendingTransactionError, Provider, WatchTxError,
};
use alloy::rpc::types::BlockNumberOrTag;
use alloy_primitives::{Address, B256, U256};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info};

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

/// Derive the flusher's watch/poll durations from the configured block time.
///
/// `confirmation_timeout` is 10 blocks — long enough to survive one-off L1
/// stalls but short enough to retry within a reasonable window.
/// `safe_poll_interval` is one block — matches the natural cadence for
/// `get_transaction_count(Safe)` to advance.
///
/// H6 regression: both values must scale with `SEQ_SECONDS_PER_BLOCK`; a fixed
/// 12s assumption would mis-pace on non-mainnet chains.
fn derive_timeouts(seconds_per_block: u64) -> (Duration, Duration) {
    (
        Duration::from_secs(10 * seconds_per_block),
        Duration::from_secs(seconds_per_block),
    )
}

/// Bump base 1559 fees to satisfy Ethereum's transaction replacement rule
/// (EIP-1559 §Replacement, ≥10% bump on both `max_fee_per_gas` and
/// `max_priority_fee_per_gas`).
///
/// H5 regression: a replacement no-op must out-bid any pending batch tx at the
/// same nonce to guarantee slot consumption. The `+ 1` on `max_fee` handles the
/// edge case where `base * 11 / 10` equals `base * 11 / 10` after integer
/// rounding; the priority doubling is generous but preserves the invariant.
fn bumped_replacement_fees(base_max_fee: u128, base_priority_fee: u128) -> (u128, u128) {
    let new_max_fee = base_max_fee.saturating_mul(11) / 10 + 1;
    let new_priority_fee = base_priority_fee.saturating_mul(2).max(1);
    (new_max_fee, new_priority_fee)
}

fn send_failures_error(failures: &[(u64, String)]) -> FlushError {
    const MAX_SAMPLES: usize = 3;

    let samples = failures
        .iter()
        .take(MAX_SAMPLES)
        .map(|(nonce, message)| format!("nonce {nonce}: {message}"))
        .collect::<Vec<_>>()
        .join("; ");
    let remaining = failures.len().saturating_sub(MAX_SAMPLES);
    let suffix = if remaining == 0 {
        String::new()
    } else {
        format!("; ... and {remaining} more")
    };

    FlushError::Provider(format!(
        "failed to submit {} flush no-op transaction(s): {samples}{suffix}",
        failures.len()
    ))
}

fn map_watch_error(err: PendingTransactionError) -> Result<bool, FlushError> {
    match err {
        PendingTransactionError::TxWatcher(WatchTxError::Timeout) => Ok(false),
        other => Err(FlushError::Provider(other.to_string())),
    }
}

impl MempoolFlusher {
    pub fn new(provider: DynProvider, address: Address, seconds_per_block: u64) -> Self {
        let (confirmation_timeout, safe_poll_interval) = derive_timeouts(seconds_per_block);
        Self {
            provider,
            address,
            confirmation_timeout,
            safe_poll_interval,
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

        let (bumped_max_fee, bumped_priority_fee) =
            bumped_replacement_fees(fees.max_fee_per_gas, fees.max_priority_fee_per_gas);

        debug!(
            from_nonce,
            to_nonce,
            count = to_nonce - from_nonce,
            max_fee_per_gas = bumped_max_fee,
            max_priority_fee = bumped_priority_fee,
            "submitting flush no-ops"
        );

        let mut tx_hashes = Vec::new();
        let mut send_failures = Vec::new();
        for nonce in from_nonce..to_nonce {
            let tx = alloy::rpc::types::TransactionRequest::default()
                .with_to(self.address)
                .with_value(U256::ZERO)
                .with_nonce(nonce)
                .with_max_fee_per_gas(bumped_max_fee)
                .with_max_priority_fee_per_gas(bumped_priority_fee);

            match self.provider.send_transaction(tx).await {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    debug!(nonce, %tx_hash, "flush no-op submitted");
                    tx_hashes.push(tx_hash);
                }
                Err(e) => {
                    let message = e.to_string();
                    error!(nonce, error = %message, "flush no-op send failed");
                    send_failures.push((nonce, message));
                }
            }
        }

        if !send_failures.is_empty() {
            return Err(send_failures_error(send_failures.as_slice()));
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
                Err(err @ PendingTransactionError::TxWatcher(WatchTxError::Timeout)) => {
                    // This should not happen during normal L1 operation.
                    // Possible causes: L1 congestion, tx dropped from mempool,
                    // gas price too low to compete.
                    error!(
                        %tx_hash,
                        timeout_secs = self.confirmation_timeout.as_secs(),
                        "flush no-op timed out waiting for L1 inclusion — will retry"
                    );
                    return map_watch_error(err);
                }
                Err(err) => return map_watch_error(err),
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

    // ── H5: replacement-fee bump satisfies EIP-1559 rules ─────────

    #[test]
    fn replacement_fee_bump_exceeds_ten_percent_for_max_fee() {
        // `max_fee_per_gas` must strictly exceed base by ≥10% for any positive base.
        for base in [1_u128, 10, 100, 1_000, 1_000_000, 1_000_000_000_000] {
            let (new_max, _) = bumped_replacement_fees(base, 0);
            assert!(
                new_max.saturating_mul(10) >= base.saturating_mul(11),
                "max_fee bump violates ≥10% rule: base={base}, new={new_max}",
            );
        }
    }

    #[test]
    fn replacement_fee_bump_doubles_priority_fee() {
        // `priority_fee` doubles (200%), easily clearing the 10% replacement threshold.
        for base in [1_u128, 10, 1_000, 1_000_000_000] {
            let (_, new_prio) = bumped_replacement_fees(0, base);
            assert_eq!(new_prio, base.saturating_mul(2));
            assert!(
                new_prio.saturating_mul(10) >= base.saturating_mul(11),
                "priority bump violates ≥10% rule: base={base}, new={new_prio}",
            );
        }
    }

    #[test]
    fn replacement_fee_floor_is_positive_even_when_base_is_zero() {
        // If the estimator returns zero, bumped values are still positive so the
        // tx is actually broadcast rather than rejected by the node.
        let (new_max, new_prio) = bumped_replacement_fees(0, 0);
        assert!(new_max >= 1);
        assert!(new_prio >= 1);
    }

    #[test]
    fn send_failure_error_summarizes_failed_slots() {
        let err = send_failures_error(&[
            (7, "nonce too low".to_string()),
            (8, "replacement transaction underpriced".to_string()),
            (9, "insufficient funds".to_string()),
            (10, "fee cap less than block base fee".to_string()),
        ]);

        let message = err.to_string();
        assert!(message.contains("failed to submit 4 flush no-op transaction(s)"));
        assert!(message.contains("nonce 7: nonce too low"));
        assert!(message.contains("nonce 8: replacement transaction underpriced"));
        assert!(message.contains("nonce 9: insufficient funds"));
        assert!(message.contains("and 1 more"));
        assert!(!message.contains("nonce 10"));
    }

    #[test]
    fn watch_error_mapping_retries_only_timeouts() {
        let timeout = map_watch_error(PendingTransactionError::TxWatcher(WatchTxError::Timeout))
            .expect("timeout should be a retryable watch result");
        assert!(!timeout, "timeout should ask the caller to retry");

        let err = map_watch_error(PendingTransactionError::FailedToRegister)
            .expect_err("non-timeout watcher failures must surface");
        assert!(matches!(err, FlushError::Provider(_)));
    }

    #[test]
    fn replacement_fee_bump_saturates_at_u128_max() {
        // Overflow safety: astronomical base fees must not wrap around.
        let (new_max, new_prio) = bumped_replacement_fees(u128::MAX, u128::MAX);
        assert_eq!(new_max, u128::MAX / 10 + 1);
        assert_eq!(new_prio, u128::MAX);
    }

    // ── H6: timeouts derive from seconds_per_block ────────────────

    #[test]
    fn timeouts_derive_from_seconds_per_block() {
        assert_eq!(
            derive_timeouts(12),
            (Duration::from_secs(120), Duration::from_secs(12)),
            "mainnet 12s block: 120s confirmation, 12s poll",
        );
        assert_eq!(
            derive_timeouts(2),
            (Duration::from_secs(20), Duration::from_secs(2)),
            "fast L2 2s block: scaled proportionally",
        );
        assert_eq!(
            derive_timeouts(1),
            (Duration::from_secs(10), Duration::from_secs(1)),
            "minimum accepted block time (H8: SEQ_SECONDS_PER_BLOCK >= 1)",
        );
    }

    #[test]
    fn confirmation_timeout_is_ten_times_safe_poll_interval() {
        // Structural invariant: confirmation window == 10 × poll interval.
        for spb in [1_u64, 2, 5, 12, 30] {
            let (conf, poll) = derive_timeouts(spb);
            assert_eq!(conf, poll * 10);
        }
    }

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

        let flusher = MempoolFlusher::new(provider, addr, 12);
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
        let flusher = MempoolFlusher::new(provider.clone(), addr, 12)
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
        let flusher = MempoolFlusher::new(provider.clone(), addr, 12)
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

    // ── flusher under extended provider outage ──────────────────────────
    //
    // Implementation note (matters for what this test pins): `flush_and_wait`
    // does NOT retry internally on `Provider` errors — a failed `nonce_at`
    // call propagates via `?` and the function returns. "Retry forever" is
    // really the orchestrator's restart loop: on each respawn a fresh flusher
    // is constructed and tried, and this repeats until the provider becomes
    // reachable again. The e2e suite covers that orchestrator-loop story via
    // `respawn_until_stable`.
    //
    // This test pins the two ends of that contract: (a) a mid-flush
    // disconnect surfaces as `FlushError::Provider` fast (no hang, no
    // internal retry), and (b) a fresh flusher call after reconnect
    // completes and consumes the pending wallet-nonce slot.

    #[tokio::test]
    async fn flush_surfaces_provider_error_under_disconnect_and_completes_on_reconnect() {
        use rollups_harness::TcpProxy;

        require_anvil();

        let anvil = spawn_anvil();
        // Direct-to-Anvil provider: the test uses this to seed pending
        // mempool state and inspect the chain. Bypasses the proxy so the
        // seeding itself isn't affected by disconnect.
        let direct_provider = signer_provider(&anvil);
        let addr = anvil.addresses()[0];

        // Proxy in front of Anvil — this is what the flusher dials. Anvil's
        // endpoint uses `localhost` which the proxy's upstream parser rejects
        // (it expects a literal IP). Swap for `127.0.0.1` so `parse` accepts.
        let anvil_upstream = anvil.endpoint().replace("localhost", "127.0.0.1");
        let proxy = TcpProxy::spawn(anvil_upstream.as_str())
            .await
            .expect("spawn proxy");

        let key_hex = alloy_primitives::hex::encode(anvil.first_key().to_bytes());
        let proxied_provider = crate::l1::provider::create_signer_provider(
            proxy.endpoint().as_str(),
            &format!("0x{key_hex}"),
        )
        .expect("create signer provider through proxy");

        // Seed: submit a tx at wallet-nonce 0 into Anvil's mempool (auto-
        // mining is off, so it stays pending). The flusher now has work.
        send_tx_at_nonce(&direct_provider, addr, 0).await;
        let pending = direct_provider
            .get_transaction_count(addr)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        assert_eq!(pending, 1, "seed tx should be pending");

        // Disconnect the proxy. The flusher's provider can no longer reach
        // Anvil — any RPC call sees a torn-down TCP connection.
        proxy.disconnect();
        let flusher = MempoolFlusher::new(proxied_provider.clone(), addr, 12)
            .with_timeouts(Duration::from_secs(2), Duration::from_millis(200));

        // `flush_and_wait` must fail fast (no internal retry loop). Wrap in
        // a generous outer timeout just to bound test flakiness if alloy's
        // HTTP client has small internal retries.
        let err = tokio::time::timeout(Duration::from_secs(5), flusher.flush_and_wait())
            .await
            .expect("flush_and_wait must not hang under disconnect")
            .expect_err("flush_and_wait must surface a Provider error under disconnect");
        assert!(
            matches!(err, FlushError::Provider(_)),
            "expected FlushError::Provider, got: {err:?}",
        );

        // Reconnect the proxy + start mining so the flusher can make forward
        // progress. This models the orchestrator's next respawn succeeding
        // after the provider returns.
        proxy.reconnect();
        let _miner = start_miner(direct_provider.clone(), Duration::from_millis(100));

        // A fresh flusher (a respawn would build a new one from scratch).
        // It should now read nonces, replace the pending tx with a bumped-
        // fee no-op (or let the original land), wait for safe, and return.
        let flusher_after = MempoolFlusher::new(proxied_provider, addr, 12)
            .with_timeouts(Duration::from_secs(5), Duration::from_millis(200));
        tokio::time::timeout(Duration::from_secs(15), flusher_after.flush_and_wait())
            .await
            .expect("flush_and_wait should complete after reconnect")
            .expect("flush should succeed once the provider is reachable");

        // Forward progress: the nonce-0 slot was consumed (either by the
        // flusher's no-op or by the original tx landing). `safe_nonce` is
        // >= 1 only if something at nonce 0 reached safe finality — proof
        // the flusher completed its job end-to-end.
        let safe_after = direct_provider
            .get_transaction_count(addr)
            .block_id(BlockNumberOrTag::Safe.into())
            .await
            .expect("safe nonce after flush");
        assert!(
            safe_after >= 1,
            "nonce-0 slot must be consumed and safe after flush, got {safe_after}",
        );

        proxy.shutdown().await.expect("proxy shutdown");
    }
}
