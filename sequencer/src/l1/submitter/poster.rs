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
use std::future::Future;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::l1::eip1559::{Eip1559Fees, estimate_fees};
use crate::l1::partition::{decode_evm_advance_input, get_input_added_events_ordered};
use crate::l1::watermark::{WalletNonceWatermarkError, WalletNonceWatermarkSink};
use crate::runtime::shutdown::RuntimeScope;

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
    #[error(
        "wallet nonce range starting at {first_nonce} for {batch_count} batches \
         cannot be represented durably"
    )]
    WalletNonceRangeUnrepresentable {
        first_nonce: u64,
        batch_count: usize,
    },
    #[error("runtime stopped L1 submission after a persistent storage invariant failure")]
    StorageInvariantViolation,
    #[error("runtime shutdown cancelled L1 submission")]
    Shutdown,
    #[error(transparent)]
    Watermark(#[from] WalletNonceWatermarkError),
}

impl BatchPosterError {
    pub(crate) fn is_terminal_invariant(&self) -> bool {
        // Exhaustive on purpose: a new variant must decide its terminality
        // here, not silently default to restartable.
        match self {
            Self::ChainIdMismatch { .. }
            | Self::WalletNonceRangeUnrepresentable { .. }
            | Self::StorageInvariantViolation => true,
            Self::Watermark(source) => source.is_persistent_invariant(),
            Self::Provider(_) | Self::Shutdown => false,
        }
    }
}

#[async_trait]
pub(crate) trait BatchPoster: Send + Sync {
    /// Broadcast the payloads as L1 txs at consecutive wallet nonces.
    /// Implementations must raise `watermark` to the highest nonce they
    /// are about to use *before* the first send (write-before-broadcast,
    /// review R1a).
    /// Requires the externalization token: the caller consulted containment
    /// this tick. Implementations may re-check at finer grain (the Ethereum
    /// poster gates each send); a mock ignoring `_auth` is correct — the
    /// token is the caller's proof, not the implementation's (S-A).
    async fn submit_batches(
        &self,
        auth: crate::runtime::shutdown::Authorized<'_>,
        payloads: Vec<Vec<u8>>,
        watermark: &dyn WalletNonceWatermarkSink,
    ) -> Result<Vec<TxHash>, BatchPosterError>;

    async fn observed_submitted_batch_nonces(
        &self,
        from_block: u64,
    ) -> Result<Vec<u64>, BatchPosterError>;
}

#[derive(Clone)]
pub struct EthereumBatchPoster {
    provider: DynProvider,
    config: BatchPosterConfig,
    /// Externalization gate for keyed L1 sends. A construction-time field,
    /// not a trait parameter: the gate is this implementation's posture, and
    /// mocks were ignoring the parameter anyway (H11).
    shutdown: RuntimeScope,
}

impl EthereumBatchPoster {
    pub fn new(provider: DynProvider, config: BatchPosterConfig, shutdown: RuntimeScope) -> Self {
        Self {
            provider,
            config,
            shutdown,
        }
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
        let input_box = InputBox::new(self.config.l1_submit_address, &self.provider);
        input_box
            .addInput(self.config.app_address, payload.into())
            .nonce(nonce)
            .max_fee_per_gas(fees.max_fee_per_gas)
            .max_priority_fee_per_gas(fees.max_priority_fee_per_gas)
            .send()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))
    }

    /// Wait serially for each tx to reach `confirmation_depth + 1` confirmations.
    ///
    /// **Serial is not a performance concession; it's correct.** Ethereum mines
    /// transactions from a single EOA in strict wallet-nonce order: tx[k] cannot
    /// land on-chain until tx[k-1] has landed. So:
    ///
    /// - If tx[0] times out, tx[1..] cannot have been mined either; watching
    ///   them is provably pointless. We return `Ok(())` early and let the next
    ///   tick retry the whole sequence.
    /// - If tx[0] confirms, tx[1] was blocked only on tx[0] and is unblocked by
    ///   the time we start watching it.
    ///
    /// Timeouts return `Ok(())` rather than `Err` because the safe response is
    /// "re-enter `submit_batches` on the next tick" — which re-estimates fees
    /// (possibly replacing a pending transaction if the node accepts it) and
    /// re-submits at the same wallet nonces. The
    /// wallet-nonce ordering invariant above guarantees we cannot accidentally
    /// skip work by returning early here.
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

fn derive_confirmation_timeout(
    confirmation_depth: u64,
    seconds_per_block: u64,
) -> std::time::Duration {
    let blocks_to_wait = confirmation_depth.saturating_add(1).saturating_mul(2);
    std::time::Duration::from_secs(blocks_to_wait.saturating_mul(seconds_per_block))
}

