// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::history::ExecutedInputCount;
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
    /// A canonical application input executed successfully. The receipt owns
    /// its pre-execution history offset and any application outputs.
    Included(ExecutedInput),

    Invalid(InvalidReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationOutcome {
    Accept,
    Reject(InvalidReason),
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

/// Canonical progress owned and persisted by the application alongside its state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplicationProgress {
    executed_input_count: ExecutedInputCount,
    last_executed_safe_block: u64,
}

impl ApplicationProgress {
    /// Construct a coherent application-history boundary: `None` when the
    /// pair is incoherent (zero executed inputs with a nonzero clock), since
    /// a nonzero clock proves that at least one input executed. Used when
    /// restoring a dump or importing progress from a native application;
    /// a genesis instance starts from `Default`.
    pub const fn try_new(
        executed_input_count: ExecutedInputCount,
        last_executed_safe_block: u64,
    ) -> Option<Self> {
        if executed_input_count.get() == 0 && last_executed_safe_block != 0 {
            return None;
        }
        Some(Self {
            executed_input_count,
            last_executed_safe_block,
        })
    }

    pub const fn executed_input_count(self) -> ExecutedInputCount {
        self.executed_input_count
    }

    pub const fn last_executed_safe_block(self) -> u64 {
        self.last_executed_safe_block
    }

    /// Advance after one successful input, including an application-level no-op.
    /// Panics if the history count has no representable successor.
    pub fn advance(&mut self, safe_block: u64) {
        *self = self
            .checked_after_input(safe_block)
            .expect("executed input count overflow: no canonical successor");
    }

    fn checked_after_input(self, safe_block: u64) -> Option<Self> {
        Some(Self {
            executed_input_count: self.executed_input_count.checked_next()?,
            last_executed_safe_block: if self.last_executed_safe_block > safe_block {
                self.last_executed_safe_block
            } else {
                safe_block
            },
        })
    }
}

/// One successfully executed canonical application input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutedInput {
    /// The boundary before this input executed; this is its history offset.
    pub offset: ExecutedInputCount,
    pub outputs: AppOutputs,
}

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
    /// Sender cannot pay the frame fee. "Fee" (not "gas"): the current fee
    /// tracks DA usage; compute metering, if it ever exists, will be a
    /// separate concept.
    InsufficientFeeBalance {
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
            Self::InsufficientFeeBalance {
                required,
                available,
            } => {
                write!(
                    f,
                    "insufficient balance for fee: required {required}, available {available}"
                )
            }
        }
    }
}

/// Deterministic application state with exclusive ownership and thread transfer.
pub trait Application: Send + Sized {
    /// Maximum user-op method payload size, stable for this implementation.
    /// Zero permits only empty method payloads.
    fn max_method_payload_bytes() -> usize;

    /// Pure validation predicate over current app state: nonce match
    /// (user replay protection) and fee-balance coverage. Must not
    /// mutate state. [`validate_and_execute_user_op`] enforces the protocol
    /// `max_fee >= current_fee` guard before calling here. Rejection leaves
    /// the app unchanged; `AppError` is fatal and defines no successor.
    fn validate_user_op(
        &self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u16,
    ) -> Result<ValidationOutcome, AppError>;

    /// Apply a validated user op and advance progress exactly once on success,
    /// using `safe_block` for the clock. Included business failures and no-ops
    /// also advance progress. `AppError` is fatal: callers discard the instance.
    /// Execution callers use [`execute_valid_user_op`] to check the transition.
    fn apply_valid_user_op(
        &mut self,
        user_op: &ValidUserOp,
        safe_block: u64,
    ) -> Result<AppOutputs, AppError>;

    /// Apply a direct input and advance progress exactly once on success,
    /// using its L1 block number for the clock. Ignored or malformed inputs
    /// still count. Execution callers use [`execute_direct_input`] to check
    /// the transition; `AppError` requires discarding the instance.
    fn apply_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError>;

    /// Return the progress embedded in the application's logical state.
    fn progress(&self) -> ApplicationProgress;

