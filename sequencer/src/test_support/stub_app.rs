// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_primitives::Address;
use app_core::application::MAX_METHOD_PAYLOAD_BYTES;
use sequencer_core::application::{
    AppError, AppOutputs, Application, ApplicationProgress, InvalidReason, ValidationOutcome,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
use sequencer_core::user_op::UserOp;

use crate::ingress::inclusion_lane::dump_info::{self, create_dump_dir_with_info};
use crate::runtime::shutdown::RuntimeScope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplayEvent {
    UserOp {
        sender: Address,
        data: Vec<u8>,
    },
    DirectInput {
        sender: Address,
        block_number: u64,
        payload: Vec<u8>,
    },
}

/// Accepts and executes every input. Dumps persist only progress (two LE
/// `u64`s at `prefix/state`); `from_dump` resets every other field, so a test
/// observes what the lane executed through the dumps it writes.
#[derive(Default)]
pub(crate) struct StubApp {
    pub(crate) progress: ApplicationProgress,
    /// Inputs this instance executed; not persisted.
    pub(crate) replayed: Vec<ReplayEvent>,
    /// Test-only scheduling seam used to keep a rejected queue saturated long
    /// enough to distinguish one bounded turn from an unbounded drain.
    pub(crate) reject_user_ops_after: Option<Duration>,
    pub(crate) fail_validation: bool,
    pub(crate) fail_apply: bool,
    pub(crate) fail_dump: bool,
    /// Requests shutdown while the lane executes a direct input, so the
    /// request lands after the loop's shutdown check and before the queue is
    /// read.
    pub(crate) shutdown_on_direct: Option<RuntimeScope>,
    /// Not `Sync`, and no `Clone` derive: hosts must not require either.
    pub(crate) not_sync: PhantomData<std::cell::Cell<()>>,
}

impl StubApp {
    pub(crate) fn with_progress(executed_input_count: u64, last_executed_safe_block: u64) -> Self {
        Self {
            progress: ApplicationProgress::try_new(
                ExecutedInputCount::new(executed_input_count),
                last_executed_safe_block,
            )
            .expect("coherent progress"),
            ..Self::default()
        }
    }
}

fn internal_failure() -> AppError {
    AppError::Internal {
        reason: "app invariant failed".to_owned(),
    }
}

impl Application for StubApp {
    fn max_method_payload_bytes() -> usize {
        MAX_METHOD_PAYLOAD_BYTES
    }

    fn validate_user_op(
        &self,
        _sender: Address,
        user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<ValidationOutcome, AppError> {
        if self.fail_validation {
            return Err(internal_failure());
        }
        if let Some(delay) = self.reject_user_ops_after {
            std::thread::sleep(delay);
            return Ok(ValidationOutcome::Reject(InvalidReason::InvalidNonce {
                expected: user_op.nonce.wrapping_add(1),
                got: user_op.nonce,
            }));
        }
        Ok(ValidationOutcome::Accept)
    }

    fn apply_valid_user_op(
        &mut self,
        user_op: &ValidUserOp,
        safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        if self.fail_apply {
            return Err(internal_failure());
        }
        self.replayed.push(ReplayEvent::UserOp {
            sender: user_op.sender,
            data: user_op.data.clone(),
        });
        self.progress.advance(safe_block);
        Ok(Vec::new())
    }

    fn apply_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
        if let Some(shutdown) = &self.shutdown_on_direct {
            shutdown.request_shutdown();
        }
        self.replayed.push(ReplayEvent::DirectInput {
            sender: input.sender,
            block_number: input.block_number,
            payload: input.payload.clone(),
        });
        self.progress.advance(input.block_number);
        Ok(Vec::new())
    }

    fn progress(&self) -> ApplicationProgress {
        self.progress
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let bytes = std::fs::read(Self::state_file_in_dump(prefix))?;
        Ok(Self {
            progress: decode_progress(&bytes)?,
            ..Self::default()
        })
    }

    fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError> {
        if self.fail_dump {
            return Err(std::io::Error::other("injected application dump failure").into());
        }
        std::fs::create_dir(prefix)?;
        std::fs::write(
            Self::state_file_in_dump(prefix),
            encode_progress(self.progress),
        )?;
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
    }
}

fn encode_progress(progress: ApplicationProgress) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&progress.executed_input_count().get().to_le_bytes());
    bytes[8..].copy_from_slice(&progress.last_executed_safe_block().to_le_bytes());
    bytes
}

fn decode_progress(bytes: &[u8]) -> Result<ApplicationProgress, AppError> {
    if bytes.len() != 16 {
        return Err(AppError::Internal {
            reason: "StubApp dump must be exactly 16 bytes".to_owned(),
        });
    }
    let count = u64::from_le_bytes(bytes[..8].try_into().expect("checked slice length"));
    let safe_block = u64::from_le_bytes(bytes[8..].try_into().expect("checked slice length"));
    Ok(
        ApplicationProgress::try_new(ExecutedInputCount::new(count), safe_block)
            .expect("coherent progress"),
    )
}

/// Mirror of the production dump layout: a structured directory with
/// `info.toml` and the stub's dump under `state`.
pub(crate) fn create_structured_dump(dump_dir: &Path) {
    create_dump_dir_with_info(
        &mut StubApp::default(),
        dump_dir,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce: 0,
        },
    )
    .expect("create structured dump");
}
