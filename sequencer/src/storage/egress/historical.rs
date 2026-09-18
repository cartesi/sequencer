// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Era-pinned raw L1 pages and their immutable handoff to application history.

use alloy_primitives::{Address, B256};
use rusqlite::{Connection, OptionalExtension, params};
use sequencer_core::history::{
    EraId, ExecutedInputCount, HistoryBounds, HistoryPolicyError, RecoveryGeneration,
};
use sequencer_core::history_api::{
    AcceptedCheckpoint, HISTORICAL_INPUT_MAX_ITEMS, HISTORICAL_INPUT_PAYLOAD_TARGET_BYTES,
    HistoricalL1Input, HistoricalL1InputStart, HistoricalL1InputsPage, HistoryBaseline,
    HistoryCompatibility, HistoryDeployment, HistoryInfo,
};

use crate::storage::Storage;
use crate::storage::convert::{i64_to_u64, u64_to_i64};
use crate::storage::history::{
    next_executed_input_count_in, preserved_input_count_in, query_history_state,
};
use crate::storage::l1_inputs::query_deployment_identity;
use crate::storage::mutations::batch_tree_anchor_in;
use crate::storage::safe_accepted_batches::canonical_divergence_in;
use crate::storage::snapshot_dumps::{finalized_dump_in, has_rollback_safe_snapshot_in};

#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoricalReadError {
    #[error(transparent)]
    Policy(#[from] HistoryPolicyError),
    #[error("{0}")]
    BadRequest(String),
    #[error("canonical divergence prevents accepted checkpoint selection")]
    CanonicalDivergence,
    #[error("reading historical L1 inputs: {0}")]
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
            if canonical_divergence_in(tx)?.is_some() {
                return Ok(Err(HistoricalReadError::CanonicalDivergence));
            }
            let deployment =
                query_deployment_identity(tx)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let accepted = finalized_dump_in(tx)?;
            if accepted.is_none() {
                assert!(
                    has_rollback_safe_snapshot_in(tx)?,
                    "history has no rollback-safe snapshot"
                );
            }
            let head = next_executed_input_count_in(tx)?;
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
                    available_from: ExecutedInputCount::new(state.base_executed_input_count),
                    head,
                },
                baseline: HistoryBaseline {
                    l1_stop_block: state.base_safe_block,
                    l1_end_input_index: historical_end_in(tx, state.base_safe_block)?,
                    next_batch_nonce: batch_tree_anchor_in(tx)?,
                },
                accepted_checkpoint: accepted.map(|snapshot| AcceptedCheckpoint {
                    inclusion_block: snapshot.inclusion_block,
                    executed_input_count: snapshot.executed_input_count,
                    next_batch_nonce: snapshot.next_batch_nonce,
                }),
                compatibility,
            }))
        })?
    }

    pub(crate) fn historical_l1_inputs(
        &mut self,
        era: EraId,
        start: HistoricalL1InputStart,
        limit: usize,
    ) -> Result<HistoricalL1InputsPage, HistoricalReadError> {
        self.read(|tx| {
            let state = query_history_state(tx)?;
            if era != state.version.era_id {
                return Ok(Err(HistoryPolicyError::EraChanged {
                    current: state.version,
                }
                .into()));
            }
            let end = historical_end_in(tx, state.base_safe_block)?;
            if !(1..=HISTORICAL_INPUT_MAX_ITEMS).contains(&limit) {
                return Ok(Err(HistoricalReadError::BadRequest(format!(
                    "limit must be between 1 and {HISTORICAL_INPUT_MAX_ITEMS}"
                ))));
            }
            let next = match start {
                HistoricalL1InputStart::NextInputIndex(next) if next <= end => next,
                HistoricalL1InputStart::AfterBlock(block) if block <= state.base_safe_block => {
                    first_input_after_block_in(tx, block, state.base_safe_block)?.unwrap_or(end)
                }
                HistoricalL1InputStart::NextInputIndex(_) => {
                    return Ok(Err(HistoricalReadError::BadRequest(format!(
                        "next_input_index exceeds historical end {end}"
                    ))));
                }
                HistoricalL1InputStart::AfterBlock(_) => {
                    return Ok(Err(HistoricalReadError::BadRequest(format!(
                        "after_block exceeds historical stop block {}",
                        state.base_safe_block
                    ))));
                }
            };
            raw_page_in(tx, era, state.base_safe_block, end, next, limit).map(Ok)
        })?
    }
}

