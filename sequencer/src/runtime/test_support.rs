// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared `#[cfg(test)]` fixtures for the runtime modules: a minimal
//! [`Application`] stub plus a dump-layout helper, used by both the worker
//! lifecycle tests ([`super::workers`]) and the setup-fill tests
//! ([`super::setup_fill`]).

use std::path::Path;

use crate::ingress::inclusion_lane::dump_info::{self, create_dump_dir_with_info};
use sequencer_core::application::{AppError, AppOutputs, Application, InvalidReason};
use sequencer_core::l2_tx::ValidUserOp;
use sequencer_core::user_op::UserOp;

/// Application stub used in runtime tests. Its dump preserves the execution
/// counters so it still satisfies the application contract while sweep tests
/// exercise directory lifecycle.
#[derive(Default)]
pub(crate) struct SweepTestApp {
    executed_input_count: u64,
    last_executed_safe_block: u64,
}

impl SweepTestApp {
    pub(crate) fn with_executed_input_count(executed_input_count: u64) -> Self {
        Self {
            executed_input_count,
            last_executed_safe_block: 0,
        }
    }
}

impl Application for SweepTestApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = 0;
    fn validate_user_op(
        &self,
        _sender: alloy_primitives::Address,
        _user_op: &UserOp,
        _current_fee: u16,
    ) -> Result<(), InvalidReason> {
        Ok(())
    }
    fn execute_valid_user_op(
        &mut self,
        _user_op: &ValidUserOp,
        safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        self.executed_input_count =
            self.executed_input_count
                .checked_add(1)
                .ok_or_else(|| AppError::Internal {
                    reason: "test app executed-input count overflow".to_string(),
                })?;
        self.last_executed_safe_block = self.last_executed_safe_block.max(safe_block);
        Ok(Vec::new())
    }
    fn execute_direct_input(
        &mut self,
        input: &sequencer_core::l2_tx::DirectInput,
    ) -> Result<AppOutputs, AppError> {
        self.executed_input_count =
            self.executed_input_count
                .checked_add(1)
                .ok_or_else(|| AppError::Internal {
                    reason: "test app executed-input count overflow".to_string(),
                })?;
        self.last_executed_safe_block = self.last_executed_safe_block.max(input.block_number);
        Ok(Vec::new())
    }
    fn executed_input_count(&self) -> u64 {
        self.executed_input_count
    }
    fn last_executed_safe_block(&self) -> u64 {
        self.last_executed_safe_block
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let bytes = std::fs::read(prefix.join("state"))?;
        let bytes: [u8; 16] = bytes.try_into().map_err(|_| AppError::Internal {
            reason: "invalid test app dump".to_string(),
        })?;
        Ok(Self {
            executed_input_count: u64::from_le_bytes(bytes[..8].try_into().expect("8-byte slice")),
            last_executed_safe_block: u64::from_le_bytes(
                bytes[8..].try_into().expect("8-byte slice"),
            ),
        })
    }
    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&self.executed_input_count.to_le_bytes());
        bytes.extend_from_slice(&self.last_executed_safe_block.to_le_bytes());
        std::fs::write(prefix.join("state"), bytes)?;
        Ok(())
    }
    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        std::fs::remove_dir_all(prefix)?;
        Ok(())
    }
    fn state_file_in_dump(prefix: &Path) -> std::path::PathBuf {
        prefix.join("state")
    }
}

/// Mirror of the production dump layout for these tests: structured
/// dir with `info.toml` + the stub app's dump under `state`.
pub(crate) fn create_structured_dump(dump_dir: &std::path::Path) {
    create_dump_dir_with_info(
        &SweepTestApp::default(),
        dump_dir,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce: 0,
            l2_tx_index: 0,
            promoted_inclusion_block: None,
        },
    )
    .expect("create structured dump");
}
