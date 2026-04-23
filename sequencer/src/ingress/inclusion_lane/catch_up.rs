// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Startup-only replay: walk the persisted ordered-L2-tx stream and feed it
//! to the application so its in-memory state matches the DB before the lane
//! starts taking new work. Runs once, before the hot loop.

use alloy_primitives::Address;

use crate::storage::Storage;
use sequencer_core::application::Application;
use sequencer_core::l2_tx::SequencedL2Tx;

use super::error::CatchUpError;

const DEFAULT_CATCH_UP_PAGE_SIZE: usize = 256;

pub(super) fn catch_up_application(
    app: &mut impl Application,
    storage: &mut Storage,
    batch_submitter_address: Address,
) -> Result<(), CatchUpError> {
    catch_up_application_paged(
        app,
        storage,
        batch_submitter_address,
        DEFAULT_CATCH_UP_PAGE_SIZE,
    )
}

pub(super) fn catch_up_application_paged(
    app: &mut impl Application,
    storage: &mut Storage,
    batch_submitter_address: Address,
    page_size: usize,
) -> Result<(), CatchUpError> {
    // Cursor tracks the DB offset of the last processed item.
    // SQLite rowids start at 1, so 0 means "before all items".
    let mut next_offset: u64 = 0;
    let page_size = page_size.max(1);

    loop {
        let replay = storage
            .ordered_l2_txs_page_from(next_offset, page_size)
            .map_err(|source| CatchUpError::LoadReplay {
                offset: next_offset,
                source,
            })?;

        if replay.is_empty() {
            return Ok(());
        }

        for (db_offset, item) in replay {
            replay_sequenced_l2_tx(app, batch_submitter_address, item)?;
            next_offset = db_offset;
        }
    }
}

fn replay_sequenced_l2_tx(
    app: &mut impl Application,
    batch_submitter_address: Address,
    item: SequencedL2Tx,
) -> Result<(), CatchUpError> {
    match item {
        SequencedL2Tx::UserOp(value) => {
            app.execute_valid_user_op(&value)
                .map(|_| ())
                .map_err(|err| CatchUpError::ReplayUserOpInternal {
                    reason: err.to_string(),
                })
        }
        SequencedL2Tx::Direct(direct) => {
            if direct.sender == batch_submitter_address {
                return Ok(());
            }

            app.execute_direct_input(&direct)
                .map(|_| ())
                .map_err(|err| CatchUpError::ReplayDirectInputInternal {
                    reason: err.to_string(),
                })
        }
    }
}
