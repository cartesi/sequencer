// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::sync::Mutex;

use alloy::providers::Provider;
use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use cartesi_rollups_contracts::input_box::InputBox;
use sequencer_core::batch::Batch;
use thiserror::Error;

use crate::input_reader::logs::{get_input_added_events, get_transaction_sender};

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
    /// EOA whose inputs are batch submissions. Only these are counted when deriving latest submitted batch index.
    pub batch_submitter_address: Address,
    /// Number of blocks behind Latest to treat as confirmed; scan range is [previous_scanned+1 ..= Latest-scan_depth].
    pub scan_depth: u64,
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

    /// Returns the highest batch index observed as submitted on the chain in the confirmed
    /// range [next_scan_block_start ..= Latest - scan_depth]. Used each tick so S is not
    /// persisted and reorgs are recovered automatically.
    async fn latest_submitted_batch_index(&self) -> Result<Option<u64>, BatchPosterError>;
}

/// State shared across clones for incremental scan and max nonce.
#[derive(Debug, Default)]
struct ScanState {
    /// Inclusive start block for the next scan (0 = start from genesis).
    next_scan_block_start: u64,
    /// Highest batch nonce seen in confirmed range.
    max_nonce_seen: Option<u64>,
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
    /// Shared state for incremental scan [next_scan_block_start ..= Latest-scan_depth].
    state: std::sync::Arc<Mutex<ScanState>>,
}

impl<P> EthereumBatchPoster<P>
where
    P: Provider + Send + Sync + Clone + 'static,
{
    pub fn new(provider: P, config: BatchPosterConfig) -> Self {
        Self {
            provider,
            config,
            state: std::sync::Arc::new(Mutex::new(ScanState::default())),
        }
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

    async fn latest_submitted_batch_index(&self) -> Result<Option<u64>, BatchPosterError> {
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(|e| BatchPosterError::Provider(e.to_string()))?;
        let end_block = latest.saturating_sub(self.config.scan_depth);
        let (start_block, current_max) = {
            let state = self.state.lock().map_err(|e| {
                BatchPosterError::Provider(format!("scan state lock poisoned: {e}"))
            })?;
            (state.next_scan_block_start, state.max_nonce_seen)
        };
        if start_block > end_block {
            return Ok(current_max);
        }
        let events = get_input_added_events(
            &self.provider,
            self.config.app_address,
            &self.config.l1_submit_address,
            start_block,
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
        // Only events from the sequencer (batch submitter) count; batch index comes from SSZ decode.
        let mut scan_max = current_max;
        for (event, log) in &events {
            let tx_hash = match log.transaction_hash {
                Some(h) => h,
                None => continue,
            };
            let sender = match get_transaction_sender(&self.provider, tx_hash).await {
                Some(s) => s,
                None => continue,
            };
            if sender != self.config.batch_submitter_address {
                continue;
            }
            let batch: Batch = match ssz::Decode::from_ssz_bytes(event.input.as_ref()) {
                Ok(b) => b,
                Err(_) => continue,
            };
            scan_max = Some(scan_max.map_or(batch.nonce, |m| m.max(batch.nonce)));
        }
        {
            let mut state = self.state.lock().map_err(|e| {
                BatchPosterError::Provider(format!("scan state lock poisoned: {e}"))
            })?;
            state.next_scan_block_start = end_block.saturating_add(1);
            state.max_nonce_seen = scan_max;
        }
        Ok(scan_max)
    }
}

#[cfg(test)]
pub(crate) mod mock {
    //! Mock `BatchPoster` for unit and integration tests.

    use super::{Batch, BatchPoster, BatchPosterError, TxHash};
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
            let batch_index = ssz::Decode::from_ssz_bytes(payload.as_ref())
                .map(|b: Batch| b.nonce)
                .unwrap_or(0);
            self.submissions
                .lock()
                .expect("lock")
                .push((batch_index, payload.len()));
            Ok(B256::ZERO)
        }

        async fn latest_submitted_batch_index(&self) -> Result<Option<u64>, BatchPosterError> {
            Ok(self
                .submissions
                .lock()
                .expect("lock")
                .iter()
                .map(|(idx, _)| *idx)
                .max())
        }
    }
}
