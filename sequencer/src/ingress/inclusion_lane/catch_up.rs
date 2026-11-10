// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Replay committed application history after launch, before processing new user ops.

use crate::storage::{HistoryReadError, L2TxContext, Snapshot, Storage};
use sequencer_core::application::{Application, Executed, execute};
use sequencer_core::history::{HistoryClaim, L2TxCount};
use sequencer_core::l2_tx::L2Tx;

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
    start: L2TxCount,
) -> Result<(), CatchUpError> {
    catch_up_application_paged(app, storage, start, DEFAULT_CATCH_UP_PAGE_SIZE)
}

pub(super) fn catch_up_application_paged(
    app: &mut impl Application,
    storage: &mut Storage,
    start: L2TxCount,
    page_size: usize,
) -> Result<(), CatchUpError> {
    if app.l2_tx_count() != start {
        return Err(CatchUpError::SnapshotExecutionCountMismatch {
            application: app.l2_tx_count().get(),
            storage: start.get(),
        });
    }
    tracing::debug!(
        l2_tx_count = start.get(),
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
                app.l2_tx_count(),
                row.offset,
                "application replay offset differs from stored history"
            );
            // Replay runs the executor's own checks, so a stored op the
            // restored application would skip stops the lane instead of
            // silently shifting every later offset.
            let executed = match row.context {
                L2TxContext::UserOp { tx, .. } => execute(app, &L2Tx::UserOp(tx))
                    .map_err(|source| CatchUpError::ReplayUserOp { source })?,
                L2TxContext::Direct { tx, .. } => execute(app, &L2Tx::Direct(tx))
                    .map_err(|source| CatchUpError::ReplayDirectInput { source })?,
            };
            if let Executed::Skipped(reason) = executed {
                return Err(CatchUpError::ReplaySkipped {
                    offset: row.offset.get(),
                    reason,
                });
            }
        }
    }
}