    /// The app's safe-block clock: the maximum block carried by any input
    /// this instance has executed (frame safe blocks for user ops, L1
    /// inclusion blocks for direct inputs), or 0 if nothing executed.
    /// Recovery reads this as `A`, the safe block a checkpoint state reflects;
    /// it must survive dump round-trips.
    fn last_executed_safe_block(&self) -> u64 {
        self.progress().last_executed_safe_block()
    }

    /// Canonical application-history boundary. Starts at zero and advances by
    /// exactly one after each successful user-op or direct-input execution; an
    /// application at `X` is ready to consume history input `X`. It must
    /// survive dump round-trips.
    fn executed_input_count(&self) -> ExecutedInputCount {
        self.progress().executed_input_count()
    }

    // Genesis construction stays on the concrete type: its inputs depend on
    // the application. The host manages the resulting instance through dumps.

    /// Construct an instance from a dump at `prefix`. The dump must have
    /// been produced by a previous call to [`Application::create_dump`]
    /// on the same implementation. The loaded instance must own independent
    /// mutable state: executing it must not change the dump or another instance
    /// loaded from that dump. It must remain usable after the dump is deleted.
    fn from_dump(prefix: &Path) -> Result<Self, AppError>;

    /// Write a complete recovery dump at the absent path `prefix`, which may
    /// be a file or directory. A subsequent [`Application::from_dump`] must
    /// rehydrate equivalent logical state, including progress. Creating the
    /// dump must preserve the live instance's logical state; later execution
    /// of that instance must not change the dump. All checkpoint-owned artifacts
    /// must reside at or beneath `prefix`; discarding them uses ordinary
    /// filesystem deletion and requires no application-specific cleanup.
    /// Deletion must leave other checkpoints and restored instances usable.
    ///
    /// **Durability**: when this method returns `Ok`, the dump on disk
    /// must survive an immediate kernel crash. Concretely, the impl
    /// must `fsync` the dump's files and the directory entries that
    /// reference them, including the parent of `prefix`, before returning.
    /// The sequencer inserts the SQLite row that references this path after
    /// `create_dump` returns; without the in-method fsync, the OS may
    /// flush the SQLite WAL ahead of our file contents and leave a
    /// crash-recovered DB with a row pointing at a missing path.
    ///
    /// Implementations must also ensure that
    /// [`Application::state_file_in_dump`] points at a file in the dump
    /// whose bytes match the independent canonical application's
    /// state representation, obtained through inspection or its designated
    /// state drive, for the same logical state. The recovery dump and canonical
    /// state file may be the same file when their representations coincide.
    fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError>;

    /// Path of the canonical state file in a dump at `prefix` (possibly
    /// `prefix` itself). The returned path must point at a single file. It
    /// is a pure function of `prefix`: callers may invoke it without
    /// loading the dump or instantiating the Application.
    fn state_file_in_dump(prefix: &Path) -> PathBuf;
}

/// Canonical inspection for applications hosted by the shared Rust scheduler.
/// Applications using another canonical runtime need not implement this trait.
pub trait CanonicalState {
    /// Deterministic bytes matching the state file in a dump of this state.
    fn canonical_snapshot_bytes(&self) -> Result<Vec<u8>, AppError>;
}

/// Validate and execute a live user op: protocol guard, app validation, execution.
///
/// Live inclusion and the canonical scheduler use this boundary. Trusted
/// replay uses [`execute_valid_user_op`] with the persisted validation result.
pub fn validate_and_execute_user_op<A: Application>(
    app: &mut A,
    sender: Address,
    user_op: &UserOp,
    current_fee: u16,
    safe_block: u64,
) -> Result<ExecutionOutcome, AppError> {
    // Protocol invariant: max_fee must cover the current frame fee.
    if user_op.max_fee < current_fee {
        return Ok(ExecutionOutcome::Invalid(InvalidReason::InvalidMaxFee {
            max_fee: user_op.max_fee,
            base_fee: current_fee,
        }));
    }

    if let ValidationOutcome::Reject(reason) = app.validate_user_op(sender, user_op, current_fee)? {
        return Ok(ExecutionOutcome::Invalid(reason));
    }

    let valid = ValidUserOp {
        sender,
        fee: current_fee,
        data: user_op.data.to_vec(),
    };
    execute_valid_user_op(app, &valid, safe_block).map(ExecutionOutcome::Included)
}

