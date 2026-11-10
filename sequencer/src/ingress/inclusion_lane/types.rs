// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Cross-module types: the unit of work the API hands the lane, and the
//! per-op outcome the lane sends back through the response channel.

use std::time::SystemTime;

use crate::storage::ExecutedUserOp;
use sequencer_core::history::L2TxCount;
use sequencer_core::user_op::SignedUserOp;
use thiserror::Error;
use tokio::sync::oneshot;

/// A signed user op accepted by the API and queued for the inclusion lane.
/// The lane sends the inclusion outcome back through `respond_to`.
#[derive(Debug)]
pub struct PendingUserOp {
    pub signed: SignedUserOp,
    pub respond_to: oneshot::Sender<Result<(), SequencerError>>,
    pub received_at: SystemTime,
}

/// A user op whose application mutation has succeeded but whose SQLite chunk
/// transaction has not committed yet. Keeping the receipt offset attached to
/// the request prevents the lane from persisting an included op without its
/// canonical application-history coordinate.
#[derive(Debug)]
pub(crate) struct IncludedUserOp {
    pub pending: PendingUserOp,
    pub l2_tx_index: L2TxCount,
}

impl IncludedUserOp {
    /// The borrowed view storage persists.
    pub(crate) fn executed(&self) -> ExecutedUserOp<'_> {
        ExecutedUserOp {
            signed: &self.pending.signed,
            received_at: self.pending.received_at,
            l2_tx_index: self.l2_tx_index,
        }
    }
}

#[cfg(test)]
impl crate::storage::Storage {
    /// Test shorthand: append a chunk of lane-included ops.
    pub(crate) fn append_included_user_ops(
        &mut self,
        head: &mut crate::storage::WriteHead,
        user_ops: &[IncludedUserOp],
    ) -> rusqlite::Result<()> {
        self.append_executed_user_ops_chunk(head, user_ops.iter().map(IncludedUserOp::executed))
    }
}

/// Per-op outcome reported back to the API caller via the response channel.
///
/// - `Invalid` — the lane refused the op: its fee cap is below the frame's price,
///   the executor would skip it (wrong or exhausted nonce), or the application's
///   admission refused it; the text is the refusal's own; maps to HTTP 422.
/// - `Unavailable` — sequencer can't currently accept (shutting down, queue full); maps to HTTP 503/429.
/// - `Internal` — bug or unrecoverable failure; maps to HTTP 500.
#[derive(Debug, Error, Clone)]
pub enum SequencerError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

impl SequencerError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }
}
