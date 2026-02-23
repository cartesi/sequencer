// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod method;
mod wallet;

use crate::l2_tx::ValidUserOp;
use crate::user_op::UserOp;
use alloy_primitives::{Address, U256};
use std::fmt;
use thiserror::Error;

pub use method::{Deposit, Method, Transfer, Withdrawal};
pub use wallet::{WalletApp, WalletConfig};

#[derive(Debug, Error)]
pub enum AppError {
    #[error("internal: {reason}")]
    Internal { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionOutcome {
    // NOTE: this is a transaction that may fail execution but still be included.
    // We don't need to differentiate it now necessarily, but we can.
    Included,

    Invalid(InvalidReason),
}

impl ExecutionOutcome {
    pub fn is_included(&self) -> bool {
        matches!(self, Self::Included)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    InvalidNonce { expected: u32, got: u32 },
    InvalidMaxFee { max_fee: u32, base_fee: u64 },
    InsufficientGasBalance { required: U256, available: U256 },
}

impl fmt::Display for InvalidReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNonce { expected, got } => {
                write!(f, "bad nonce: expected {expected}, got {got}")
            }
            Self::InvalidMaxFee { max_fee, base_fee } => {
                write!(f, "max fee {max_fee} below base fee {base_fee}")
            }
            Self::InsufficientGasBalance {
                required,
                available,
            } => {
                write!(
                    f,
                    "insufficient balance for gas: required {required}, available {available}"
                )
            }
        }
    }
}

pub trait Application: Send {
    fn current_user_nonce(&self, sender: Address) -> u32;

    fn current_user_balance(&self, sender: Address) -> U256;

    fn validate_user_op(
        &self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u64,
    ) -> Result<(), InvalidReason>;

    fn execute_valid_user_op(&mut self, user_op: &ValidUserOp) -> Result<(), AppError>;

    fn validate_and_execute_user_op(
        &mut self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u64,
    ) -> Result<ExecutionOutcome, AppError> {
        if let Err(reason) = self.validate_user_op(sender, user_op, current_fee) {
            return Ok(ExecutionOutcome::Invalid(reason));
        }

        let valid = ValidUserOp {
            sender,
            fee: current_fee,
            data: user_op.data.to_vec(),
        };
        self.execute_valid_user_op(&valid)?;
        Ok(ExecutionOutcome::Included)
    }

    fn execute_direct_input(&mut self, _payload: &[u8]) -> Result<(), AppError> {
        Ok(())
    }

    fn executed_input_count(&self) -> u64 {
        0
    }
}