/// Execute an already-validated user op and verify the application-owned progress.
/// The caller is responsible for supplying a valid op, including its fee.
pub fn execute_valid_user_op<A: Application>(
    app: &mut A,
    user_op: &ValidUserOp,
    safe_block: u64,
) -> Result<ExecutedInput, AppError> {
    execute_and_verify_progress(app, safe_block, |app| {
        app.apply_valid_user_op(user_op, safe_block)
    })
}

/// Execute one direct input and verify the application-owned progress.
pub fn execute_direct_input<A: Application>(
    app: &mut A,
    input: &DirectInput,
) -> Result<ExecutedInput, AppError> {
    execute_and_verify_progress(app, input.block_number, |app| app.apply_direct_input(input))
}

fn execute_and_verify_progress<A, F>(
    app: &mut A,
    safe_block: u64,
    apply: F,
) -> Result<ExecutedInput, AppError>
where
    A: Application,
    F: FnOnce(&mut A) -> Result<AppOutputs, AppError>,
{
    let progress_before = app.progress();
    let progress_after = progress_before
        .checked_after_input(safe_block)
        .expect("executed input count overflow: no canonical successor");

    let outputs = apply(app)?;
    assert_eq!(
        app.progress(),
        progress_after,
        "successful application execution must advance progress exactly once"
    );

    Ok(ExecutedInput {
        offset: progress_before.executed_input_count(),
        outputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ProgressApp {
        progress: ApplicationProgress,
        applied: u64,
        reject: bool,
        fail_validation: bool,
        fail_apply: bool,
        advance_count: usize,
        clock_override: Option<u64>,
    }

    impl ProgressApp {
        fn new(count: u64) -> Self {
            Self {
                progress: ApplicationProgress::try_new(ExecutedInputCount::new(count), 0)
                    .expect("coherent progress"),
                applied: 0,
                reject: false,
                fail_validation: false,
                fail_apply: false,
                advance_count: 1,
                clock_override: None,
            }
        }

        fn apply(&mut self, safe_block: u64) -> Result<AppOutputs, AppError> {
            self.applied += 1;
            if self.fail_apply {
                return Err(AppError::Internal {
                    reason: "execution failed".into(),
                });
            }
            for _ in 0..self.advance_count {
                self.progress
                    .advance(self.clock_override.unwrap_or(safe_block));
            }
            Ok(Vec::new())
        }
    }

    impl Application for ProgressApp {
        fn max_method_payload_bytes() -> usize {
            0
        }

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<ValidationOutcome, AppError> {
            if self.fail_validation {
                return Err(AppError::Internal {
                    reason: "validation failed".into(),
                });
            }
            if self.reject {
                Ok(ValidationOutcome::Reject(InvalidReason::InvalidNonce {
                    expected: 1,
                    got: 0,
                }))
            } else {
                Ok(ValidationOutcome::Accept)
            }
        }

        fn apply_valid_user_op(
            &mut self,
            _user_op: &ValidUserOp,
            safe_block: u64,
        ) -> Result<AppOutputs, AppError> {
            self.apply(safe_block)
        }

        fn apply_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
            self.apply(input.block_number)
        }

        fn progress(&self) -> ApplicationProgress {
            assert!(
                !(self.fail_apply && self.applied > 0),
                "failed instance is unusable"
            );
            self.progress
        }

        fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
            unreachable!("not used")
        }
        fn create_dump(&mut self, _prefix: &Path) -> Result<(), AppError> {
            unreachable!("not used")
        }
        fn state_file_in_dump(prefix: &Path) -> PathBuf {
            prefix.join("state")
        }
    }

    fn user_op() -> UserOp {
        UserOp {
            nonce: 0,
            max_fee: 0,
            data: Vec::new().into(),
        }
    }

    fn direct(block_number: u64) -> DirectInput {
        DirectInput {
            sender: Address::ZERO,
            block_number,
            payload: Vec::new(),
        }
    }

    #[test]
    fn native_progress_defines_receipt_offsets_and_monotonic_clock() {
        let mut app = ProgressApp::new(0);
        let ExecutionOutcome::Included(user) =
            validate_and_execute_user_op(&mut app, Address::ZERO, &user_op(), 0, 9).unwrap()
        else {
            panic!("user op should be included")
        };
        assert_eq!(user.offset, ExecutedInputCount::ZERO);
        assert_eq!(app.executed_input_count(), ExecutedInputCount::new(1));
        assert_eq!(app.last_executed_safe_block(), 9);

        for (offset, block, clock) in [(1, 12, 12), (2, 10, 12)] {
            let receipt = execute_direct_input(&mut app, &direct(block)).unwrap();
            assert_eq!(receipt.offset, ExecutedInputCount::new(offset));
            assert!(
                receipt.outputs.is_empty(),
                "included no-ops still advance progress"
            );
            assert_eq!(
                app.executed_input_count(),
                ExecutedInputCount::new(offset + 1)
            );
            assert_eq!(app.last_executed_safe_block(), clock);
        }
    }

    #[test]
    fn rejection_and_fatal_validation_do_not_execute() {
        let mut app = ProgressApp::new(7);
        app.reject = true;
        assert!(matches!(
            validate_and_execute_user_op(&mut app, Address::ZERO, &user_op(), 0, 9).unwrap(),
            ExecutionOutcome::Invalid(InvalidReason::InvalidNonce { .. })
        ));
        app.fail_validation = true;
        assert!(matches!(
            validate_and_execute_user_op(&mut app, Address::ZERO, &user_op(), 0, 9),
            Err(AppError::Internal { reason }) if reason == "validation failed"
        ));
        assert_eq!(app.applied, 0);
        assert_eq!(app.executed_input_count(), ExecutedInputCount::new(7));
    }

    #[test]
    fn protocol_max_fee_guard_precedes_application_validation() {
        let mut app = ProgressApp::new(0);
        app.fail_validation = true;
        assert!(matches!(
            validate_and_execute_user_op(&mut app, Address::ZERO, &user_op(), 1, 9).unwrap(),
            ExecutionOutcome::Invalid(InvalidReason::InvalidMaxFee {
                max_fee: 0,
                base_fee: 1
            })
        ));
        assert_eq!(app.applied, 0);
    }

    #[test]
    fn execution_error_is_propagated_without_reading_the_failed_instance() {
        let mut app = ProgressApp::new(7);
        app.fail_apply = true;
        assert!(matches!(
            execute_direct_input(&mut app, &direct(12)),
            Err(AppError::Internal { reason }) if reason == "execution failed"
        ));
        assert_eq!(app.applied, 1);
    }

    #[test]
    fn count_exhaustion_fails_before_application_mutation() {
        let mut app = ProgressApp::new(u64::MAX);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = execute_direct_input(&mut app, &direct(1));
        }));
        assert!(panic.is_err());
        assert_eq!(app.applied, 0, "overflow must preflight the app hook");
        assert_eq!(
            app.executed_input_count(),
            ExecutedInputCount::new(u64::MAX)
        );
    }

    #[test]
    fn successful_hooks_must_report_the_exact_successor() {
        for (advances, clock_override) in [(0, None), (2, None), (1, Some(99))] {
            let mut app = ProgressApp::new(1);
            app.advance_count = advances;
            app.clock_override = clock_override;
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = execute_direct_input(&mut app, &direct(12));
            }));
            assert!(panic.is_err(), "wrong application progress must fail loud");
        }
    }

    #[test]
    fn zero_count_rejects_nonzero_safe_block_clock() {
        assert!(ApplicationProgress::try_new(ExecutedInputCount::ZERO, 1).is_none());
        assert!(ApplicationProgress::try_new(ExecutedInputCount::ZERO, 0).is_some());
        assert!(ApplicationProgress::try_new(ExecutedInputCount::new(1), 7).is_some());
    }
}
