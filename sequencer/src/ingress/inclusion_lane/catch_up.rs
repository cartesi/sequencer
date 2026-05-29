// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Startup-only replay: walk the persisted ordered-L2-tx stream and feed it
//! to the application so its in-memory state matches the DB before the lane
//! starts taking new work. Runs once, before the hot loop.

use std::path::PathBuf;

use alloy_primitives::Address;

use crate::storage::Storage;
use sequencer_core::application::Application;
use sequencer_core::l2_tx::SequencedL2Tx;

use super::error::CatchUpError;

const DEFAULT_CATCH_UP_PAGE_SIZE: usize = 256;

/// The single checkpoint the lane resumes from: the latest pending
/// snapshot if any, else the finalized snapshot. Carries both the dump
/// `prefix` (to load the Application via `from_dump`) and the matching
/// `l2_tx_index` (the replay cursor), so the loaded state and the
/// catch-up cursor are guaranteed to come from the *same* checkpoint.
#[derive(Debug, Clone)]
pub(super) struct CatchUpSnapshot {
    pub(super) prefix: PathBuf,
    pub(super) l2_tx_index: u64,
    pub(super) kind: SnapshotKind,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum SnapshotKind {
    Pending,
    Finalized,
}

/// Select the resume checkpoint. Prefers the latest pending snapshot
/// (closest to current — less replay; safe because danger-zone recovery
/// clears any cascade-doomed pending *before* the lane starts, so a
/// surviving pending is either gold or legitimately in-flight), falling
/// back to the finalized snapshot.
///
/// Returns the checkpoint directly rather than an `Option`: the
/// always-load invariant — `ensure_finalized_snapshot` runs in
/// `Workers::spawn` before `InclusionLane::start` — guarantees at least
/// the genesis finalized snapshot exists by the time the lane resumes.
/// Absence is a violated invariant (runtime/setup bug), surfaced
/// fail-loud as [`CatchUpError::NoSnapshot`].
pub(super) fn catch_up_snapshot(storage: &mut Storage) -> Result<CatchUpSnapshot, CatchUpError> {
    if let Some(pending) = storage
        .latest_pending_dump()
        .map_err(|source| CatchUpError::LoadSnapshot { source })?
    {
        return Ok(CatchUpSnapshot {
            prefix: pending.dump.prefix,
            l2_tx_index: pending.l2_tx_index,
            kind: SnapshotKind::Pending,
        });
    }
    let finalized = storage
        .finalized_dump()
        .map_err(|source| CatchUpError::LoadSnapshot { source })?
        .ok_or(CatchUpError::NoSnapshot)?;
    Ok(CatchUpSnapshot {
        prefix: finalized.dump.prefix,
        l2_tx_index: finalized.l2_tx_index,
        kind: SnapshotKind::Finalized,
    })
}

pub(super) fn catch_up_application(
    app: &mut impl Application,
    storage: &mut Storage,
    batch_submitter_address: Address,
    start_offset: u64,
) -> Result<(), CatchUpError> {
    catch_up_application_paged(
        app,
        storage,
        batch_submitter_address,
        start_offset,
        DEFAULT_CATCH_UP_PAGE_SIZE,
    )
}

pub(super) fn catch_up_application_paged(
    app: &mut impl Application,
    storage: &mut Storage,
    batch_submitter_address: Address,
    start_offset: u64,
    page_size: usize,
) -> Result<(), CatchUpError> {
    // `start_offset` is the resume checkpoint's `l2_tx_index` — the
    // global replay head captured when that snapshot's batch closed. The
    // Application was loaded from the *same* checkpoint (see
    // `catch_up_snapshot`), so replaying `offset > start_offset` applies
    // exactly the txs not yet reflected in the loaded state.
    // `ordered_l2_txs_page_from` uses `offset > ?1`.
    let mut next_offset: u64 = start_offset;
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
