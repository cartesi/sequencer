// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy::providers::{
    DynProvider, PendingTransactionBuilder, PendingTransactionConfig, PendingTransactionError,
    Provider,
};
use alloy::rpc::types::BlockNumberOrTag;
use async_trait::async_trait;
use cartesi_rollups_contracts::input_box::InputBox;
use sequencer_core::batch::Batch;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::l1::eip1559::{
    Eip1559Fees, FeesForNonce, estimate_fees, fees_for_nonce, pad_gas_estimate,
};
use crate::l1::partition::{decode_evm_advance_input, get_input_added_events_ordered};
use crate::l1::watermark::WalletNonceWatermarkSink;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub type TxHash = alloy_primitives::B256;

#[derive(Debug, Clone)]
pub struct BatchPosterConfig {
    pub l1_submit_address: alloy_primitives::Address,
    pub app_address: alloy_primitives::Address,
    pub batch_submitter_address: alloy_primitives::Address,
    pub start_block: u64,
    pub confirmation_depth: u64,
    /// Assumed L1 block time in seconds, used to derive a conservative
    /// confirmation timeout for watched batch-submission txs.
    pub seconds_per_block: u64,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    pub long_block_range_error_codes: Vec<String>,
    /// The pinned deployment chain id. Re-confirmed against the RPC immediately
    /// before every productive send (`submit_batches`), so a long-lived
    /// submitter whose load-balanced RPC fails over to another chain refuses to
    /// burn nonce slots on it rather than relying only on the one-shot boot /
    /// reader checks.
    pub expected_chain_id: u64,
}

#[derive(Debug, Error)]
pub enum BatchPosterError {
    #[error("provider/transport: {0}")]
    Provider(String),
    #[error("rpc chain id {rpc} does not match pinned chain id {expected}")]
    ChainIdMismatch { rpc: u64, expected: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitBatchesOutcome {
    /// All sends progressed normally.
    Submitted(Vec<TxHash>),
    /// At least one nonce is in fee-ceiling hold: still broadcast as a mempool
    /// probe and wrote no floor. This is not an internal retry loop; the outer
    /// tick sleeps on the confirmation cadence.
    Held(Vec<TxHash>),
}

#[async_trait]
pub trait BatchPoster: Send + Sync {
    /// Broadcast the payloads as L1 txs at consecutive wallet nonces.
    /// Implementations must raise `watermark` to the highest nonce they
    /// are about to use *before* the first send (write-before-broadcast,
    /// review R1a).
    async fn submit_batches(
        &self,
        payloads: Vec<Vec<u8>>,
        watermark: &dyn WalletNonceWatermarkSink,
    ) -> Result<SubmitBatchesOutcome, BatchPosterError>;

    async fn observed_submitted_batch_nonces(
        &self,
        from_block: u64,
    ) -> Result<Vec<u64>, BatchPosterError>;
}

#[derive(Clone)]
pub struct EthereumBatchPoster {
    provider: DynProvider,
    config: BatchPosterConfig,
    /// Fees of the last successful broadcast (or underpriced raise) per wallet
    /// nonce still ≥ Latest.
    ///
    /// Same-nonce retries floor a fresh estimate against
    /// [`crate::l1::eip1559::bumped_replacement_fees`] of this record for
    /// **every** pending nonce (at-least-once re-broadcast of the whole
    /// unconfirmed suffix). Overpay is ~×1.1 per suffix tx per rare timeout
    /// round — cheaper than silently dropping a batch or watching a hash the
    /// node no longer holds after failover/restart/eviction.
    ///
    /// Process-local, so the floor is best-effort, not an invariant. A restart
    /// (or a send whose response is lost after the node accepted) re-opens the
    /// underpriced-retry window for a cycle. A rejected "replacement transaction
    /// underpriced" still raises the stored floor so the next tick self-corrects
    /// without waiting for a confirmation timeout.
    in_flight: Arc<Mutex<BTreeMap<u64, Eip1559Fees>>>,
    /// Nonces already reported as entering ceiling hold. Cleared when Latest
    /// advances or the nonce leaves hold, so operators see transitions rather
    /// than one warning per tick.
    ceiling_holds: Arc<Mutex<BTreeSet<u64>>>,
    /// Last insufficient-funds operator alert. Provider failures still return
    /// every time; only the duplicate log is rate-limited.
    last_insufficient_funds_log: Arc<Mutex<Option<Instant>>>,
    /// Test-only: next `send_batch_at_nonce` returns this error string without
    /// broadcasting (so callers can assert map updates / underpriced handling).
    #[cfg(test)]
    fail_next_send: Arc<Mutex<Option<String>>>,
}

impl EthereumBatchPoster {
    pub fn new(provider: DynProvider, config: BatchPosterConfig) -> Self {
        Self {
            provider,
            config,
            in_flight: Arc::new(Mutex::new(BTreeMap::new())),
            ceiling_holds: Arc::new(Mutex::new(BTreeSet::new())),
            last_insufficient_funds_log: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            fail_next_send: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn in_flight_fees_for_test(&self) -> BTreeMap<u64, Eip1559Fees> {
        self.in_flight.lock().expect("in_flight lock").clone()
    }

    #[cfg(test)]
    pub(crate) fn seed_in_flight_fees_for_test(&self, fees: BTreeMap<u64, Eip1559Fees>) {
        *self.in_flight.lock().expect("in_flight lock") = fees;
    }

    #[cfg(test)]
    pub(crate) fn fail_next_send_for_test(&self) {
        *self.fail_next_send.lock().expect("fail_next_send lock") =
            Some("test-injected send failure".to_string());
    }

    #[cfg(test)]
    pub(crate) fn fail_next_send_with_for_test(&self, message: &str) {
        *self.fail_next_send.lock().expect("fail_next_send lock") = Some(message.to_string());
    }

    /// Conservative upper-bound timeout for waiting on confirmations, derived
    /// from the configured block time. Shorter block times on other chains just
    /// make the watch complete sooner.
    fn confirmation_timeout(&self) -> std::time::Duration {
        derive_confirmation_timeout(
            self.config.confirmation_depth,
            self.config.seconds_per_block,
        )
    }

    fn note_ceiling_hold(&self, nonce: u64, fees: Eip1559Fees) {
        let first = self
            .ceiling_holds
            .lock()
            .expect("ceiling_holds lock")
            .insert(nonce);
        if first {
            warn!(
                tx_nonce = nonce,
                max_fee_per_gas = fees.max_fee_per_gas,
                max_priority_fee_per_gas = fees.max_priority_fee_per_gas,
                "batch submission entered fee-ceiling hold; probing again on confirmation cadence"
            );
        }
    }

    fn clear_ceiling_hold(&self, nonce: u64) {
        self.ceiling_holds
            .lock()
            .expect("ceiling_holds lock")
            .remove(&nonce);
    }

    fn log_insufficient_funds(&self) {
        let now = Instant::now();
        let mut last = self
            .last_insufficient_funds_log
            .lock()
            .expect("last_insufficient_funds_log lock");
        if last.is_some_and(|then| now.duration_since(then) < self.confirmation_timeout()) {
            return;
        }
        *last = Some(now);
        error!(
            submitter_address = %self.config.batch_submitter_address,
            "batch submitter has insufficient funds; top up the batch-submitter wallet"
        );
    }

    async fn latest_account_nonce(&self) -> Result<u64, BatchPosterError> {
        self.provider
            .get_transaction_count(self.config.batch_submitter_address)
            .block_id(BlockNumberOrTag::Latest.into())
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))
    }

    async fn send_batch_at_nonce(
        &self,
        payload: Vec<u8>,
        nonce: u64,
        fees: &Eip1559Fees,
    ) -> Result<PendingTransactionBuilder<alloy::network::Ethereum>, BatchPosterError> {
        #[cfg(test)]
        {
            if let Some(message) = self
                .fail_next_send
                .lock()
                .expect("fail_next_send lock")
                .take()
            {
                return Err(BatchPosterError::Provider(message));
            }
        }
        let input_box = InputBox::new(self.config.l1_submit_address, &self.provider);
        let call = input_box
            .addInput(self.config.app_address, payload.into())
            .max_fee_per_gas(fees.max_fee_per_gas)
            .max_priority_fee_per_gas(fees.max_priority_fee_per_gas);

        // Always estimate without an explicit nonce and pin gas (+10% pad)
        // before send. Anvil applies mempool nonce policy to pending
        // `eth_estimateGas` and rejects with "nonce too low" when that nonce
        // is already pending — including the restart shape where we have no
        // in-flight floor yet. (geth typically skips nonce checks in
        // estimateGas via SkipNonceChecks; the Anvil path is what bites
        // locally/CI.) Explicit `CallBuilder::estimate_gas` also pins
        // block=Latest (filler default is pending), either of which avoids
        // Anvil's check. With gas + both fee fields set, the GasFiller is
        // Finished — this estimate replaces the filler's rather than adding
        // a second round-trip.
        let gas = pad_gas_estimate(
            call.estimate_gas()
                .await
                .map_err(|err| BatchPosterError::Provider(err.to_string()))?,
        );

        call.gas(gas)
            .nonce(nonce)
            .send()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))
    }

