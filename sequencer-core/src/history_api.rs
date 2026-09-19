// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Historical scheduler inputs and their handoff to the application feed.

use alloy_primitives::{Address, B256, Bytes};
use serde::{Deserialize, Serialize};

use crate::history::{EraId, ExecutedInputCount, HistoryBounds, RecoveryGeneration};

pub const HISTORICAL_INPUT_MAX_ITEMS: usize = 256;
/// A larger first input is served alone, preserving progress through any history.
pub const HISTORICAL_INPUT_PAYLOAD_TARGET_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryDeployment {
    pub chain_id: u64,
    pub app_address: Address,
    pub input_box_address: Address,
    pub app_deployment_block: u64,
    pub batch_submitter_address: Address,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryBaseline {
    pub l1_stop_block: u64,
    /// Exclusive end of the per-application InputBox prefix through the stop block.
    pub l1_end_input_index: u64,
    pub next_batch_nonce: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedCheckpoint {
    pub inclusion_block: u64,
    pub executed_input_count: ExecutedInputCount,
    pub next_batch_nonce: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryInfo {
    pub deployment: HistoryDeployment,
    pub history: HistoryBounds,
    pub baseline: HistoryBaseline,
    /// The accepted boundary shares `history.version`; it does not certify client state.
    pub accepted_checkpoint: Option<AcceptedCheckpoint>,
    pub compatibility: Option<HistoryCompatibility>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCompatibility {
    pub from_generation: RecoveryGeneration,
    /// Checkpoint counts up to and including this boundary preserve their input prefix.
    pub preserved_input_count: ExecutedInputCount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoricalL1InputStart {
    NextInputIndex(u64),
    AfterBlock(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalL1Input {
    pub input_index: u64,
    pub sender: Address,
    /// Original inner application/batch payload, including malformed batches.
    pub payload: Bytes,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub transaction_hash: B256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalL1InputsPage {
    pub era_id: EraId,
    pub l1_stop_block: u64,
    pub end_input_index: u64,
    /// Only equality with `end_input_index` establishes completion, not page length.
    pub next_input_index: u64,
    pub items: Vec<HistoricalL1Input>,
}