fn checked_highest_wallet_nonce(
    first_nonce: u64,
    batch_count: usize,
) -> Result<u64, BatchPosterError> {
    let invalid = || BatchPosterError::WalletNonceRangeUnrepresentable {
        first_nonce,
        batch_count,
    };
    let count = u64::try_from(batch_count).map_err(|_| invalid())?;
    let last_offset = count.checked_sub(1).ok_or_else(invalid)?;
    let highest = first_nonce.checked_add(last_offset).ok_or_else(invalid)?;
    i64::try_from(highest).map_err(|_| invalid())?;
    Ok(highest)
}

async fn externalize_provider_call<T>(
    shutdown: &RuntimeScope,
    call: impl Future<Output = Result<T, BatchPosterError>>,
) -> Result<T, BatchPosterError> {
    if shutdown.is_storage_invariant_contained() {
        return Err(BatchPosterError::StorageInvariantViolation);
    }
    tokio::select! {
        biased;
        _ = shutdown.wait_for_shutdown() => {
            if shutdown.is_storage_invariant_contained() {
                Err(BatchPosterError::StorageInvariantViolation)
            } else {
                Err(BatchPosterError::Shutdown)
            }
        }
        result = call => result,
    }
}

#[async_trait]
impl BatchPoster for EthereumBatchPoster {
    async fn submit_batches(
        &self,
        _auth: crate::runtime::shutdown::Authorized<'_>,
        payloads: Vec<Vec<u8>>,
        watermark: &dyn WalletNonceWatermarkSink,
    ) -> Result<Vec<TxHash>, BatchPosterError> {
        if payloads.is_empty() {
            return Ok(Vec::new());
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

        let fees = estimate_fees(&self.provider)
            .await
            .map_err(BatchPosterError::Provider)?;
        let first_nonce = self.latest_account_nonce().await?;

        // Write-before-broadcast (R1a): durably cover every nonce this
        // tick will use before the first send. One raise to the highest
        // covers the whole consecutive range.
        let highest_nonce = checked_highest_wallet_nonce(first_nonce, payloads.len())?;
        if self.shutdown.is_storage_invariant_contained() {
            return Err(BatchPosterError::StorageInvariantViolation);
        }
        watermark.raise_to(highest_nonce)?;

        let mut tx_hashes = Vec::with_capacity(payloads.len());

        for (offset, payload) in payloads.into_iter().enumerate() {
            let offset =
                u64::try_from(offset).expect("validated wallet-nonce range offset must fit in u64");
            let nonce = first_nonce
                .checked_add(offset)
                .expect("validated wallet-nonce range must not overflow");
            let pending = externalize_provider_call(
                &self.shutdown,
                self.send_batch_at_nonce(payload, nonce, &fees),
            )
            .await?;
            let tx_hash = *pending.tx_hash();
            debug!(
                tx_nonce = nonce,
                %tx_hash,
                confirmation_depth = self.config.confirmation_depth,
                "sent batch submission tx to L1"
            );
            tx_hashes.push(tx_hash);
        }

        self.wait_for_confirmations(tx_hashes.as_slice()).await?;
        Ok(tx_hashes)
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
    use super::{Batch, BatchPoster, BatchPosterError, TxHash};
    use crate::l1::watermark::WalletNonceWatermarkSink;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Debug)]
    pub struct MockBatchPoster {
        pub submissions: Mutex<Vec<(u64, usize)>>,
        pub observed_submitted_nonces: Mutex<Vec<u64>>,
        pub observed_submitted_error: Mutex<Option<String>>,
        pub last_from_block: Mutex<Option<u64>>,
    }

    impl MockBatchPoster {
        pub fn new() -> Self {
            Self {
                submissions: Mutex::new(Vec::new()),
                observed_submitted_nonces: Mutex::new(Vec::new()),
                observed_submitted_error: Mutex::new(None),
                last_from_block: Mutex::new(None),
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
    }

    #[async_trait]
    impl BatchPoster for MockBatchPoster {
        async fn submit_batches(
            &self,
            _auth: crate::runtime::shutdown::Authorized<'_>,
            payloads: Vec<Vec<u8>>,
            _watermark: &dyn WalletNonceWatermarkSink,
        ) -> Result<Vec<TxHash>, BatchPosterError> {
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
            Ok(tx_hashes)
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
    use std::sync::Mutex;
    use std::time::Duration;

    use super::{
        BatchPoster, BatchPosterConfig, BatchPosterError, EthereumBatchPoster,
        checked_highest_wallet_nonce, derive_confirmation_timeout, externalize_provider_call,
        mock::MockBatchPoster,
    };
    use crate::l1::watermark::{WalletNonceWatermarkError, WalletNonceWatermarkSink};
    use crate::runtime::shutdown::RuntimeScope;
    use alloy::node_bindings::Anvil;
    use alloy::providers::Provider;
    use alloy::rpc::types::BlockNumberOrTag;

    #[test]
    fn wallet_nonce_range_is_checked_before_watermark_or_broadcast() {
        assert_eq!(
            checked_highest_wallet_nonce(i64::MAX as u64 - 1, 2).expect("representable range"),
            i64::MAX as u64
        );
        assert!(matches!(
            checked_highest_wallet_nonce(i64::MAX as u64, 2),
            Err(BatchPosterError::WalletNonceRangeUnrepresentable { .. })
        ));
        assert!(matches!(
            checked_highest_wallet_nonce(u64::MAX, 2),
            Err(BatchPosterError::WalletNonceRangeUnrepresentable { .. })
        ));
    }

    #[tokio::test]
    async fn ordinary_shutdown_cancels_provider_send_without_terminal_classification() {
        let shutdown = RuntimeScope::default();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let send = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                externalize_provider_call(&shutdown, async move {
                    started_tx.send(()).expect("mark provider send started");
                    std::future::pending::<Result<(), BatchPosterError>>().await
                })
                .await
            }
        });
        started_rx.await.expect("provider send acquired the gate");

        shutdown.request_shutdown();

        assert!(matches!(
            send.await.expect("provider send task"),
            Err(BatchPosterError::Shutdown)
        ));
        assert!(
            !shutdown.is_storage_invariant_contained(),
            "ordinary shutdown cancellation must remain nonterminal"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_close_cancels_provider_send_and_finishes_publication() {
        let shutdown = RuntimeScope::default();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let send = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                externalize_provider_call(&shutdown, async move {
                    started_tx.send(()).expect("mark provider send started");
                    std::future::pending::<Result<(), BatchPosterError>>().await
                })
                .await
            }
        });
        started_rx.await.expect("provider send acquired the gate");