    /// Wait serially for each tx to reach `confirmation_depth + 1` confirmations.
    ///
    /// Timeouts return `Ok(())` rather than `Err` because the safe response is
    /// "re-enter `submit_batches` on the next tick." That tick re-derives the
    /// unresolved suffix from Latest, so returning after the first timeout is
    /// safe even if a replacement caused a later watched hash to mine.
    async fn wait_for_confirmations(&self, tx_hashes: &[TxHash]) -> Result<(), BatchPosterError> {
        let timeout = self.confirmation_timeout();
        for tx_hash in tx_hashes {
            let watch = PendingTransactionConfig::new(*tx_hash)
                .with_required_confirmations(self.config.confirmation_depth.saturating_add(1))
                .with_timeout(Some(timeout))
                .with_provider(self.provider.root().clone());
            match watch.watch().await {
                Ok(_) => {
                    info!(
                        %tx_hash,
                        confirmation_depth = self.config.confirmation_depth,
                        required_confirmations = self.config.confirmation_depth.saturating_add(1),
                        "batch submission confirmed on L1"
                    );
                }
                Err(PendingTransactionError::TxWatcher(
                    alloy::providers::WatchTxError::Timeout,
                )) => {
                    warn!(
                        %tx_hash,
                        confirmation_depth = self.config.confirmation_depth,
                        timeout_secs = timeout.as_secs(),
                        "timed out waiting for batch submission confirmations; next tick will retry under fresher state"
                    );
                    return Ok(());
                }
                Err(err) => return Err(BatchPosterError::Provider(err.to_string())),
            }
        }

        Ok(())
    }
}

/// geth-family nodes reject same-nonce replacements below the ≥10% bump
/// threshold. Haystack is lowercased so Besu's capitalized wording still
/// matches; Nethermind/Erigon reword the error and are not covered — a miss
/// during bootstrap stalls rather than degrades.
fn is_replacement_underpriced(err: &str) -> bool {
    err.to_ascii_lowercase()
        .contains("replacement transaction underpriced")
}

fn is_already_known(err: &str) -> bool {
    let err = err.to_ascii_lowercase();
    err.contains("already known")
        || err.contains("already imported")
        || err.contains("known transaction")
}

fn is_insufficient_funds(err: &str) -> bool {
    let err = err.to_ascii_lowercase();
    err.contains("insufficient funds") || err.contains("gas required exceeds allowance")
}

