// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::storage::Storage;
use sequencer_core::application::Application;
use sequencer_core::l2_tx::SequencedL2Tx;

use super::error::CatchUpError;

const DEFAULT_CATCH_UP_PAGE_SIZE: usize = 256;

pub(super) fn catch_up_application(
    app: &mut impl Application,
    storage: &mut Storage,
) -> Result<(), CatchUpError> {
    catch_up_application_paged(app, storage, DEFAULT_CATCH_UP_PAGE_SIZE)
}

pub(super) fn catch_up_application_paged(
    app: &mut impl Application,
    storage: &mut Storage,
    page_size: usize,
) -> Result<(), CatchUpError> {
    let mut next_offset = app.executed_input_count();
    let page_size = page_size.max(1);

    loop {
        let replay = storage
            .load_ordered_l2_txs_page_from(next_offset, page_size)
            .map_err(|source| CatchUpError::LoadReplay {
                offset: next_offset,
                source,
            })?;

        if replay.is_empty() {
            return Ok(());
        }

        for item in replay {
            replay_sequenced_l2_tx(app, item)?;
            next_offset = next_offset.saturating_add(1);
        }
    }
}

fn replay_sequenced_l2_tx(
    app: &mut impl Application,
    item: SequencedL2Tx,
) -> Result<(), CatchUpError> {
    match item {
        SequencedL2Tx::UserOp(value) => {
            app.execute_valid_user_op(&value)
                .map_err(|err| CatchUpError::ReplayUserOpInternal {
                    reason: err.to_string(),
                })
        }
        SequencedL2Tx::Direct(direct) => app
            .execute_direct_input(direct.payload.as_slice())
            .map_err(|err| CatchUpError::ReplayDirectInputInternal {
                reason: err.to_string(),
            }),
    }
}
