// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Historical scheduler inputs and their handoff to the application feed.

use alloy_primitives::{Address, B256, Bytes};
use serde::{Deserialize, Serialize};

use crate::history::{EraId, ExecutedInputCount, HistoryBounds, HistoryClaim, RecoveryGeneration};
use thiserror::Error;

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

/// Where the era's rebuild ended. Its history starts at `available_from`, the
/// base's count, and setup rebuilt it from L1 through the stop block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryBaseline {
    pub l1_stop_block: u64,
    /// Exclusive end of the per-application InputBox prefix through the stop block.
    pub l1_end_input_index: u64,
    /// The scheduler's next batch nonce at the stop block.
    pub next_batch_nonce: u64,
    /// The L2 tx count at the end of the stop block: the rebuilt history ends
    /// here and the live rows start.
    pub rebuilt_history_end: ExecutedInputCount,
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

impl HistoryInfo {
    /// The claim a client holding state at `count` from another era resumes
    /// this one with. Every count from the base through the rebuilt history's
    /// end is one this era's history reaches through rows that landed on L1;
    /// past that end the client holds rows that never landed.
    pub fn adopt_claim(&self, count: ExecutedInputCount) -> Result<HistoryClaim, AdoptError> {
        if count < self.history.available_from {
            return Err(AdoptError::BelowBase {
                available_from: self.history.available_from,
            });
        }
        if count > self.baseline.rebuilt_history_end {
            return Err(AdoptError::PastRebuiltEnd {
                rebuilt_history_end: self.baseline.rebuilt_history_end,
            });
        }
        Ok(HistoryClaim {
            version: self.history.version,
            next_input: count,
        })
    }
}

/// Why a client's count cannot adopt the current era.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AdoptError {
    /// The era starts after the client's state: recover from an older base.
    #[error("count is below the era's base count {available_from}", available_from = available_from.get())]
    BelowBase { available_from: ExecutedInputCount },
    /// The client's state holds rows that never landed by the stop block:
    /// restore an older state.
    #[error("count is past the era's rebuilt history end {rebuilt_history_end}", rebuilt_history_end = rebuilt_history_end.get())]
    PastRebuiltEnd {
        rebuilt_history_end: ExecutedInputCount,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryVersion;

    fn info(base: u64, rebuilt_end: u64, head: u64) -> HistoryInfo {
        HistoryInfo {
            deployment: HistoryDeployment {
                chain_id: 1,
                app_address: Address::repeat_byte(1),
                input_box_address: Address::repeat_byte(2),
                app_deployment_block: 0,
                batch_submitter_address: Address::repeat_byte(3),
            },
            history: HistoryBounds {
                version: HistoryVersion {
                    era_id: "00112233-4455-4677-8899-aabbccddeeff".parse().unwrap(),
                    recovery_generation: RecoveryGeneration::new(2),
                },
                available_from: ExecutedInputCount::new(base),
                head: ExecutedInputCount::new(head),
            },
            baseline: HistoryBaseline {
                l1_stop_block: 50,
                l1_end_input_index: 9,
                next_batch_nonce: 4,
                rebuilt_history_end: ExecutedInputCount::new(rebuilt_end),
            },
            accepted_checkpoint: None,
            compatibility: None,
        }
    }

    #[test]
    fn a_count_from_the_base_through_the_rebuilt_end_adopts_the_current_version() {
        let info = info(41, 45, 60);
        for count in [41, 43, 45] {
            let claim = info.adopt_claim(ExecutedInputCount::new(count)).unwrap();
            assert_eq!(claim.version, info.history.version);
            assert_eq!(claim.next_input, ExecutedInputCount::new(count));
            assert_eq!(info.history.validate(claim), Ok(()));
        }
        let empty_rebuild = self::info(41, 41, 41);
        assert!(
            empty_rebuild
                .adopt_claim(ExecutedInputCount::new(41))
                .is_ok()
        );
    }

    #[test]
    fn a_count_outside_the_rebuilt_range_cannot_adopt() {
        let info = info(41, 45, 60);
        for count in [0, 40] {
            assert_eq!(
                info.adopt_claim(ExecutedInputCount::new(count)),
                Err(AdoptError::BelowBase {
                    available_from: ExecutedInputCount::new(41)
                })
            );
        }
        // Live rows past the rebuilt end were served by this era, not landed
        // before its stop block, so an older era's state there is not canonical.
        for count in [46, 60, u64::MAX] {
            assert_eq!(
                info.adopt_claim(ExecutedInputCount::new(count)),
                Err(AdoptError::PastRebuiltEnd {
                    rebuilt_history_end: ExecutedInputCount::new(45)
                })
            );
        }
    }

    #[test]
    fn the_baseline_carries_the_rebuilt_end_on_the_wire() {
        let baseline = info(41, 45, 60).baseline;
        let json = serde_json::to_value(baseline).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "l1_stop_block": 50,
                "l1_end_input_index": 9,
                "next_batch_nonce": 4,
                "rebuilt_history_end": 45
            })
        );
        assert_eq!(
            serde_json::from_value::<HistoryBaseline>(json).unwrap(),
            baseline
        );
    }
}
