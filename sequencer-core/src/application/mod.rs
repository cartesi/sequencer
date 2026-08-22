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

/// Scheduler-owned progress embedded in the application's durable state.
///
/// Application hooks own only application-specific mutation. The shared
/// execution functions below advance this value after a hook succeeds, so the
/// history coordinate and safe-block clock are not hand-maintained by every
/// application implementation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplicationProgress {
    executed_input_count: ExecutedInputCount,
    last_executed_safe_block: u64,
}

impl ApplicationProgress {
    /// Construct a coherent application-history boundary.
    ///
    /// A nonzero clock proves that at least one input executed, so it cannot
    /// accompany the zero history boundary.
    ///
    /// # Panics
    ///
    /// Panics when `executed_input_count` is zero and
    /// `last_executed_safe_block` is nonzero. Use [`Self::try_new`] on
    /// deserialization paths, where the pair comes from untrusted bytes and
    /// the caller owes a typed error, not a panic (D10).
    pub const fn new(
        executed_input_count: ExecutedInputCount,
        last_executed_safe_block: u64,
    ) -> Self {
        match Self::try_new(executed_input_count, last_executed_safe_block) {
            Some(progress) => progress,
            None => panic!("zero executed inputs require a zero safe-block clock"),
        }
    }

    /// Fallible sibling of [`Self::new`] for decode paths: `None` when the
    /// pair is incoherent (zero executed inputs with a nonzero clock).
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

struct CapabilitySeal;

/// Opaque, call-scoped authority to invoke an application's raw mutation hook.
///
/// Only the shared execution functions in this module can construct this
/// capability. Its borrowed private seal prevents application implementations
/// from safely forging or retaining it beyond the hook call.
pub struct ApplyInputCapability<'a> {
    _seal: &'a CapabilitySeal,
}

/// Opaque, call-scoped authority to commit scheduler-owned application progress.
///
/// This is deliberately distinct from [`ApplyInputCapability`]: application
/// hooks receive authority to mutate application state, never authority to
/// overwrite the canonical history count or safe-block clock.
pub struct ProgressCommitCapability<'a> {
    _seal: &'a CapabilitySeal,
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

pub trait Application: Send + Sized {
    const MAX_METHOD_PAYLOAD_BYTES: usize;

    /// Pure validation predicate over current app state: nonce match
    /// (user replay protection) and fee-balance coverage. Must not
    /// mutate state. The protocol-level `max_fee >= current_fee` guard
    /// is NOT this method's job — [`validate_and_execute_user_op`]
    /// enforces it before calling here.
    fn validate_user_op(
        &self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u16,
    ) -> Result<(), InvalidReason>;

    /// Apply a validated user op's application-specific mutation.
    ///
    /// Callers use [`execute_valid_user_op`], never this hook directly. The
    /// shared function advances [`ApplicationProgress`] only after this hook
    /// returns `Ok`. The opaque capability makes that boundary structural for
    /// safe Rust callers.
    fn apply_valid_user_op(
        &mut self,
        capability: ApplyInputCapability<'_>,
        user_op: &ValidUserOp,
        safe_block: u64,
    ) -> Result<AppOutputs, AppError>;

    /// Required (no default): deposits are direct-input-only, so a silent
    /// no-op impl would strand every deposit on L1 with no L2 credit.
    /// Callers use [`execute_direct_input`], never this hook directly. The
    /// shared function advances [`ApplicationProgress`] only after this hook
    /// returns `Ok`. The opaque capability makes that boundary structural for
    /// safe Rust callers.
    fn apply_direct_input(
        &mut self,
        capability: ApplyInputCapability<'_>,
        input: &DirectInput,
    ) -> Result<AppOutputs, AppError>;

    /// Scheduler-owned progress embedded in, and persisted with, application
    /// state. Application hooks must not mutate it.
    fn execution_progress(&self) -> &ApplicationProgress;

    /// Mutable access exists only for the shared execution boundary. Its
    /// distinct opaque capability is never passed to application hooks, and
    /// the progress type itself exposes no mutating operation.
    fn execution_progress_mut(
        &mut self,
        capability: ProgressCommitCapability<'_>,
    ) -> &mut ApplicationProgress;

    /// The app's safe-block clock: the maximum block carried by any input
    /// this instance has executed (frame safe blocks for user ops, L1
    /// inclusion blocks for direct inputs), or 0 if nothing executed.
    /// Carried in [`ApplicationProgress`] so it advances at the same shared
    /// boundary as the history count. Recovery reads this as `A`, the safe
    /// block a checkpoint state reflects; it must survive dump round-trips.
    fn last_executed_safe_block(&self) -> u64 {
        self.execution_progress().last_executed_safe_block()
    }

