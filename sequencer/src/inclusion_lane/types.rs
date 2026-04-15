// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Cross-module types: the unit of work the API hands the lane, and the
//! per-op outcome the lane sends back through the response channel.

use std::time::SystemTime;

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

/// Per-op outcome reported back to the API caller via the response channel.
///
/// - `Invalid` — application rejected the op (nonce mismatch, fee too low, etc.); maps to HTTP 4xx.
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
