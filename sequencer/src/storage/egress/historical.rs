// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `/history`'s metadata: the era, its baseline and the accepted checkpoint, in one read.

use sequencer_core::history::{EraId, HistoryPolicyError};
use sequencer_core::history_api::{
    AcceptedCheckpoint, HistoryBaseline, HistoryDeployment, HistoryInfo,
};

use crate::storage::Storage;
use crate::storage::history::{era_in, history_bounds_from};
use crate::storage::l1_inputs::query_deployment_identity;
use crate::storage::queries::safe_input_count_through_block_in;
use crate::storage::snapshots::{
    FinalizedSelectionError, finalized_dump_in, has_accepted_snapshot_in,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoricalReadError {
    #[error(transparent)]
    Policy(#[from] HistoryPolicyError),
    #[error(transparent)]
    Checkpoint(#[from] FinalizedSelectionError),
    #[error("reading history: {0}")]
    Storage(#[from] rusqlite::Error),
}

impl Storage {
    pub(crate) fn history_info(
        &mut self,
        expected_era: Option<EraId>,
    ) -> Result<HistoryInfo, HistoricalReadError> {
        self.read(|tx| {
            let era = era_in(tx)?;
            if expected_era.is_some_and(|expected| expected != era.id) {
                return Ok(Err(HistoryPolicyError::EraChanged { era_id: era.id }.into()));
            }
            let accepted = match finalized_dump_in(tx) {
                Ok(accepted) => accepted,
                Err(error) => return Ok(Err(error.into())),
            };
            let deployment =
                query_deployment_identity(tx)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            if accepted.is_none() {
                assert!(
                    has_accepted_snapshot_in(tx)?,
                    "history has no accepted snapshot"
                );
            }
            Ok(Ok(HistoryInfo {
                deployment: HistoryDeployment {
                    chain_id: deployment.chain_id,
                    app_address: deployment.app_address,
                    input_box_address: deployment.input_box_address,
                    app_deployment_block: deployment.app_deployment_block,
                    batch_submitter_address: deployment.batch_submitter_address,
                },
                history: history_bounds_from(tx, &era)?,
                baseline: HistoryBaseline {
                    l1_stop_block: era.stop_block,
                    l1_end_input_index: safe_input_count_through_block_in(tx, era.stop_block)?,
                    next_batch_nonce: era.anchor_batch_nonce,
                    rebuilt_history_end: era.rebuilt_end,
                },
                accepted_checkpoint: accepted.map(|finalized| AcceptedCheckpoint {
                    inclusion_block: finalized.inclusion_block,
                    executed_input_count: finalized.snapshot.l2_tx_count,
                    next_batch_nonce: finalized.next_batch_nonce,
                }),
            }))
        })?
    }
}

#[cfg(test)]
mod tests;