    /// Canonical application-history boundary. Starts at zero and advances by
    /// exactly one after each successful user-op or direct-input execution; an
    /// application at `X` is ready to consume history input `X`. It must
    /// survive dump round-trips. The planned Track 3 feed uses this value as
    /// its subscription offset; the current rowid feed has not cut over yet.
    fn executed_input_count(&self) -> ExecutedInputCount {
        self.execution_progress().executed_input_count()
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

/// The single entry point for executing a user op against an app: protocol
/// guard, then app validation, then execution.
///
/// Deliberately a free function, not a trait method: an overridable default
/// would let an `Application` impl skip the protocol-level
/// `max_fee >= current_fee` invariant. As a free function the guard is
/// non-bypassable by construction. Both consumers — the inclusion lane and
/// the canonical scheduler — must execute user ops through here; agreement
/// between them is the system's most load-bearing invariant.
pub fn validate_and_execute_user_op<A: Application>(
    app: &mut A,
    sender: Address,
    user_op: &UserOp,
    current_fee: u16,
    safe_block: u64,
) -> Result<ExecutionOutcome, AppError> {
    let progress_before_validation = *app.execution_progress();

    // Protocol invariant: max_fee must cover the current frame fee.
    if user_op.max_fee < current_fee {
        return Ok(ExecutionOutcome::Invalid(InvalidReason::InvalidMaxFee {
            max_fee: user_op.max_fee,
            base_fee: current_fee,
        }));
    }

    let validation = app.validate_user_op(sender, user_op, current_fee);
    assert_eq!(
        *app.execution_progress(),
        progress_before_validation,
        "validate_user_op mutated scheduler-owned application progress"
    );
    if let Err(reason) = validation {
        return Ok(ExecutionOutcome::Invalid(reason));
    }

    let valid = ValidUserOp {
        sender,
        fee: current_fee,
        data: user_op.data.to_vec(),
    };
    execute_valid_user_op(app, &valid, safe_block).map(ExecutionOutcome::Included)
}

/// Execute one already-validated user op and advance scheduler-owned progress.
pub fn execute_valid_user_op<A: Application>(
    app: &mut A,
    user_op: &ValidUserOp,
    safe_block: u64,
) -> Result<ExecutedInput, AppError> {
    let seal = CapabilitySeal;
    execute_and_advance(app, safe_block, |app| {
        app.apply_valid_user_op(ApplyInputCapability { _seal: &seal }, user_op, safe_block)
    })
}

/// Execute one direct input and advance scheduler-owned progress.
pub fn execute_direct_input<A: Application>(
    app: &mut A,
    input: &DirectInput,
) -> Result<ExecutedInput, AppError> {
    let seal = CapabilitySeal;
    execute_and_advance(app, input.block_number, |app| {
        app.apply_direct_input(ApplyInputCapability { _seal: &seal }, input)
    })
}

fn execute_and_advance<A, F>(
    app: &mut A,
    safe_block: u64,
    apply: F,
) -> Result<ExecutedInput, AppError>
where
    A: Application,
    F: FnOnce(&mut A) -> Result<AppOutputs, AppError>,
{
    let progress_before = *app.execution_progress();
    let progress_after = progress_before
        .checked_after_input(safe_block)
        .expect("executed input count overflow: no canonical successor");

    let apply_result = apply(app);
    assert_eq!(
        *app.execution_progress(),
        progress_before,
        "application hook mutated scheduler-owned application progress"
    );
    let outputs = apply_result?;

    let seal = CapabilitySeal;
    *app.execution_progress_mut(ProgressCommitCapability { _seal: &seal }) = progress_after;
    assert_eq!(
        *app.execution_progress(),
        progress_after,
        "application progress commit is incoherent with its immutable accessor"
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
        commit_target: ApplicationProgress,
        applied: u64,
        reject: bool,
        fail: bool,
        mutate_progress_in_hook: bool,
        misdirect_progress_commit: bool,
    }

    impl ProgressApp {
        fn new(count: u64) -> Self {
            Self {
                progress: ApplicationProgress::new(ExecutedInputCount::new(count), 0),
                commit_target: ApplicationProgress::default(),
                applied: 0,
                reject: false,
                fail: false,
                mutate_progress_in_hook: false,
                misdirect_progress_commit: false,
            }
        }
    }

    impl Application for ProgressApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = 0;

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<(), InvalidReason> {
            if self.reject {
                Err(InvalidReason::InvalidNonce {
                    expected: 1,
                    got: 0,
                })
            } else {
                Ok(())
            }
        }

        fn apply_valid_user_op(
            &mut self,
            _capability: ApplyInputCapability<'_>,
            _user_op: &ValidUserOp,
            _safe_block: u64,
        ) -> Result<AppOutputs, AppError> {
            self.applied += 1;
            if self.mutate_progress_in_hook {
                self.progress = ApplicationProgress::new(
                    self.progress
                        .executed_input_count()
                        .checked_next()
                        .expect("test count"),
                    99,
                );
            }
            if self.fail {
                Err(AppError::Internal {
                    reason: "injected failure".to_string(),
                })
            } else {
                Ok(Vec::new())
            }
        }

        fn apply_direct_input(
            &mut self,
            _capability: ApplyInputCapability<'_>,
            _input: &DirectInput,
        ) -> Result<AppOutputs, AppError> {
            self.applied += 1;
            if self.mutate_progress_in_hook {
                self.progress = ApplicationProgress::new(
                    self.progress
                        .executed_input_count()
                        .checked_next()
                        .expect("test count"),
                    99,
                );
            }
            if self.fail {
                Err(AppError::Internal {
                    reason: "injected failure".to_string(),
                })
            } else {
                Ok(Vec::new())
            }
        }

        fn execution_progress(&self) -> &ApplicationProgress {
            &self.progress
        }

        fn execution_progress_mut(
            &mut self,
            _capability: ProgressCommitCapability<'_>,
        ) -> &mut ApplicationProgress {
            if self.misdirect_progress_commit {
                &mut self.commit_target
            } else {
                &mut self.progress
            }
        }

        fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
            unreachable!("not used")
        }

        fn create_dump(&self, _prefix: &Path) -> Result<(), AppError> {
            unreachable!("not used")
        }

        fn delete_dump(_prefix: &Path) -> Result<(), AppError> {
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

    #[test]
    fn shared_boundaries_own_count_and_clock_progress() {
        let mut app = ProgressApp::new(0);
        let user = validate_and_execute_user_op(&mut app, Address::ZERO, &user_op(), 0, 9)
            .expect("execute user op");
        let ExecutionOutcome::Included(user) = user else {
            panic!("user op should be included")
        };
        assert_eq!(user.offset, ExecutedInputCount::ZERO);
        assert_eq!(app.executed_input_count(), ExecutedInputCount::new(1));
        assert_eq!(app.last_executed_safe_block(), 9);

        let direct = execute_direct_input(
            &mut app,
            &DirectInput {
                sender: Address::ZERO,
                block_number: 12,
                payload: Vec::new(),
            },
        )
        .expect("execute direct");
        assert_eq!(direct.offset, ExecutedInputCount::new(1));
        assert_eq!(app.executed_input_count(), ExecutedInputCount::new(2));
        assert_eq!(app.last_executed_safe_block(), 12);
    }

    #[test]
    fn rejection_and_error_do_not_commit_progress() {
        let mut rejected = ProgressApp::new(7);
        rejected.reject = true;
        assert!(matches!(
            validate_and_execute_user_op(&mut rejected, Address::ZERO, &user_op(), 0, 9)
                .expect("validation rejection"),
            ExecutionOutcome::Invalid(_)
        ));
        assert_eq!(rejected.executed_input_count(), ExecutedInputCount::new(7));
        assert_eq!(rejected.applied, 0);

        let mut failed = ProgressApp::new(7);
        failed.fail = true;
        assert!(
            execute_direct_input(
                &mut failed,
                &DirectInput {
                    sender: Address::ZERO,
                    block_number: 12,
                    payload: Vec::new(),
                },
            )
            .is_err()
        );
        assert_eq!(failed.executed_input_count(), ExecutedInputCount::new(7));
        assert_eq!(failed.last_executed_safe_block(), 0);
    }

    #[test]
    fn count_exhaustion_fails_before_application_mutation() {
        let mut app = ProgressApp::new(u64::MAX);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = execute_direct_input(
                &mut app,
                &DirectInput {
                    sender: Address::ZERO,
                    block_number: 1,
                    payload: Vec::new(),
                },
            );
        }));
        assert!(panic.is_err());
        assert_eq!(app.applied, 0, "overflow must preflight the app hook");
        assert_eq!(
            app.executed_input_count(),
            ExecutedInputCount::new(u64::MAX)
        );
    }

    #[test]
    #[should_panic(expected = "zero executed inputs require a zero safe-block clock")]
    fn zero_count_rejects_nonzero_safe_block_clock() {
        let _ = ApplicationProgress::new(ExecutedInputCount::ZERO, 1);
    }

    #[test]
    fn hook_progress_mutation_panics_on_success_and_error() {
        for fail in [false, true] {
            let mut app = ProgressApp::new(1);
            app.fail = fail;
            app.mutate_progress_in_hook = true;
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = execute_direct_input(
                    &mut app,
                    &DirectInput {
                        sender: Address::ZERO,
                        block_number: 12,
                        payload: Vec::new(),
                    },
                );
            }));
            assert!(
                panic.is_err(),
                "progress mutation must fail loud when hook fail={fail}"
            );
        }
    }

    #[test]
    fn incoherent_mutable_progress_accessor_fails_after_commit() {
        let mut app = ProgressApp::new(1);
        app.misdirect_progress_commit = true;
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = execute_direct_input(
                &mut app,
                &DirectInput {
                    sender: Address::ZERO,
                    block_number: 12,
                    payload: Vec::new(),
                },
            );
        }));
        assert!(
            panic.is_err(),
            "incoherent progress accessors must fail loud"
        );
    }
}
