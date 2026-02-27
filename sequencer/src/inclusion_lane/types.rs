// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::SystemTime;

use thiserror::Error;
use tokio::sync::oneshot;

use crate::user_op::SignedUserOp;

#[derive(Debug)]
pub struct PendingUserOp {
    pub signed: SignedUserOp,
    pub respond_to: oneshot::Sender<Result<(), SequencerError>>,
    pub received_at: SystemTime,
}

#[derive(Debug)]
pub enum InclusionLaneInput {
    UserOp(PendingUserOp),
}

#[derive(Debug, Error, Clone)]
pub enum SequencerError {
    #[error("{0}")]
    Invalid(String),
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
}