        // Containment is sync and never waits — a stalled provider cannot
        // delay the durable verdict.
        shutdown.contain_storage_invariant_failure("test fault");

        assert!(matches!(
            send.await.expect("provider send task"),
            Err(BatchPosterError::StorageInvariantViolation)
        ));
        assert!(shutdown.is_storage_invariant_contained());
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
        fn raise_to(&self, highest: u64) -> Result<(), WalletNonceWatermarkError> {
            self.calls.lock().expect("lock").push(highest);
            if self.fail {
                Err(WalletNonceWatermarkError::Other(
                    "recording sink: forced failure".to_string(),
                ))
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
        let poster = EthereumBatchPoster::new(provider.clone(), config, RuntimeScope::default());

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        let sink = RecordingWatermarkSink::failing();
        let payloads = vec![vec![0u8; 4], vec![1u8; 4], vec![2u8; 4]]; // 3 consecutive nonces

        let scope = RuntimeScope::default();
        let result = poster
            .submit_batches(scope.authorize().expect("clear scope"), payloads, &sink)
            .await;

        assert!(
            matches!(
                result,
                Err(BatchPosterError::Watermark(
                    WalletNonceWatermarkError::Other(_)
                ))
            ),
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
        let poster = EthereumBatchPoster::new(provider.clone(), config, RuntimeScope::default());

        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        // A sink that would *succeed* — so the only thing that can stop a send is
        // the chain-id gate, not the watermark guard. (Recording proves the gate
        // fires first: a passing chain check would reach `raise_to`.)
        let sink = RecordingWatermarkSink::passing();
        let payloads = vec![vec![0u8; 4], vec![1u8; 4]];

        let scope = RuntimeScope::default();
        let result = poster
            .submit_batches(scope.authorize().expect("clear scope"), payloads, &sink)
            .await;

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
    async fn terminal_storage_fault_blocks_watermark_and_broadcast() {
        require_anvil();
        let anvil = Anvil::default().spawn();
        let key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let submitter = alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let provider = crate::l1::provider::create_signer_provider(&anvil.endpoint(), key, false)
            .expect("signer provider");
        let shutdown = RuntimeScope::default();
        let poster = EthereumBatchPoster::new(
            provider.clone(),
            BatchPosterConfig {
                l1_submit_address: alloy_primitives::Address::repeat_byte(0x11),
                app_address: alloy_primitives::Address::repeat_byte(0x22),
                batch_submitter_address: submitter,
                start_block: 0,
                confirmation_depth: 0,
                seconds_per_block: 1,
                long_block_range_error_codes: vec![],
                expected_chain_id: anvil.chain_id(),
            },
            shutdown.clone(),
        );
        let base_nonce = provider
            .get_transaction_count(submitter)
            .await
            .expect("base nonce");
        let sink = RecordingWatermarkSink::passing();
        // Mint the token BEFORE the fault is contained: the honest race the
        // ADR accepts. The poster's inner per-send gate must still refuse a
        // stale token's send.
        let auth = shutdown
            .authorize()
            .expect("token minted before the fault is contained");
        shutdown.contain_storage_invariant_failure("test fault");

        let result = poster.submit_batches(auth, vec![vec![0u8; 4]], &sink).await;

        assert!(matches!(
            result,
            Err(BatchPosterError::StorageInvariantViolation)
        ));
        assert!(
            sink.calls().is_empty(),
            "terminal gate must close before the watermark write"
        );
        let pending = provider
            .get_transaction_count(submitter)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        assert_eq!(
            pending, base_nonce,
            "terminal gate must prevent an L1 broadcast"
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
}
