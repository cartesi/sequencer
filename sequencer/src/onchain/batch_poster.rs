// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy::providers::Provider;
use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use cartesi_rollups_contracts::input_box::InputBox;
use thiserror::Error;

/// Alias for the transaction hash type returned by the L1 provider.
pub type TxHash = B256;

#[derive(Debug, Clone)]
pub struct BatchPosterConfig {
    pub rpc_url: String,
    /// InputBox contract address that receives batch submissions as inputs.
    pub l1_submit_address: Address,
    /// Application / dapp address to which inputs are addressed when calling
    /// `InputBox.addInput`. This is the same contract used as the EIP-712
    /// verifying contract in the sequencer API.
    pub app_address: Address,
}

#[derive(Debug, Error)]
pub enum BatchPosterError {
    #[error("provider/transport: {0}")]
    Provider(String),
    #[error("contract interaction not yet implemented: {0}")]
    NotImplemented(&'static str),
}

#[async_trait]
pub trait BatchPoster: Send + Sync {
    /// Submits a batch payload whose header encodes the batch index as a nonce.
    ///
    /// Same batch may be submitted more than once (at-least-once); the scheduler invalidates
    /// duplicates by nonce based on the encoded batch index.
    async fn submit_batch(&self, payload: Vec<u8>) -> Result<TxHash, BatchPosterError>;
}

/// Ethereum implementation of the `BatchPoster` trait backed by an Alloy provider.
///
/// Posts batch payloads to L1 via the Cartesi `InputBox` contract using
/// `addInput(app_address, payload)`. The scheduler that validates and
/// deduplicates by batch nonce runs offchain.
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
            .map_err(|e| BatchPosterError::Provider(e.to_string()))?;

        Ok(*pending.tx_hash())
    }
}

#[cfg(test)]
pub(crate) mod mock {
    //! Mock `BatchPoster` for unit and integration tests.

    use super::{BatchPoster, BatchPosterError, TxHash};
    use alloy_primitives::B256;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Mock poster that records submissions and can be configured to fail on submit.
    #[derive(Debug)]
    pub struct MockBatchPoster {
        /// Record of (batch_index, payload_len) for each submit_batch call.
        pub submissions: Mutex<Vec<(u64, usize)>>,
        /// If true, submit_batch returns an error.
        pub fail_submit: Mutex<bool>,
    }

    impl MockBatchPoster {
        pub fn new() -> Self {
            Self {
                submissions: Mutex::new(Vec::new()),
                fail_submit: Mutex::new(false),
            }
        }

        pub fn submissions(&self) -> Vec<(u64, usize)> {
            self.submissions.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl BatchPoster for MockBatchPoster {
        async fn submit_batch(&self, payload: Vec<u8>) -> Result<TxHash, BatchPosterError> {
            if *self.fail_submit.lock().expect("lock") {
                return Err(BatchPosterError::Provider("mock submit fail".into()));
            }
            // Decode batch index from payload header for tests: [tag, nonce_be, body...].
            let batch_index = if payload.len() >= 9 {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&payload[1..9]);
                u64::from_be_bytes(bytes)
            } else {
                0
            };
            self.submissions
                .lock()
                .expect("lock")
                .push((batch_index, payload.len()));
            Ok(B256::ZERO)
        }
    }
}
