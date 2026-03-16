// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy::providers::Provider;
use async_trait::async_trait;
use cartesi_rollups_contracts::input_box::InputBox;
use sequencer_core::batch::Batch;
use thiserror::Error;

use crate::input_reader::logs::{decode_evm_advance_input, get_input_added_events};

pub type TxHash = alloy_primitives::B256;

#[derive(Debug, Clone)]
pub struct BatchPosterConfig {
    pub l1_submit_address: alloy_primitives::Address,
    pub app_address: alloy_primitives::Address,
    pub batch_submitter_address: alloy_primitives::Address,
    pub start_block: u64,
    pub confirmation_depth: u64,
}

#[derive(Debug, Error)]
pub enum BatchPosterError {
    #[error("provider/transport: {0}")]
    Provider(String),
}

#[async_trait]
pub trait BatchPoster: Send + Sync {
    async fn submit_batch(&self, payload: Vec<u8>) -> Result<TxHash, BatchPosterError>;

    async fn latest_submitted_batch_index(&self) -> Result<Option<u64>, BatchPosterError>;
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

    async fn latest_submitted_batch_index(&self) -> Result<Option<u64>, BatchPosterError> {
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))?;
        let end_block = latest.saturating_sub(self.config.confirmation_depth);
        if self.config.start_block > end_block {
            return Ok(None);
        }

        let events = get_input_added_events(
            &self.provider,
            self.config.app_address,
            &self.config.l1_submit_address,
            self.config.start_block,
            end_block,
            &[],
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

        Ok(latest_accepted_batch_nonce(observed_nonces))
    }
}

fn latest_accepted_batch_nonce(observed_nonces: Vec<u64>) -> Option<u64> {
    let mut expected = 0_u64;
    for nonce in observed_nonces {
        if nonce == expected {
            expected = expected.saturating_add(1);
        }
    }

    expected.checked_sub(1)
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{Batch, BatchPoster, BatchPosterError, TxHash, latest_accepted_batch_nonce};
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Debug)]
    pub struct MockBatchPoster {
        pub submissions: Mutex<Vec<(u64, usize)>>,
        pub fail_submit: Mutex<bool>,
        pub latest_submitted: Mutex<Option<u64>>,
        pub latest_submitted_error: Mutex<Option<String>>,
    }

    impl MockBatchPoster {
        pub fn new() -> Self {
            Self {
                submissions: Mutex::new(Vec::new()),
                fail_submit: Mutex::new(false),
                latest_submitted: Mutex::new(None),
                latest_submitted_error: Mutex::new(None),
            }
        }

        pub fn submissions(&self) -> Vec<(u64, usize)> {
            self.submissions.lock().expect("lock").clone()
        }

        pub fn set_latest_submitted(&self, value: Option<u64>) {
            *self.latest_submitted.lock().expect("lock") = value;
        }

        pub fn set_latest_submitted_error(&self, value: Option<&str>) {
            *self.latest_submitted_error.lock().expect("lock") = value.map(str::to_string);
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

        async fn latest_submitted_batch_index(&self) -> Result<Option<u64>, BatchPosterError> {
            if let Some(err) = self.latest_submitted_error.lock().expect("lock").clone() {
                return Err(BatchPosterError::Provider(err));
            }
            if let Some(value) = *self.latest_submitted.lock().expect("lock") {
                return Ok(Some(value));
            }
            let observed_nonces = self
                .submissions
                .lock()
                .expect("lock")
                .iter()
                .map(|(idx, _)| *idx)
                .collect();
            Ok(latest_accepted_batch_nonce(observed_nonces))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::latest_accepted_batch_nonce;

    #[test]
    fn latest_accepted_batch_nonce_matches_scheduler_nonce_rule() {
        assert_eq!(latest_accepted_batch_nonce(Vec::new()), None);
        assert_eq!(latest_accepted_batch_nonce(vec![0, 1, 2]), Some(2));
        assert_eq!(latest_accepted_batch_nonce(vec![0, 2, 3]), Some(0));
        assert_eq!(latest_accepted_batch_nonce(vec![1, 2, 3]), None);
        assert_eq!(latest_accepted_batch_nonce(vec![0, 1, 1, 2]), Some(2));
        assert_eq!(
            latest_accepted_batch_nonce(vec![6, 4, 3, 2, 2, 0, 1]),
            Some(1)
        );
        assert_eq!(latest_accepted_batch_nonce(vec![0, 2, 1]), Some(1));
    }
}
