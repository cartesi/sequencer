// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `/history`'s metadata: the era, its baseline and the accepted checkpoint, in one read.

use sequencer_core::history::{
    EraId, HistoryBounds, HistoryPolicyError, L2TxCount, RecoveryGeneration,
};
use sequencer_core::history_api::{
    AcceptedCheckpoint, HistoryBaseline, HistoryCompatibility, HistoryDeployment, HistoryInfo,
};

use crate::storage::Storage;
use crate::storage::history::{next_l2_tx_count_in, preserved_input_count_in, query_history_state};
use crate::storage::l1_inputs::query_deployment_identity;
use crate::storage::mutations::batch_tree_anchor_in;
use crate::storage::queries::safe_input_count_through_block_in;
use crate::storage::snapshot_dumps::{
    FinalizedSelectionError, finalized_dump_in, has_rollback_safe_snapshot_in,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoricalReadError {
    #[error(transparent)]
    Policy(#[from] HistoryPolicyError),
    #[error("{0}")]
    BadRequest(String),
    #[error(transparent)]
    Checkpoint(#[from] FinalizedSelectionError),
    #[error("reading history: {0}")]
    Storage(#[from] rusqlite::Error),
}

impl Storage {
    pub(crate) fn history_info(
        &mut self,
        expected_era: Option<EraId>,
        from_generation: Option<RecoveryGeneration>,
    ) -> Result<HistoryInfo, HistoricalReadError> {
        self.read(|tx| {
            let state = query_history_state(tx)?;
            if expected_era.is_some_and(|era| era != state.version.era_id) {
                return Ok(Err(HistoryPolicyError::EraChanged {
                    current: state.version,
                }
                .into()));
            }
            if from_generation.is_some() && expected_era.is_none() {
                return Ok(Err(HistoricalReadError::BadRequest(
                    "era_id is required with from_generation".to_owned(),
                )));
            }
            if from_generation.is_some_and(|from| from > state.version.recovery_generation) {
                return Ok(Err(HistoricalReadError::BadRequest(
                    "from_generation exceeds the current recovery generation".to_owned(),
                )));
            }
            let accepted = match finalized_dump_in(tx) {
                Ok(accepted) => accepted,
                Err(error) => return Ok(Err(error.into())),
            };
            let deployment =
                query_deployment_identity(tx)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            if accepted.is_none() {
                assert!(
                    has_rollback_safe_snapshot_in(tx)?,
                    "history has no rollback-safe snapshot"
                );
            }
            let head = next_l2_tx_count_in(tx)?;
            let compatibility = from_generation
                .map(|from| {
                    preserved_input_count_in(tx, from, state.version.recovery_generation, head).map(
                        |preserved_input_count| HistoryCompatibility {
                            from_generation: from,
                            preserved_input_count,
                        },
                    )
                })
                .transpose()?;
            Ok(Ok(HistoryInfo {
                deployment: HistoryDeployment {
                    chain_id: deployment.chain_id,
                    app_address: deployment.app_address,
                    input_box_address: deployment.input_box_address,
                    app_deployment_block: deployment.app_deployment_block,
                    batch_submitter_address: deployment.batch_submitter_address,
                },
                history: HistoryBounds {
                    version: state.version,
                    available_from: L2TxCount::new(state.base_l2_tx_count),
                    head,
                },
                baseline: HistoryBaseline {
                    l1_stop_block: state.base_safe_block,
                    l1_end_input_index: safe_input_count_through_block_in(
                        tx,
                        state.base_safe_block,
                    )?,
                    next_batch_nonce: batch_tree_anchor_in(tx)?,
                    rebuilt_history_end: L2TxCount::new(state.rebuilt_history_end),
                },
                accepted_checkpoint: accepted.map(|snapshot| AcceptedCheckpoint {
                    inclusion_block: snapshot.inclusion_block,
                    executed_input_count: snapshot.l2_tx_count,
                    next_batch_nonce: snapshot.next_batch_nonce,
                }),
                compatibility,
            }))
        })?
    }
}

#[cfg(test)]
mod tests;
