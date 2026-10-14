// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Replay committed application history after launch, before processing new user ops.

use crate::storage::{HistoryReadError, L2TxContext, Snapshot, Storage};
use sequencer_core::application::{Application, execute_direct_input, execute_valid_user_op};
use sequencer_core::history::{ExecutedInputCount, HistoryClaim};

use super::error::CatchUpError;

const DEFAULT_CATCH_UP_PAGE_SIZE: usize = 256;

pub(super) fn catch_up_snapshot(storage: &mut Storage) -> Result<Snapshot, CatchUpError> {
    storage
        .latest_snapshot()
        .map_err(|source| CatchUpError::LoadSnapshot { source })?
        .ok_or(CatchUpError::NoSnapshot)
}

pub(super) fn catch_up_application(
    app: &mut impl Application,
    storage: &mut Storage,
    start: ExecutedInputCount,
) -> Result<(), CatchUpError> {
    catch_up_application_paged(app, storage, start, DEFAULT_CATCH_UP_PAGE_SIZE)
}

pub(super) fn catch_up_application_paged(
    app: &mut impl Application,
    storage: &mut Storage,
    start: ExecutedInputCount,
    page_size: usize,
) -> Result<(), CatchUpError> {
    if app.executed_input_count() != start {
        return Err(CatchUpError::SnapshotExecutionCountMismatch {
            application: app.executed_input_count().get(),
            storage: start.get(),
        });
    }
    tracing::debug!(
        executed_input_count = start.get(),
        "inclusion lane resuming from snapshot"
    );
    let bounds = storage
        .history_bounds()
        .map_err(|source| CatchUpError::LoadReplay {
            offset: start.get(),
            source,
        })?;
    let mut claim = HistoryClaim {
        version: bounds.version,
        next_input: start,
    };
    loop {
        let page = storage
            .canonical_history_page(claim, page_size.max(1))
            .map_err(|error| match error {
                HistoryReadError::Storage(source) => CatchUpError::LoadReplay {
                    offset: claim.next_input.get(),
                    source,
                },
                HistoryReadError::Policy(source) => CatchUpError::History(source),
            })?;
        if page.rows.is_empty() {
            return Ok(());
        }
        claim = page.next_claim();
        for row in page.rows {
            assert_eq!(
                app.executed_input_count(),
                row.offset,
                "application replay offset differs from stored history"
            );
            match row.context {
                L2TxContext::UserOp { tx, safe_block, .. } => {
                    execute_valid_user_op(app, &tx, safe_block)
                        .map_err(|source| CatchUpError::ReplayUserOp { source })?;
                }
                L2TxContext::DirectInput { tx, .. } => {
                    execute_direct_input(app, &tx)
                        .map_err(|source| CatchUpError::ReplayDirectInput { source })?;
                }
            }
        }
    }
}
