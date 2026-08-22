// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared `#[cfg(test)]` fixtures for the runtime modules: a minimal
//! [`Application`] stub plus a dump-layout helper, used by both the worker
//! lifecycle tests ([`super::workers`]) and the setup-fill tests
//! ([`super::setup_fill`]).

use std::path::Path;

use crate::ingress::inclusion_lane::dump_info::{self, create_dump_dir_with_info};
use sequencer_core::application::{
    AppError, AppOutputs, Application, ApplicationProgress, ApplyInputCapability, InvalidReason,
    ProgressCommitCapability,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::ValidUserOp;
use sequencer_core::user_op::UserOp;

/// Application stub used in the sweep tests: `create_dump` makes
/// a directory with a marker file inside, `delete_dump` is
/// `remove_dir_all`. The actual marker content is irrelevant —
/// we only care about which directories exist post-sweep.
#[derive(Clone, Default)]
pub(crate) struct SweepTestApp {
    progress: ApplicationProgress,
}

// Preserve the unit-struct-like fixture spelling used across runtime tests
// while carrying the scheduler-owned progress required by `Application`.
#[allow(non_upper_case_globals)]
pub(crate) const SweepTestApp: SweepTestApp = SweepTestApp {
    progress: ApplicationProgress::new(ExecutedInputCount::ZERO, 0),
};

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
    fn apply_valid_user_op(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _user_op: &ValidUserOp,
        _safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        Ok(Vec::new())
    }
    fn apply_direct_input(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        _input: &sequencer_core::l2_tx::DirectInput,
    ) -> Result<AppOutputs, AppError> {
        unimplemented!("not used in these tests")
    }
    fn execution_progress(&self) -> &ApplicationProgress {
        &self.progress
    }
    fn execution_progress_mut(
        &mut self,
        _capability: ProgressCommitCapability<'_>,
    ) -> &mut ApplicationProgress {
        &mut self.progress
    }

    fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
        Ok(Self::default())
    }
    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        std::fs::create_dir(prefix)?;
        std::fs::write(prefix.join("state"), b"")?;
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