fn historical_end_in(conn: &Connection, stop: u64) -> rusqlite::Result<u64> {
    let last: Option<i64> = conn
        .query_row(
            "SELECT safe_input_index FROM safe_inputs WHERE block_number <= ?1 \
             ORDER BY block_number DESC, safe_input_index DESC LIMIT 1",
            [u64_to_i64(stop)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(last.map_or(0, |index| {
        i64_to_u64(index)
            .checked_add(1)
            .expect("historical input index overflow")
    }))
}

fn first_input_after_block_in(
    conn: &Connection,
    block: u64,
    stop: u64,
) -> rusqlite::Result<Option<u64>> {
    conn.query_row(
        "SELECT safe_input_index FROM safe_inputs WHERE block_number > ?1 AND block_number <= ?2 \
         ORDER BY block_number, safe_input_index LIMIT 1",
        params![u64_to_i64(block), u64_to_i64(stop)],
        |row| Ok(i64_to_u64(row.get(0)?)),
    )
    .optional()
}

fn raw_page_in(
    conn: &Connection,
    era_id: EraId,
    stop: u64,
    end: u64,
    mut next: u64,
    limit: usize,
) -> rusqlite::Result<HistoricalL1InputsPage> {
    let expected_len = (end - next).min(limit as u64);
    let mut items = Vec::new();
    if expected_len > 0 {
        let mut stmt = conn.prepare_cached(
            "SELECT safe_input_index, sender, payload, block_number, block_timestamp, \
                    transaction_hash, length(payload) \
             FROM safe_inputs WHERE safe_input_index >= ?1 AND safe_input_index <= ?2 \
             ORDER BY safe_input_index LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![
            u64_to_i64(next),
            u64_to_i64(end - 1),
            u64_to_i64(expected_len),
        ])?;
        let mut payload_bytes = 0_u64;
        let mut byte_limited = false;
        while let Some(row) = rows.next()? {
            let input_index = i64_to_u64(row.get(0)?);
            assert_eq!(input_index, next, "historical L1 page has an input gap");
            let payload_len = i64_to_u64(row.get(6)?);
            // Budget before copying the BLOB out of SQLite. The first row may
            // exceed the target so every valid L1 input remains consumable.
            if !items.is_empty()
                && payload_len > (HISTORICAL_INPUT_PAYLOAD_TARGET_BYTES as u64 - payload_bytes)
            {
                byte_limited = true;
                break;
            }
            let block_number = i64_to_u64(row.get(3)?);
            assert!(
                block_number <= stop,
                "historical L1 page exceeds its stop block"
            );
            items.push(HistoricalL1Input {
                input_index,
                sender: Address::from_slice(&row.get::<_, Vec<u8>>(1)?),
                payload: row.get::<_, Vec<u8>>(2)?.into(),
                block_number,
                block_timestamp: i64_to_u64(row.get(4)?),
                transaction_hash: B256::from_slice(&row.get::<_, Vec<u8>>(5)?),
            });
            next = next
                .checked_add(1)
                .expect("historical input index overflow");
            payload_bytes += payload_len;
            if payload_bytes > HISTORICAL_INPUT_PAYLOAD_TARGET_BYTES as u64 {
                byte_limited = true;
                break;
            }
        }
        if !byte_limited {
            assert_eq!(
                items.len() as u64,
                expected_len,
                "historical L1 page ended before its recorded end"
            );
        }
    }
    Ok(HistoricalL1InputsPage {
        era_id,
        l1_stop_block: stop,
        end_input_index: end,
        next_input_index: next,
        items,
    })
}

#[cfg(test)]
mod tests;