fn derive_confirmation_timeout(
    confirmation_depth: u64,
    seconds_per_block: u64,
) -> std::time::Duration {
    let blocks_to_wait = confirmation_depth.saturating_add(1).saturating_mul(2);
    std::time::Duration::from_secs(blocks_to_wait.saturating_mul(seconds_per_block))
}

#[async_trait]
impl BatchPoster for EthereumBatchPoster {
    async fn submit_batches(
        &self,
        payloads: Vec<Vec<u8>>,
        watermark: &dyn WalletNonceWatermarkSink,
    ) -> Result<SubmitBatchesOutcome, BatchPosterError> {
        if payloads.is_empty() {
            return Ok(SubmitBatchesOutcome::Submitted(Vec::new()));
        }

        // Keyed-write chain-id gate (review): re-confirm the RPC still serves the
        // pinned chain immediately before any productive send. The submitter is
        // long-lived — its signing provider is built once at spawn and the
        // boot-time / reader chain-id checks are one-shot — so a load-balanced
        // RPC that fails over to another chain mid-life would otherwise burn
        // submitter nonce slots on the wrong chain. Reached only when there is
        // something to send (the empty early-return above), so idle ticks add no
        // RPC load. A mismatch is terminal (lifted out of the transient bucket by
        // the submitter run-loop); a transient RPC error retries like any blip.
        let rpc_chain_id = self
            .provider
            .get_chain_id()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))?;
        if rpc_chain_id != self.config.expected_chain_id {
            return Err(BatchPosterError::ChainIdMismatch {
                rpc: rpc_chain_id,
                expected: self.config.expected_chain_id,
            });
        }

        let estimate = estimate_fees(&self.provider)
            .await
            .map_err(BatchPosterError::Provider)?;
        let mut next_nonce = self.latest_account_nonce().await?;

        // Drop fee floors for nonces Latest has advanced past — those slots
        // are resolved and must not floor a later send.
        {
            let mut in_flight = self.in_flight.lock().expect("in_flight lock");
            in_flight.retain(|&nonce, _| nonce >= next_nonce);
        }
        self.ceiling_holds
            .lock()
            .expect("ceiling_holds lock")
            .retain(|&nonce| nonce >= next_nonce);

        // Write-before-broadcast (R1a): durably cover every nonce this
        // tick will use before the first send. One raise to the highest
        // covers the whole consecutive range.
        let highest_nonce = next_nonce.saturating_add(payloads.len() as u64 - 1);
        watermark
            .raise_to(highest_nonce)
            .map_err(BatchPosterError::Provider)?;

        let mut tx_hashes = Vec::with_capacity(payloads.len());
        let mut any_held = false;

        for payload in payloads {
            let prior = {
                let in_flight = self.in_flight.lock().expect("in_flight lock");
                in_flight.get(&next_nonce).copied()
            };
            let FeesForNonce { fees, hold } = fees_for_nonce(estimate, prior);
            let pending = match self.send_batch_at_nonce(payload, next_nonce, &fees).await {
                Ok(pending) => {
                    if hold {
                        any_held = true;
                        self.note_ceiling_hold(next_nonce, fees);
                    } else {
                        self.clear_ceiling_hold(next_nonce);
                        self.in_flight
                            .lock()
                            .expect("in_flight lock")
                            .insert(next_nonce, fees);
                    }
                    pending
                }
                Err(BatchPosterError::Provider(ref msg)) if is_already_known(msg) => {
                    self.clear_ceiling_hold(next_nonce);
                    self.in_flight
                        .lock()
                        .expect("in_flight lock")
                        .insert(next_nonce, fees);
                    next_nonce = next_nonce.saturating_add(1);
                    continue;
                }
                Err(BatchPosterError::Provider(ref msg)) if is_replacement_underpriced(msg) => {
                    if hold {
                        any_held = true;
                        self.note_ceiling_hold(next_nonce, fees);
                        next_nonce = next_nonce.saturating_add(1);
                        continue;
                    }
                    // Node rejected the replacement fee — raise the floor from
                    // what we just tried so the next tick clears the threshold
                    // without waiting for a confirmation timeout.
                    let raised = fees_for_nonce(estimate, Some(fees));
                    self.in_flight
                        .lock()
                        .expect("in_flight lock")
                        .insert(next_nonce, raised.fees);
                    return Err(BatchPosterError::Provider(msg.clone()));
                }
                Err(BatchPosterError::Provider(ref msg)) if is_insufficient_funds(msg) => {
                    self.log_insufficient_funds();
                    return Err(BatchPosterError::Provider(msg.clone()));
                }
                Err(err) => return Err(err),
            };
            // Record only after a successful broadcast — a failed send must
            // not raise the replacement floor for the next tick (except the
            // underpriced path above, which self-corrects against a live pending
            // tx the node already holds).
            let tx_hash = *pending.tx_hash();
            debug!(
                tx_nonce = next_nonce,
                %tx_hash,
                max_fee_per_gas = fees.max_fee_per_gas,
                max_priority_fee_per_gas = fees.max_priority_fee_per_gas,
                confirmation_depth = self.config.confirmation_depth,
                "sent batch submission tx to L1"
            );
            tx_hashes.push(tx_hash);
            next_nonce = next_nonce.saturating_add(1);
        }

        self.wait_for_confirmations(tx_hashes.as_slice()).await?;
        if any_held {
            Ok(SubmitBatchesOutcome::Held(tx_hashes))
        } else {
            Ok(SubmitBatchesOutcome::Submitted(tx_hashes))
        }
    }

    async fn observed_submitted_batch_nonces(
        &self,
        from_block: u64,
    ) -> Result<Vec<u64>, BatchPosterError> {
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))?;
        let start_block = from_block.max(self.config.start_block);
        if start_block > latest {
            return Ok(Vec::new());
        }

        // Ordered fetch: `advance_expected_batch_nonce` folds these nonces
        // assuming L1 event order, so a raw `eth_getLogs` reorder would
        // under-advance the frontier and resubmit an already-mined suffix
        // (wasted gas + InputBox noise). The `_ordered` helper guarantees the
        // canonical (block, tx_index, log_index) order — the same the reader
        // relies on for its contiguity check.
        let events = get_input_added_events_ordered(
            &self.provider,
            self.config.app_address,
            &self.config.l1_submit_address,
            start_block,
            latest,
            self.config.long_block_range_error_codes.as_slice(),
        )
        .await
        .map_err(|err| BatchPosterError::Provider(format!("get_input_added_events: {err}")))?;

        let mut observed_nonces = Vec::new();
        for (event, _log) in events {
            let evm_advance = decode_evm_advance_input(event.input.as_ref()).map_err(|err| {
                BatchPosterError::Provider(format!(
                    "decode EvmAdvance for InputAdded index {}: {err}",
                    event.index
                ))
            })?;
            if evm_advance.msgSender != self.config.batch_submitter_address {
                continue;
            }
            let batch: Batch = ssz::Decode::from_ssz_bytes(evm_advance.payload.as_ref())
                .map_err(|err| BatchPosterError::Provider(format!("{err:?}")))?;
            observed_nonces.push(batch.nonce);
        }

        Ok(observed_nonces)
    }
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{Batch, BatchPoster, BatchPosterError, SubmitBatchesOutcome, TxHash};
    use crate::l1::watermark::WalletNonceWatermarkSink;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Debug)]
    pub struct MockBatchPoster {
        pub submissions: Mutex<Vec<(u64, usize)>>,
        pub observed_submitted_nonces: Mutex<Vec<u64>>,
        pub observed_submitted_error: Mutex<Option<String>>,
        pub last_from_block: Mutex<Option<u64>>,
        pub held: Mutex<bool>,
    }

    impl MockBatchPoster {
        pub fn new() -> Self {
            Self {
                submissions: Mutex::new(Vec::new()),
                observed_submitted_nonces: Mutex::new(Vec::new()),
                observed_submitted_error: Mutex::new(None),
                last_from_block: Mutex::new(None),
                held: Mutex::new(false),
            }
        }

        pub fn submissions(&self) -> Vec<(u64, usize)> {
            self.submissions.lock().expect("lock").clone()
        }

        pub fn set_observed_submitted_nonces(&self, value: Vec<u64>) {
            *self.observed_submitted_nonces.lock().expect("lock") = value;
        }

        pub fn set_observed_submitted_error(&self, value: Option<&str>) {
            *self.observed_submitted_error.lock().expect("lock") = value.map(str::to_string);
        }

        pub fn last_from_block(&self) -> Option<u64> {
            *self.last_from_block.lock().expect("lock")
        }

        pub fn set_held(&self, held: bool) {
            *self.held.lock().expect("lock") = held;
        }
    }

    #[async_trait]
    impl BatchPoster for MockBatchPoster {
        async fn submit_batches(
            &self,
            payloads: Vec<Vec<u8>>,
            _watermark: &dyn WalletNonceWatermarkSink,
        ) -> Result<SubmitBatchesOutcome, BatchPosterError> {
            let mut tx_hashes = Vec::with_capacity(payloads.len());
            for payload in payloads {
                let batch_index = ssz::Decode::from_ssz_bytes(payload.as_ref())
                    .map(|b: Batch| b.nonce)
                    .unwrap_or(0);
                self.submissions
                    .lock()
                    .expect("lock")
                    .push((batch_index, payload.len()));
                tx_hashes.push(TxHash::ZERO);
            }
            if *self.held.lock().expect("lock") {
                Ok(SubmitBatchesOutcome::Held(tx_hashes))
            } else {
                Ok(SubmitBatchesOutcome::Submitted(tx_hashes))
            }
        }

        async fn observed_submitted_batch_nonces(
            &self,
            from_block: u64,
        ) -> Result<Vec<u64>, BatchPosterError> {
            *self.last_from_block.lock().expect("lock") = Some(from_block);
            if let Some(err) = self.observed_submitted_error.lock().expect("lock").clone() {
                return Err(BatchPosterError::Provider(err));
            }
            let configured = self.observed_submitted_nonces.lock().expect("lock").clone();
            if !configured.is_empty() {
                return Ok(configured);
            }
            Ok(self
                .submissions
                .lock()
                .expect("lock")
                .iter()
                .map(|(idx, _)| *idx)
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::{
        BatchPoster, BatchPosterConfig, BatchPosterError, EthereumBatchPoster,
        SubmitBatchesOutcome, derive_confirmation_timeout, is_already_known, is_insufficient_funds,
        is_replacement_underpriced, mock::MockBatchPoster,
    };
    use crate::l1::eip1559::{fee_ceiling, pad_gas_estimate};
    use crate::l1::watermark::WalletNonceWatermarkSink;
    use alloy::node_bindings::Anvil;
    use alloy::providers::Provider;
    use alloy::rpc::types::BlockNumberOrTag;

    fn submitted_hashes(outcome: SubmitBatchesOutcome) -> Vec<super::TxHash> {
        match outcome {
            SubmitBatchesOutcome::Submitted(hashes) => hashes,
            SubmitBatchesOutcome::Held(hashes) => {
                panic!("expected Submitted outcome, got Held({hashes:?})")
            }
        }
    }

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

    /// A watermark sink that records every `raise_to` call and (optionally)
    /// fails, so a test can observe whether the raise happened — and in what
    /// order relative to the first send.
    struct RecordingWatermarkSink {
        calls: Mutex<Vec<u64>>,
        fail: bool,
    }

    impl RecordingWatermarkSink {
        fn failing() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: true,
            }
        }

        fn passing() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: false,
            }
        }

        fn calls(&self) -> Vec<u64> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl WalletNonceWatermarkSink for RecordingWatermarkSink {
        fn raise_to(&self, highest: u64) -> Result<(), String> {
            self.calls.lock().expect("lock").push(highest);
            if self.fail {
                Err("recording sink: forced failure".to_string())
            } else {
                Ok(())
            }
        }
    }

    /// R1a write-before-broadcast: `submit_batches` must raise the watermark to
    /// cover the whole consecutive nonce range *before* the first send. We lock
    /// it with a sink that fails on `raise_to`: a correct poster aborts the tick
    /// before broadcasting anything, so the submitter's pending nonce is
    /// unchanged. If `raise_to` were moved after the first `addInput` send
    /// (re-opening the F1 zombie-tx hole), that send would bump the pending
    /// nonce and this test would go red. Also pins the raise count (once) and
    /// value (`base + payloads.len() - 1`). (Mutation-checked: moving the raise
    /// after the send loop fails this test.)
    #[tokio::test]
    async fn submit_batches_raises_watermark_before_any_send() {
        require_anvil();
        let anvil = Anvil::default().spawn();
        // Anvil account 0 — the submitter; its key signs the (never-sent) txs.
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");

        let config = BatchPosterConfig {
            l1_submit_address: alloy_primitives::Address::repeat_byte(0x11),
            app_address: alloy_primitives::Address::repeat_byte(0x22),
            batch_submitter_address: submitter,
            start_block: 0,
            confirmation_depth: 0,
            seconds_per_block: 1,
            long_block_range_error_codes: vec![],
            expected_chain_id: anvil.chain_id(),
        };
        let poster = EthereumBatchPoster::new(provider.clone(), config);

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        let sink = RecordingWatermarkSink::failing();
        let payloads = vec![vec![0u8; 4], vec![1u8; 4], vec![2u8; 4]]; // 3 consecutive nonces

        let result = poster.submit_batches(payloads, &sink).await;

        assert!(
            matches!(result, Err(BatchPosterError::Provider(_))),
            "a failing watermark sink must abort submit_batches, got {result:?}"
        );
        // (a) raised exactly once, (b) to the highest nonce of the range.
        assert_eq!(
            sink.calls(),
            vec![base_nonce + 2],
            "raise_to must be called once with base_nonce + payloads.len() - 1"
        );
        // (c) before any send — no tx broadcast, so pending nonce is unchanged.
        let pending = provider
            .get_transaction_count(submitter)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        assert_eq!(
            pending, base_nonce,
            "raise_to must run before any send; a broadcast would have bumped the pending nonce"
        );
    }

    /// Keyed-write chain-id gate: a long-lived submitter pointed at an RPC that
    /// serves a different chain than the pinned one must refuse to submit, before
    /// any productive work — no watermark raise, no broadcast. Anvil's chain id
    /// is 31337; we pin a different one and assert the `ChainIdMismatch` refusal
    /// fires ahead of the (would-otherwise-fail-later) watermark raise.
    #[tokio::test]
    async fn submit_batches_refuses_on_wrong_chain_before_any_work() {
        require_anvil();
        let anvil = Anvil::default().spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");

        let wrong_chain_id = anvil.chain_id() + 1;
        let config = BatchPosterConfig {
            l1_submit_address: alloy_primitives::Address::repeat_byte(0x11),
            app_address: alloy_primitives::Address::repeat_byte(0x22),
            batch_submitter_address: submitter,
            start_block: 0,
            confirmation_depth: 0,
            seconds_per_block: 1,
            long_block_range_error_codes: vec![],
            expected_chain_id: wrong_chain_id,
        };
        let poster = EthereumBatchPoster::new(provider.clone(), config);

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        // A sink that would *succeed* — so the only thing that can stop a send is
        // the chain-id gate, not the watermark guard. (Recording proves the gate
        // fires first: a passing chain check would reach `raise_to`.)
        let sink = RecordingWatermarkSink::passing();
        let payloads = vec![vec![0u8; 4], vec![1u8; 4]];

        let result = poster.submit_batches(payloads, &sink).await;

        assert!(
            matches!(
                result,
                Err(BatchPosterError::ChainIdMismatch { rpc, expected })
                    if rpc == anvil.chain_id() && expected == wrong_chain_id
            ),
            "wrong-chain RPC must abort submit_batches with ChainIdMismatch, got {result:?}"
        );
        assert!(
            sink.calls().is_empty(),
            "chain-id gate must fire before the watermark raise (no raise_to call)"
        );
        let pending = provider
            .get_transaction_count(submitter)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        assert_eq!(
            pending, base_nonce,
            "no tx may be broadcast on the wrong chain"
        );
    }

    #[tokio::test]
    async fn mock_poster_tracks_requested_suffix_start_block() {
        let poster = MockBatchPoster::new();
        let observed = poster
            .observed_submitted_batch_nonces(42)
            .await
            .expect("observe submitted batches");

        assert!(observed.is_empty());
        assert_eq!(poster.last_from_block(), Some(42));
    }

    #[test]
    fn confirmation_timeout_derives_from_seconds_per_block() {
        assert_eq!(derive_confirmation_timeout(2, 12), Duration::from_secs(72));
        assert_eq!(derive_confirmation_timeout(2, 1), Duration::from_secs(6));
        assert_eq!(derive_confirmation_timeout(5, 3), Duration::from_secs(36));
    }

    #[test]
    fn pad_gas_estimate_adds_ten_percent() {
        assert_eq!(pad_gas_estimate(100_000), 110_000);
        assert_eq!(pad_gas_estimate(0), 0);
        assert_eq!(pad_gas_estimate(u64::MAX), u64::MAX);
    }

    #[test]
    fn is_replacement_underpriced_matches_geth_family_case_insensitively() {
        assert!(is_replacement_underpriced(
            "server returned an error response: error code -32000: replacement transaction underpriced"
        ));
        assert!(is_replacement_underpriced(
            "Replacement transaction underpriced" // Besu capitalizes
        ));
        assert!(!is_replacement_underpriced("nonce too low"));
        assert!(!is_replacement_underpriced(
            "max priority fee per gas higher than max fee per gas"
        ));
    }

    #[test]
    fn already_known_matches_common_client_wording_case_insensitively() {
        assert!(is_already_known("already known"));
        assert!(is_already_known("Already Imported"));
        assert!(is_already_known("Known transaction"));
        assert!(!is_already_known("nonce too low"));
    }

    #[test]
    fn insufficient_funds_matches_common_client_wording_case_insensitively() {
        assert!(is_insufficient_funds(
            "insufficient funds for gas * price + value"
        ));
        assert!(is_insufficient_funds("Gas required exceeds allowance"));
        assert!(!is_insufficient_funds(
            "replacement transaction underpriced"
        ));
    }

    #[test]
    fn underpriced_send_raises_floor_from_attempted_fees() {
        let estimate = crate::l1::eip1559::Eip1559Fees {
            base_fee_per_gas: 20_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 41_000_000_000,
        };
        let attempted = crate::l1::eip1559::fees_for_nonce(estimate, None).fees;
        let raised = crate::l1::eip1559::fees_for_nonce(estimate, Some(attempted));
        assert!(raised.fees.max_fee_per_gas > attempted.max_fee_per_gas);
        assert!(raised.fees.max_priority_fee_per_gas > attempted.max_priority_fee_per_gas);
        assert!(raised.fees.max_priority_fee_per_gas <= raised.fees.max_fee_per_gas);
    }

    fn poster_config(anvil: &alloy::node_bindings::AnvilInstance) -> BatchPosterConfig {
        BatchPosterConfig {
            l1_submit_address: alloy_primitives::Address::repeat_byte(0x11),
            app_address: alloy_primitives::Address::repeat_byte(0x22),
            batch_submitter_address: alloy_primitives::address!(
                "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            ),
            start_block: 0,
            // confirmation_depth 0 → watch timeout is 2 * seconds_per_block;
            // keep it short so --no-mining ticks return promptly on timeout.
            confirmation_depth: 0,
            seconds_per_block: 1,
            long_block_range_error_codes: vec![],
            expected_chain_id: anvil.chain_id(),
        }
    }

    /// Same-nonce retry floors a flat re-estimate against the in-flight record
    /// (≥10% on both fields). Seeds the prior floor explicitly so the assertion
    /// does not depend on Anvil keeping a tx pending across ticks.
    #[tokio::test]
    async fn submit_batches_replacement_clears_ten_percent_bump() {
        require_anvil();
        let anvil = Anvil::default().timeout(30_000).spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        // Prior fees high enough that a fresh Anvil estimate will not clear the
        // ≥10% floor on its own, but below this estimate's ceiling so this is a
        // normal replacement rather than a ceiling hold.
        let estimate = crate::l1::eip1559::estimate_fees(&provider)
            .await
            .expect("fee estimate");
        let prior = crate::l1::eip1559::Eip1559Fees {
            base_fee_per_gas: estimate.base_fee_per_gas,
            max_priority_fee_per_gas: estimate
                .max_priority_fee_per_gas
                .saturating_mul(2)
                .min(estimate.max_fee_per_gas.saturating_mul(2)),
            max_fee_per_gas: estimate.max_fee_per_gas.saturating_mul(2),
        };
        poster.seed_in_flight_fees_for_test(BTreeMap::from([(base_nonce, prior)]));

        poster
            .submit_batches(vec![vec![0u8; 4]], &sink)
            .await
            .expect("submit with in-flight floor");
        let sent = poster
            .in_flight_fees_for_test()
            .get(&base_nonce)
            .copied()
            .expect("successful send must record fees");

        let (bumped_max, bumped_prio) = crate::l1::eip1559::bumped_replacement_fees(
            prior.max_fee_per_gas,
            prior.max_priority_fee_per_gas,
        );
        assert!(
            sent.max_fee_per_gas >= bumped_max,
            "max_fee must clear replacement floor: sent={} floor={bumped_max}",
            sent.max_fee_per_gas
        );
        assert!(
            sent.max_priority_fee_per_gas >= bumped_prio,
            "priority must clear replacement floor: sent={} floor={bumped_prio}",
            sent.max_priority_fee_per_gas
        );
    }

    /// Review E2E (jplgarcia): Anvil `--no-mining` → original poster tx →
    /// confirmation timeout → real same-nonce replacement → resume mining →
    /// assert receipt / nonce progression.
    ///
    /// Pins the gas-estimation hole that blocked the replacement path: with the
    /// original still pending, Anvil rejects `eth_estimateGas(..., nonce=N,
    /// block=pending)` as "nonce too low", so the filler never reaches
    /// `eth_sendRawTransaction`. The poster always estimates without that nonce
    /// (Latest) and pins padded gas on every send.
    #[tokio::test]
    async fn submit_batches_replaces_pending_tx_after_confirmation_timeout() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");

        // 1) Original poster tx parks in the mempool (mining disabled).
        let first_hashes = submitted_hashes(
            poster
                .submit_batches(vec![vec![0u8; 4]], &sink)
                .await
                .expect("first submit parks a pending tx"),
        );
        assert_eq!(first_hashes.len(), 1);
        let first_hash = first_hashes[0];

        let pending_after_first = provider
            .get_transaction_count(submitter)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        let latest_after_first = provider
            .get_transaction_count(submitter)
            .block_id(BlockNumberOrTag::Latest.into())
            .await
            .expect("latest nonce");
        assert_eq!(
            pending_after_first,
            base_nonce + 1,
            "first send must occupy the mempool slot"
        );
        assert_eq!(
            latest_after_first, base_nonce,
            "mining is disabled; latest must not advance"
        );

        let prior_fees = poster
            .in_flight_fees_for_test()
            .get(&base_nonce)
            .copied()
            .expect("first send records in-flight fees");

        // 2) Confirmation watch timed out; next tick must bump fees and
        //    broadcast a same-nonce replacement (the gas-estimate fix).
        let second_hashes = submitted_hashes(
            poster
                .submit_batches(vec![vec![0u8; 4]], &sink)
                .await
                .expect("replacement must clear gas estimation and broadcast"),
        );
        assert_eq!(second_hashes.len(), 1);
        let replacement_hash = second_hashes[0];
        assert_ne!(
            replacement_hash, first_hash,
            "replacement must be a distinct tx hash"
        );

        let sent = poster
            .in_flight_fees_for_test()
            .get(&base_nonce)
            .copied()
            .expect("replacement records bumped fees");
        let (bumped_max, bumped_prio) = crate::l1::eip1559::bumped_replacement_fees(
            prior_fees.max_fee_per_gas,
            prior_fees.max_priority_fee_per_gas,
        );
        assert!(
            sent.max_fee_per_gas >= bumped_max,
            "replacement max_fee must clear floor: sent={} floor={bumped_max}",
            sent.max_fee_per_gas
        );
        assert!(
            sent.max_priority_fee_per_gas >= bumped_prio,
            "replacement priority must clear floor: sent={} floor={bumped_prio}",
            sent.max_priority_fee_per_gas
        );

        // Still one pending slot until mining resumes.
        let pending_after_replace = provider
            .get_transaction_count(submitter)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending after replace");
        assert_eq!(
            pending_after_replace,
            base_nonce + 1,
            "replacement keeps a single pending nonce slot"
        );

        // 3) Resume mining and assert receipt / nonce progression.
        let _: serde_json::Value = provider
            .raw_request("evm_mine".into(), ())
            .await
            .expect("mine replacement");

        let latest_after_mine = provider
            .get_transaction_count(submitter)
            .block_id(BlockNumberOrTag::Latest.into())
            .await
            .expect("latest after mine");
        assert_eq!(
            latest_after_mine,
            base_nonce + 1,
            "mined replacement must advance the account nonce"
        );

        let receipt = provider
            .get_transaction_receipt(replacement_hash)
            .await
            .expect("receipt rpc")
            .expect("replacement must have a receipt after mining");
        assert_eq!(
            receipt.transaction_hash, replacement_hash,
            "mined receipt must belong to the replacement tx"
        );
        assert!(
            provider
                .get_transaction_receipt(first_hash)
                .await
                .expect("original receipt rpc")
                .is_none(),
            "original pending tx must be evicted by the replacement"
        );

        // On-wire fees must clear the ≥10% floor (not just the in-memory record).
        use alloy::consensus::Transaction as _;
        let mined = provider
            .get_transaction_by_hash(replacement_hash)
            .await
            .expect("get replacement tx")
            .expect("replacement tx must be fetchable after mining");
        assert!(
            mined.max_fee_per_gas() >= bumped_max,
            "mined max_fee must clear floor: on_wire={} floor={bumped_max}",
            mined.max_fee_per_gas()
        );
        let on_wire_prio = mined
            .max_priority_fee_per_gas()
            .expect("replacement must be EIP-1559");
        assert!(
            on_wire_prio >= bumped_prio,
            "mined priority must clear floor: on_wire={on_wire_prio} floor={bumped_prio}"
        );
    }

    /// When Latest advances past a nonce, that nonce's fee floor is dropped so a
    /// later tip send is not incorrectly floored by stale in-flight state.
    #[tokio::test]
    async fn submit_batches_prunes_in_flight_fees_past_latest() {
        require_anvil();
        let anvil = Anvil::default().timeout(30_000).spawn(); // automine on
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");

        poster
            .submit_batches(vec![vec![0u8; 4]], &sink)
            .await
            .expect("first submit mines under automine");
        assert!(
            poster.in_flight_fees_for_test().contains_key(&base_nonce),
            "first send records fees for the mined nonce"
        );

        // Tip confirmed → Latest = base_nonce + 1. Re-seed a stale floor on the
        // mined nonce (as if a previous tick left it) and confirm the next
        // submit prunes it.
        let stale = crate::l1::eip1559::Eip1559Fees {
            base_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 1,
        };
        poster.seed_in_flight_fees_for_test(BTreeMap::from([(base_nonce, stale)]));

        poster
            .submit_batches(vec![vec![1u8; 4]], &sink)
            .await
            .expect("second submit");

        let in_flight = poster.in_flight_fees_for_test();
        assert!(
            !in_flight.contains_key(&base_nonce),
            "mined nonce must be pruned once Latest advances: {in_flight:?}"
        );
        let tip_nonce = base_nonce.saturating_add(1);
        assert!(
            in_flight.contains_key(&tip_nonce),
            "current tip send must be recorded: {in_flight:?}"
        );
    }

    /// A failed broadcast must not raise the replacement floor — otherwise a
    /// blip would permanently overprice the next successful send, or worse,
    /// record fees for a tx that never entered the mempool.
    #[tokio::test]
    async fn submit_batches_does_not_record_fees_when_send_fails() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        let prior = crate::l1::eip1559::Eip1559Fees {
            base_fee_per_gas: 42,
            max_priority_fee_per_gas: 7,
            max_fee_per_gas: 1_000,
        };
        poster.seed_in_flight_fees_for_test(BTreeMap::from([(base_nonce, prior)]));
        poster.fail_next_send_for_test();

        let result = poster.submit_batches(vec![vec![0u8; 4]], &sink).await;
        assert!(
            matches!(result, Err(BatchPosterError::Provider(ref msg)) if msg.contains("test-injected")),
            "injected send failure must surface, got {result:?}"
        );
        assert_eq!(
            poster.in_flight_fees_for_test(),
            BTreeMap::from([(base_nonce, prior)]),
            "failed send must leave the prior in-flight floor untouched"
        );
    }

    /// Underpriced rejection raises the floor from the fees we attempted, so the
    /// next tick clears geth's ≥10% threshold without waiting for timeout.
    #[tokio::test]
    async fn submit_batches_underpriced_raises_floor_from_attempted_fees() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        let prior = crate::l1::eip1559::Eip1559Fees {
            base_fee_per_gas: 1,
            max_priority_fee_per_gas: 50_000_000,
            max_fee_per_gas: 1_000_000_000,
        };
        poster.seed_in_flight_fees_for_test(BTreeMap::from([(base_nonce, prior)]));
        poster.fail_next_send_with_for_test(
            "server returned an error response: error code -32000: Replacement transaction underpriced",
        );

        let result = poster.submit_batches(vec![vec![0u8; 4]], &sink).await;
        assert!(
            matches!(
                result,
                Err(BatchPosterError::Provider(ref msg))
                    if is_replacement_underpriced(msg)
            ),
            "underpriced injection must surface, got {result:?}"
        );

        let raised = poster
            .in_flight_fees_for_test()
            .get(&base_nonce)
            .copied()
            .expect("underpriced path must raise the floor");
        // Attempted fees clear prior; raised clears attempted.
        let (floor_max, floor_prio) = crate::l1::eip1559::bumped_replacement_fees(
            prior.max_fee_per_gas,
            prior.max_priority_fee_per_gas,
        );
        assert!(raised.max_fee_per_gas > floor_max);
        assert!(raised.max_priority_fee_per_gas > floor_prio);
        assert!(raised.max_priority_fee_per_gas <= raised.max_fee_per_gas);
    }

    #[tokio::test]
    async fn submit_batches_underpriced_at_ceiling_holds_without_raising_floor() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        let estimate = crate::l1::eip1559::estimate_fees(&provider)
            .await
            .expect("fee estimate");
        let seed = crate::l1::eip1559::Eip1559Fees {
            max_fee_per_gas: fee_ceiling(estimate.max_fee_per_gas),
            ..estimate
        };
        let seeded = BTreeMap::from([(base_nonce, seed)]);
        poster.seed_in_flight_fees_for_test(seeded.clone());
        poster.fail_next_send_with_for_test("Replacement transaction underpriced");

        let outcome = poster
            .submit_batches(vec![vec![0u8; 4]], &sink)
            .await
            .expect("ceiling hold is a non-error tick outcome");
        assert_eq!(outcome, SubmitBatchesOutcome::Held(Vec::new()));
        assert_eq!(
            poster.in_flight_fees_for_test(),
            seeded,
            "ceiling hold must leave the stored floor byte-identical"
        );
    }

    /// Restart shape: empty in-flight map vs a live pending tx. Unconditional
    /// nonce-free gas estimate must still reach `eth_sendRawTransaction`.
    #[tokio::test]
    async fn submit_batches_replaces_pending_without_in_flight_floor() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let first_hashes = submitted_hashes(
            poster
                .submit_batches(vec![vec![0u8; 4]], &sink)
                .await
                .expect("first submit parks a pending tx"),
        );
        assert_eq!(first_hashes.len(), 1);

        // Simulate restart / lost response: forget the floor while the tx is
        // still pending on the node.
        poster.seed_in_flight_fees_for_test(BTreeMap::new());

        let outcome = poster
            .submit_batches(vec![vec![0u8; 4]], &sink)
            .await
            .expect("already-known/imported is successful at-least-once progress");
        let SubmitBatchesOutcome::Submitted(second_hashes) = outcome else {
            panic!("untracked first-fee retry cannot enter ceiling hold");
        };
        assert!(
            second_hashes.len() <= 1,
            "already-known sends may omit the hash; fresh replacements return one"
        );
    }

    /// Multi-nonce suffix: every pending nonce is re-broadcast and floored on
    /// retry (no skip-and-watch). Three payloads → three replacements.
    #[tokio::test]
    async fn submit_batches_rebroadcasts_and_floors_entire_pending_suffix() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        let payloads = vec![vec![0u8; 4], vec![1u8; 4], vec![2u8; 4]];

        let first_hashes = submitted_hashes(
            poster
                .submit_batches(payloads.clone(), &sink)
                .await
                .expect("first multi-nonce submit"),
        );
        assert_eq!(first_hashes.len(), 3);
        let prior_fees = poster.in_flight_fees_for_test();
        assert_eq!(prior_fees.len(), 3);

        let second_hashes = submitted_hashes(
            poster
                .submit_batches(payloads, &sink)
                .await
                .expect("suffix rebroadcast"),
        );
        assert_eq!(second_hashes.len(), 3);
        for (first, second) in first_hashes.iter().zip(second_hashes.iter()) {
            assert_ne!(
                first, second,
                "each nonce must be replaced, not skip-watched"
            );
        }

        let sent = poster.in_flight_fees_for_test();
        for offset in 0..3u64 {
            let nonce = base_nonce + offset;
            let prior = prior_fees.get(&nonce).copied().expect("prior fees");
            let next = sent.get(&nonce).copied().expect("replacement fees");
            let (bumped_max, bumped_prio) = crate::l1::eip1559::bumped_replacement_fees(
                prior.max_fee_per_gas,
                prior.max_priority_fee_per_gas,
            );
            assert!(next.max_fee_per_gas >= bumped_max);
            assert!(next.max_priority_fee_per_gas >= bumped_prio);
        }

        let _: serde_json::Value = provider
            .raw_request("evm_mine".into(), ())
            .await
            .expect("mine");
        // One block includes the contiguous suffix in nonce order.
        let latest = provider
            .get_transaction_count(submitter)
            .block_id(BlockNumberOrTag::Latest.into())
            .await
            .expect("latest");
        assert_eq!(latest, base_nonce + 3);
    }
}
