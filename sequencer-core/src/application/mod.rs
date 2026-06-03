// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::l2_tx::DirectInput;
use crate::l2_tx::ValidUserOp;
use crate::user_op::UserOp;
use alloy_primitives::{Address, U256};
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("internal: {reason}")]
    Internal { reason: String },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOutcome {
    // NOTE: this is a transaction that may fail execution but still be included.
    // We don't need to differentiate it now necessarily, but we can.
    Included { outputs: AppOutputs },

    Invalid(InvalidReason),
}

impl ExecutionOutcome {
    pub fn is_included(&self) -> bool {
        matches!(self, Self::Included { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppOutput {
    Notice(Vec<u8>),
    Voucher {
        destination: Address,
        value: U256,
        payload: Vec<u8>,
    },
}

pub type AppOutputs = Vec<AppOutput>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    InvalidNonce {
        expected: u32,
        got: u32,
    },
    /// Both values are log-space exponents (base 129/128).
    InvalidMaxFee {
        max_fee: u16,
        base_fee: u16,
    },
    InsufficientGasBalance {
        required: U256,
        available: U256,
    },
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

pub trait Application: Send + Sized {
    const MAX_METHOD_PAYLOAD_BYTES: usize;

    fn current_user_nonce(&self, sender: Address) -> u32;

    fn current_user_balance(&self, sender: Address) -> U256;

    fn validate_user_op(
        &self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u16,
    ) -> Result<(), InvalidReason>;

    fn execute_valid_user_op(&mut self, user_op: &ValidUserOp) -> Result<AppOutputs, AppError>;

    fn validate_and_execute_user_op(
        &mut self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u16,
    ) -> Result<ExecutionOutcome, AppError> {
        // Protocol invariant: max_fee must cover the current frame fee.
        // Enforced here so every Application impl inherits it.
        if user_op.max_fee < current_fee {
            return Ok(ExecutionOutcome::Invalid(InvalidReason::InvalidMaxFee {
                max_fee: user_op.max_fee,
                base_fee: current_fee,
            }));
        }

        if let Err(reason) = self.validate_user_op(sender, user_op, current_fee) {
            return Ok(ExecutionOutcome::Invalid(reason));
        }

        let valid = ValidUserOp {
            sender,
            fee: current_fee,
            data: user_op.data.to_vec(),
        };
        let outputs = self.execute_valid_user_op(&valid)?;
        Ok(ExecutionOutcome::Included { outputs })
    }

    fn execute_direct_input(&mut self, _input: &DirectInput) -> Result<AppOutputs, AppError> {
        Ok(Vec::new())
    }

    fn executed_input_count(&self) -> u64 {
        0
    }

    // -------- snapshot / dump lifecycle --------
    //
    // These methods are used by the inclusion lane to drive snapshot
    // lifecycle (write dumps at batch close, load from the latest dump
    // during catch-up, garbage-collect superseded dumps). Genesis
    // construction is intentionally NOT on the trait — it varies per
    // impl (CLI config for the toy wallet, machine image path for a
    // CM-wrapping app, etc.) and lives on the concrete type, called
    // by the runtime at bootstrap.

    /// Construct an instance from a dump at `prefix`. The dump must have
    /// been produced by a previous call to [`Application::create_dump`]
    /// on the same implementation; loading a dump written by a different
    /// impl is undefined.
    fn from_dump(prefix: &Path) -> Result<Self, AppError>;

    /// Write a complete recovery dump rooted at the directory `prefix`,
    /// which must not already exist. The implementation is responsible
    /// for creating `prefix` and populating it with whatever files it
    /// needs; a subsequent [`Application::from_dump`] call on the same
    /// impl must rehydrate equivalent logical state from those bytes.
    ///
    /// **Durability**: when this method returns `Ok`, the dump on disk
    /// must survive an immediate kernel crash. Concretely, the impl
    /// must `fsync` the dump's files and the directory entries that
    /// reference them (on POSIX, that means `fsync`ing the prefix
    /// directory and its parent) before returning. The sequencer
    /// inserts the SQLite row that references this path after
    /// `create_dump` returns; without the in-method fsync, the OS may
    /// flush the SQLite WAL ahead of our file contents and leave a
    /// crash-recovered DB with a row pointing at a missing path.
    ///
    /// Implementations must also ensure that
    /// [`Application::state_file_in_dump`] points at a file inside
    /// `prefix` whose bytes match what an independent canonical machine's
    /// `inspect_state` procedure would produce for the same logical
    /// state. For impls whose persistence representation already IS the
    /// canonical state, the file written by `create_dump` and the file
    /// named by `state_file_in_dump` can be the same file.
    fn create_dump(&self, prefix: &Path) -> Result<(), AppError>;

    /// Delete a previously-created dump at `prefix`.
    fn delete_dump(prefix: &Path) -> Result<(), AppError>;

    /// Path of the canonical state file within a dump at `prefix`. The
    /// returned path must point at a single file (not a directory). It
    /// is a pure function of `prefix`: callers may invoke it without
    /// loading the dump or instantiating the Application.
    fn state_file_in_dump(prefix: &Path) -> PathBuf;

    /// Deterministic canonical state bytes (SSZ for the toy wallet). Used by
    /// CM `inspect_state` and the watchdog's `/finalized_state` compare.
    /// Default: not implemented.
    fn canonical_snapshot_bytes(&self) -> Result<Vec<u8>, AppError> {
        Err(AppError::Internal {
            reason: "canonical snapshot bytes are not implemented".to_string(),
        })
    }

    /// Optional human-readable JSON for debugging only (not loaded on recovery).
    fn export_state(&self) -> Result<String, AppError> {
        Err(AppError::Internal {
            reason: "application state export is not implemented".to_string(),
        })
    }
}
