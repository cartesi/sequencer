// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy::providers::Provider;
use async_trait::async_trait;
use cartesi_rollups_contracts::input_box::InputBox;
use sequencer_core::batch::Batch;
use thiserror::Error;

use crate::partition::{decode_evm_advance_input, get_input_added_events};

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
    async fn submit_batch(&self, payload: Vec<u8>) -> Result<TxHash, BatchPosterError>;

    async fn observed_submitted_batch_nonces(
        &self,
        from_block: u64,
    ) -> Result<Vec<u64>, BatchPosterError>;
}

#[derive(Clone)]
pub struct EthereumBatchPoster<P: Provider + Send + Sync + Clone + 'static> {
    provider: P,
    config: BatchPosterConfig,
}

impl<P> EthereumBatchPoster<P>
where
    P: Provider + Send + Sync + Clone + 'static,
{
    pub fn new(provider: P, config: BatchPosterConfig) -> Self {
        Self { provider, config }
    }
}

#[async_trait]
impl<P> BatchPoster for EthereumBatchPoster<P>
where
    P: Provider + Send + Sync + Clone + 'static,
{
    async fn submit_batch(&self, payload: Vec<u8>) -> Result<TxHash, BatchPosterError> {
        let input_box = InputBox::new(self.config.l1_submit_address, &self.provider);
        let pending = input_box
            .addInput(self.config.app_address, payload.into())
            .send()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))?;
        let tx_hash = *pending.tx_hash();

        pending
            .with_required_confirmations(self.config.confirmation_depth.saturating_add(1))
            .watch()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))?;

        Ok(tx_hash)
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
        let end_block = latest.saturating_sub(self.config.confirmation_depth);
        let start_block = from_block.max(self.config.start_block);
        if start_block > end_block {
            return Ok(Vec::new());
        }

        let events = get_input_added_events(
            &self.provider,
            self.config.app_address,
            &self.config.l1_submit_address,
            start_block,
            end_block,
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
        async fn submit_batch(&self, payload: Vec<u8>) -> Result<TxHash, BatchPosterError> {
            if *self.fail_submit.lock().expect("lock") {
                return Err(BatchPosterError::Provider("mock submit fail".into()));
            }
            let batch_index = ssz::Decode::from_ssz_bytes(payload.as_ref())
                .map(|b: Batch| b.nonce)
                .unwrap_or(0);
            self.submissions
                .lock()
                .expect("lock")
                .push((batch_index, payload.len()));
            Ok(TxHash::ZERO)
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
