// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_primitives::Address;
use app_core::application::{MAX_METHOD_PAYLOAD_BYTES, WALLET_SCHEDULER_LIMITS};
use sequencer_core::application::{
    Admission, AppError, AppOutputs, Application, PolicyRefusal, SchedulerLimits,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{Direct, UserOpTx};
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
        input_index: u64,
        sender: Address,
        block_number: u64,
        timestamp: u64,
        payload: Vec<u8>,
    },
}

/// Executes every transaction it is handed. Dumps persist the count and the
/// per-sender nonces at `prefix/state`; `from_dump` resets every other field,
/// so a test observes what the lane executed through the dumps it writes.
#[derive(Default)]
pub(crate) struct StubApp {
    pub(crate) count: ExecutedInputCount,
    /// Each sender's next nonce; an absent sender is at zero.
    pub(crate) nonces: BTreeMap<Address, u32>,
    /// Transactions this instance executed; not persisted.
    pub(crate) replayed: Vec<ReplayEvent>,
    /// Test-only scheduling seam that keeps a refused queue saturated long
    /// enough to distinguish one bounded turn from an unbounded drain: each
    /// nonce read sleeps, then reports one past the sender's next nonce, so
    /// every live op refuses with a wrong nonce (and a stored op would skip).
    pub(crate) reject_user_ops_after: Option<Duration>,
    /// `admit_user_op` refuses every op with this policy text.
    pub(crate) refuse_user_ops: Option<String>,
    /// `admit_user_op` fails with an internal error.
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
    pub(crate) fn with_progress(executed_input_count: u64) -> Self {
        Self {
            count: ExecutedInputCount::new(executed_input_count),
            ..Self::default()
        }
    }

    fn next_nonce(&self, sender: Address) -> u32 {
        self.nonces.get(&sender).copied().unwrap_or(0)
    }
}

fn internal_failure() -> AppError {
    AppError::Internal {
        reason: "app invariant failed".to_owned(),
    }
}

impl Application for StubApp {
    fn user_nonce(&self, sender: Address) -> Result<u32, AppError> {
        let next = self.next_nonce(sender);
        if let Some(delay) = self.reject_user_ops_after {
            std::thread::sleep(delay);
            return Ok(next.wrapping_add(1));
        }
        Ok(next)
    }

    fn apply_user_op(&mut self, op: &UserOpTx) -> Result<AppOutputs, AppError> {
        if self.fail_apply {
            return Err(internal_failure());
        }
        self.replayed.push(ReplayEvent::UserOp {
            sender: op.sender,
            data: op.data.clone(),
        });
        let next = self
            .next_nonce(op.sender)
            .checked_add(1)
            .expect("the executor never applies an op at nonce u32::MAX");
        self.nonces.insert(op.sender, next);
        self.count.advance();
        Ok(Vec::new())
    }

    fn apply_direct(&mut self, direct: &Direct) -> Result<AppOutputs, AppError> {
        if let Some(shutdown) = &self.shutdown_on_direct {
            shutdown.request_shutdown();
        }
        self.replayed.push(ReplayEvent::DirectInput {
            input_index: direct.input_index,
            sender: direct.sender,
            block_number: direct.block,
            timestamp: direct.timestamp,
            payload: direct.payload.clone(),
        });
        self.count.advance();
        Ok(Vec::new())
    }

    fn executed_input_count(&self) -> ExecutedInputCount {
        self.count
    }

    fn max_method_payload_bytes() -> usize {
        MAX_METHOD_PAYLOAD_BYTES
    }

    /// The wallet's limits, so storage fixtures that open frames with the
    /// stub's cap select the same directs a `WalletApp` lane would.
    fn scheduler_limits() -> SchedulerLimits {
        WALLET_SCHEDULER_LIMITS
    }

    fn admit_user_op(
        &self,
        _sender: Address,
        _op: &UserOp,
        _fee: u16,
    ) -> Result<Admission, AppError> {
        if self.fail_validation {
            return Err(internal_failure());
        }
        if let Some(reason) = &self.refuse_user_ops {
            return Ok(Admission::Refuse(PolicyRefusal::Other(reason.clone())));
        }
        Ok(Admission::Admit)
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let bytes = std::fs::read(Self::state_file_in_dump(prefix))?;
        decode_state(&bytes)
    }

    fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError> {
        if self.fail_dump {
            return Err(std::io::Error::other("injected application dump failure").into());
        }
        std::fs::create_dir(prefix)?;
        std::fs::write(Self::state_file_in_dump(prefix), encode_state(self))?;
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
    }

    fn from_canonical_state(
        state_file: &Path,
        executed_input_count: ExecutedInputCount,
    ) -> Result<Self, AppError> {
        let app = decode_state(&std::fs::read(state_file)?)?;
        if app.count != executed_input_count {
            return Err(AppError::Internal {
                reason: format!(
                    "StubApp state holds count {}, but the machine reports {}",
                    app.count.get(),
                    executed_input_count.get()
                ),
            });
        }
        Ok(app)
    }
}

const NONCE_ENTRY_BYTES: usize = 20 + 4;

/// The count as a LE `u64`, then each sender with a nonce as its 20 address
/// bytes and a LE `u32`, in address order.
fn encode_state(app: &StubApp) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + app.nonces.len() * NONCE_ENTRY_BYTES);
    bytes.extend_from_slice(&app.count.get().to_le_bytes());
    for (sender, nonce) in &app.nonces {
        bytes.extend_from_slice(sender.as_slice());
        bytes.extend_from_slice(&nonce.to_le_bytes());
    }
    bytes
}

fn decode_state(bytes: &[u8]) -> Result<StubApp, AppError> {
    let malformed = || AppError::Internal {
        reason: "StubApp dump must be a count and whole nonce entries".to_owned(),
    };
    let (count, entries) = bytes.split_first_chunk::<8>().ok_or_else(malformed)?;
    let (entries, rest) = entries.as_chunks::<NONCE_ENTRY_BYTES>();
    if !rest.is_empty() {
        return Err(malformed());
    }
    let nonces = entries
        .iter()
        .map(|entry| {
            let (sender, nonce) = entry.split_at(20);
            let nonce: [u8; 4] = nonce.try_into().expect("checked entry length");
            (Address::from_slice(sender), u32::from_le_bytes(nonce))
        })
        .collect();
    Ok(StubApp {
        count: ExecutedInputCount::new(u64::from_le_bytes(*count)),
        nonces,
        ..StubApp::default()
    })
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
