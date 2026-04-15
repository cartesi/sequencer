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
use tracing::{debug, info, warn};

use crate::l1::partition::{decode_evm_advance_input, get_input_added_events};

pub type TxHash = alloy_primitives::B256;

#[derive(Debug, Clone)]
pub struct BatchPosterConfig {
    pub l1_submit_address: alloy_primitives::Address,
    pub app_address: alloy_primitives::Address,
    pub batch_submitter_address: alloy_primitives::Address,
    pub start_block: u64,
    pub confirmation_depth: u64,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    pub long_block_range_error_codes: Vec<String>,
}

#[derive(Debug, Error)]
pub enum BatchPosterError {
    #[error("provider/transport: {0}")]
    Provider(String),
}

#[async_trait]
pub trait BatchPoster: Send + Sync {
    async fn submit_batches(&self, payloads: Vec<Vec<u8>>)
    -> Result<Vec<TxHash>, BatchPosterError>;

    async fn observed_submitted_batch_nonces(
        &self,
        from_block: u64,
    ) -> Result<Vec<u64>, BatchPosterError>;
}

#[derive(Clone)]
pub struct EthereumBatchPoster {
    provider: DynProvider,
    config: BatchPosterConfig,
}

impl EthereumBatchPoster {
    pub fn new(provider: DynProvider, config: BatchPosterConfig) -> Self {
        Self { provider, config }
    }

    /// Conservative upper-bound timeout for waiting on confirmations.
    /// Uses Ethereum's 12s block time as a worst-case heuristic — shorter block
    /// times on other chains just mean the timeout fires later than necessary,
    /// which is safe (the next tick retries under fresher state).
    fn confirmation_timeout(&self) -> std::time::Duration {
        const ETHEREUM_BLOCK_TIME_SECS: u64 = 12;
        let blocks_to_wait = self
            .config
            .confirmation_depth
            .saturating_add(1)
            .saturating_mul(2);
        std::time::Duration::from_secs(blocks_to_wait.saturating_mul(ETHEREUM_BLOCK_TIME_SECS))
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
        fees: &alloy::providers::utils::Eip1559Estimation,
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

#[async_trait]
impl BatchPoster for EthereumBatchPoster {
    async fn submit_batches(
        &self,
        payloads: Vec<Vec<u8>>,
    ) -> Result<Vec<TxHash>, BatchPosterError> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }

        let fees = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))?;
        let mut next_nonce = self.latest_account_nonce().await?;
        let mut tx_hashes = Vec::with_capacity(payloads.len());

        for payload in payloads {
            let pending = self.send_batch_at_nonce(payload, next_nonce, &fees).await?;
            let tx_hash = *pending.tx_hash();
            debug!(
                tx_nonce = next_nonce,
                %tx_hash,
                confirmation_depth = self.config.confirmation_depth,
                "sent batch submission tx to L1"
            );
            tx_hashes.push(tx_hash);
            next_nonce = next_nonce.saturating_add(1);
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

        let events = get_input_added_events(
            &self.provider,
            self.config.app_address,
            &self.config.l1_submit_address,
            start_block,
            latest,
            self.config.long_block_range_error_codes.as_slice(),
        )
        .await
        .map_err(|errs| {
            BatchPosterError::Provider(
                errs.into_iter()
                    .next()
                    .map(|e| e.to_string())
                    .unwrap_or_default(),
            )
        })?;

        let mut observed_nonces = Vec::new();
        for (event, _log) in events {
            let evm_advance = decode_evm_advance_input(event.input.as_ref())
                .map_err(BatchPosterError::Provider)?;
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
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Debug)]
    pub struct MockBatchPoster {
        pub submissions: Mutex<Vec<(u64, usize)>>,
        pub fail_submit: Mutex<bool>,
        pub observed_submitted_nonces: Mutex<Vec<u64>>,
        pub observed_submitted_error: Mutex<Option<String>>,
        pub last_from_block: Mutex<Option<u64>>,
    }

    impl MockBatchPoster {
        pub fn new() -> Self {
            Self {
                submissions: Mutex::new(Vec::new()),
                fail_submit: Mutex::new(false),
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
            payloads: Vec<Vec<u8>>,
        ) -> Result<Vec<TxHash>, BatchPosterError> {
            if *self.fail_submit.lock().expect("lock") {
                return Err(BatchPosterError::Provider("mock submit fail".into()));
            }
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
    use super::{BatchPoster, mock::MockBatchPoster};

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
}
