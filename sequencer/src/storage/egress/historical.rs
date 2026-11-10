// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `/history`'s metadata: the era, its baseline and the accepted checkpoint, in one read.

use sequencer_core::history::{EraId, HistoryBounds, HistoryPolicyError, RecoveryGeneration};
use sequencer_core::history_api::{
    AcceptedCheckpoint, HistoryBaseline, HistoryCompatibility, HistoryDeployment, HistoryInfo,
};

use crate::storage::Storage;
use crate::storage::history::{
    era_in, history_version_in, next_l2_tx_count_of, preserved_input_count_in,
};
use crate::storage::l1_inputs::query_deployment_identity;
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
            let era = era_in(tx)?;
            let version = history_version_in(tx)?;
            if expected_era.is_some_and(|expected| expected != version.era_id) {
                return Ok(Err(
                    HistoryPolicyError::EraChanged { current: version }.into()
                ));
            }
            if from_generation.is_some() && expected_era.is_none() {
                return Ok(Err(HistoricalReadError::BadRequest(
                    "era_id is required with from_generation".to_owned(),
                )));
            }
            if from_generation.is_some_and(|from| from > version.recovery_generation) {
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
            let head = next_l2_tx_count_of(tx, &era)?;
            let compatibility = from_generation
                .map(|from| {
                    preserved_input_count_in(tx, from, version.recovery_generation, head).map(
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
                    version,
                    available_from: era.base,
                    head,
                },
                baseline: HistoryBaseline {
                    l1_stop_block: era.stop_block,
                    l1_end_input_index: safe_input_count_through_block_in(tx, era.stop_block)?,
                    next_batch_nonce: era.anchor_batch_nonce,
                    rebuilt_history_end: era.rebuilt_end,
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
