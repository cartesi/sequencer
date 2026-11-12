// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::history::L2TxCount;
use crate::l2_tx::{Direct, L2Tx, UserOpTx};
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
pub enum AppOutput {
    Notice(Vec<u8>),
    Voucher {
        destination: Address,
        value: U256,
        payload: Vec<u8>,
    },
}

pub type AppOutputs = Vec<AppOutput>;

/// The scheduler constants a template compiles in for its application. They
/// belong to the application: the window is how long it lets a sequencer hold
/// a direct back, and the other two bound what its directs may cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerLimits {
    /// The staleness window, at least 1: a direct queued this many L1 blocks
    /// is executed, and a batch whose first frame is this old is skipped.
    pub max_wait_blocks: u64,
    /// How many admitted directs the fridge may hold.
    pub fridge_bound: u64,
    /// A direct with a longer payload never joins the fridge.
    pub max_direct_payload_bytes: u64,
}

/// The lane's admission verdict. Policy, never consensus: the canonical
/// machine never asks, and the answer never changes what an included op does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Admit,
    Refuse(PolicyRefusal),
}

/// Why the operator does not want an op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyRefusal {
    /// The sender cannot pay the frame fee. "Fee" (not "gas"): the fee
    /// tracks DA usage; compute metering, if it ever exists, will be a
    /// separate concept.
    Underfunded { required: U256, available: U256 },
    /// Any other operator policy; its text reaches the client verbatim.
    Other(String),
}

/// Why the executor skipped a user op. A skipped op changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The op's nonce is not its sender's next.
    WrongNonce { expected: u32, got: u32 },
    /// The sender's next nonce is `u32::MAX`, which has no successor.
    NonceExhausted,
}

/// Why the lane refuses a live user op. Its text is the `POST /tx` 422 body,
/// which clients parse, so every variant prints exactly today's wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Both values are log-space exponents (base 129/128).
    FeeCapBelowPrice {
        max_fee: u16,
        price: u16,
    },
    Skip(SkipReason),
    Policy(PolicyRefusal),
}

impl fmt::Display for PolicyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Underfunded {
                required,
                available,
            } => write!(
                f,
                "insufficient balance for fee: required {required}, available {available}"
            ),
            Self::Other(reason) => f.write_str(reason),
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongNonce { expected, got } => {
                write!(f, "bad nonce: expected {expected}, got {got}")
            }
            Self::NonceExhausted => write!(f, "nonce {} has no successor", u32::MAX),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FeeCapBelowPrice { max_fee, price } => {
                write!(f, "max fee {max_fee} below base fee {price}")
            }
            Self::Skip(reason) => reason.fmt(f),
            Self::Policy(reason) => reason.fmt(f),
        }
    }
}

/// What became of one L2 transaction handed to [`execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Executed {
    /// It executed; `index` is the L2 tx count before it, its index in the history.
    Done {
        index: L2TxCount,
        outputs: AppOutputs,
    },
    /// A user op whose nonce is not its sender's next. Nothing changed.
    Skipped(SkipReason),
}

/// Deterministic application state with exclusive ownership and thread transfer.
///
/// The application executes every transaction it is handed and stores each
/// sender's next nonce; shared code ([`execute`]) decides which user ops run,
/// so an application bug can change what an op does but not whether it runs.
pub trait Application: Send + Sized {
    // Execution: the canonical machine, the sequencer and every replica.

    /// The sender's next nonce; zero for an address that has sent nothing.
    /// Must not change state. [`execute`] compares it with the op's nonce.
    fn user_nonce(&self, sender: Address) -> Result<u32, AppError>;

    /// Execute a user op [`execute`] found at its sender's next nonce. It never
    /// refuses: whatever the op does or pays, even a fee the sender cannot
    /// cover (charging what it has, a saturating subtraction, is recommended),
    /// the sender's nonce and the count each advance by exactly one.
    /// `AppError` is fatal: callers discard the instance.
    fn apply_user_op(&mut self, op: &UserOpTx) -> Result<AppOutputs, AppError>;

    /// Execute a direct. Ignored or malformed payloads still count: the count
    /// advances by exactly one and no nonce moves. `AppError` is fatal.
    fn apply_direct(&mut self, direct: &Direct) -> Result<AppOutputs, AppError>;

    /// The L2 tx count: starts at zero and advances by exactly one per
    /// executed transaction, so an application at `X` executes history entry
    /// `X` next. It must survive dump round-trips.
    fn l2_tx_count(&self) -> L2TxCount;

    // Admission: the sequencer's lane only. Policy, never consensus.

    /// Maximum user-op method payload size, stable for this implementation,
    /// which only the lane enforces. Zero permits only empty method payloads.
    fn max_method_payload_bytes() -> usize;

    /// The template's scheduler constants for this application.
    fn scheduler_limits() -> SchedulerLimits;

    /// Does the operator want this op in a frame at this fee? Must not change
    /// state and must not check the nonce, which [`check_live_user_op`] does
    /// first. The canonical machine never asks, so a guest need not link it.
    fn admit_user_op(&self, sender: Address, op: &UserOp, fee: u16) -> Result<Admission, AppError>;

    // Durable state: the sequencer and replicas.

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
    /// rehydrate equivalent logical state, including the count. Creating the
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
    /// whose bytes equal what the canonical machine yields after the same
    /// inputs, also for an engine restored with [`Application::from_dump`]:
    /// its whole labeled state range, or its `state` inspect report
    /// (docs/protocol/application-contract.md, "Comparison bytes").
    /// The recovery dump and canonical state file may be the same file when
    /// their representations coincide.
    fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError>;

    /// Path of the canonical state file in a dump at `prefix` (possibly
    /// `prefix` itself). The returned path must point at a single file. It
    /// is a pure function of `prefix`: callers may invoke it without
    /// loading the dump or instantiating the Application.
    fn state_file_in_dump(prefix: &Path) -> PathBuf;

    /// Build an instance from a canonical machine's comparison bytes, held in
    /// `state_file`, at the machine's count, whether or not those bytes hold
    /// it. A dump of the result holds the same comparison bytes.
    fn from_canonical_state(state_file: &Path, l2_tx_count: L2TxCount) -> Result<Self, AppError>;
}

/// Canonical inspection for applications hosted by the shared Rust scheduler.
/// Applications using another canonical runtime need not implement this trait.
pub trait CanonicalState {
    /// Deterministic bytes matching the state file in a dump of this state;
    /// [`Application::from_canonical_state`] on them yields an equal instance.
    fn canonical_snapshot_bytes(&self) -> Result<Vec<u8>, AppError>;
}

/// The nonce rule, shared by [`execute`] and [`check_live_user_op`] so the
/// lane refuses exactly what the canonical machine would skip. The mismatch
/// comes first, so a stale op carrying `u32::MAX` reads as a wrong nonce.
fn nonce_skip(expected: u32, got: u32) -> Option<SkipReason> {
    if got != expected {
        Some(SkipReason::WrongNonce { expected, got })
    } else if expected == u32::MAX {
        Some(SkipReason::NonceExhausted)
    } else {
        None
    }
}

/// The one way a transaction reaches the application, in the canonical
/// machine, a rebuild, the lane and its replay alike. A direct always
/// executes; a user op executes only at its sender's next nonce. Wrong
/// application bookkeeping after a successful hook fails loud.
pub fn execute<A: Application>(app: &mut A, tx: &L2Tx) -> Result<Executed, AppError> {
    match tx {
        L2Tx::Direct(direct) => {
            let index = preflight_count(app);
            let outputs = app.apply_direct(direct)?;
            assert_count_advanced(app, index);
            // A direct's no-nonce rule is not checked: the executor cannot
            // enumerate the senders whose nonces it might have moved.
            Ok(Executed::Done { index, outputs })
        }
        L2Tx::UserOp(op) => {
            let expected = app.user_nonce(op.sender)?;
            if let Some(skip) = nonce_skip(expected, op.nonce) {
                return Ok(Executed::Skipped(skip));
            }
            let nonce_after = expected
                .checked_add(1)
                .expect("nonce_skip lets no op through at u32::MAX");
            let index = preflight_count(app);
            let outputs = app.apply_user_op(op)?;
            assert_count_advanced(app, index);
            assert_eq!(
                app.user_nonce(op.sender)?,
                nonce_after,
                "an executed user op must advance its sender's nonce exactly once"
            );
            Ok(Executed::Done { index, outputs })
        }
    }
}

/// The lane's pre-check of a live op, in the order whose texts clients see:
/// the fee cap, then the executor's own nonce rule, then admission. `None`
/// means the op is wanted and [`execute`] will run it, not skip it.
pub fn check_live_user_op<A: Application>(
    app: &A,
    sender: Address,
    op: &UserOp,
    frame_fee: u16,
) -> Result<Option<Refusal>, AppError> {
    if op.max_fee < frame_fee {
        return Ok(Some(Refusal::FeeCapBelowPrice {
            max_fee: op.max_fee,
            price: frame_fee,
        }));
    }
    if let Some(skip) = nonce_skip(app.user_nonce(sender)?, op.nonce) {
        return Ok(Some(Refusal::Skip(skip)));
    }
    match app.admit_user_op(sender, op, frame_fee)? {
        Admission::Admit => Ok(None),
        Admission::Refuse(policy) => Ok(Some(Refusal::Policy(policy))),
    }
}

/// The count before execution. Panics before the hook runs when the count
/// has no successor, so an exhausted history never mutates the application.
fn preflight_count<A: Application>(app: &A) -> L2TxCount {
    let count = app.l2_tx_count();
    count
        .checked_next()
        .expect("L2 tx count overflow: no canonical successor");
    count
}

fn assert_count_advanced<A: Application>(app: &A, before: L2TxCount) {
    assert_eq!(
        Some(app.l2_tx_count()),
        before.checked_next(),
        "successful application execution must advance the count exactly once"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// One sender's worth of state with knobs for every way an application can
    /// misbehave. The nonce is shared by all senders, which suffices here.
    struct ProgressApp {
        count: L2TxCount,
        nonce: u32,
        applied: u64,
        admit_calls: Cell<u64>,
        nonce_steps: u32,
        count_steps: usize,
        fail_nonce: bool,
        fail_apply: bool,
        admission: Admission,
    }

    impl ProgressApp {
        fn new(count: u64) -> Self {
            Self {
                count: L2TxCount::new(count),
                nonce: 0,
                applied: 0,
                admit_calls: Cell::new(0),
                nonce_steps: 1,
                count_steps: 1,
                fail_nonce: false,
                fail_apply: false,
                admission: Admission::Admit,
            }
        }

        fn assert_usable(&self) {
            assert!(
                !(self.fail_apply && self.applied > 0),
                "failed instance is unusable"
            );
        }

        fn apply(&mut self) -> Result<AppOutputs, AppError> {
            self.applied += 1;
            if self.fail_apply {
                return Err(AppError::Internal {
                    reason: "execution failed".into(),
                });
            }
            for _ in 0..self.count_steps {
                self.count.advance();
            }
            Ok(Vec::new())
        }
    }

    impl Application for ProgressApp {
        fn user_nonce(&self, _sender: Address) -> Result<u32, AppError> {
            self.assert_usable();
            if self.fail_nonce {
                return Err(AppError::Internal {
                    reason: "nonce read failed".into(),
                });
            }
            Ok(self.nonce)
        }

        fn apply_user_op(&mut self, _op: &UserOpTx) -> Result<AppOutputs, AppError> {
            let outputs = self.apply()?;
            self.nonce = self.nonce.wrapping_add(self.nonce_steps);
            Ok(outputs)
        }

        fn apply_direct(&mut self, _direct: &Direct) -> Result<AppOutputs, AppError> {
            self.apply()
        }

        fn l2_tx_count(&self) -> L2TxCount {
            self.assert_usable();
            self.count
        }

        fn max_method_payload_bytes() -> usize {
            0
        }

        fn scheduler_limits() -> SchedulerLimits {
            SchedulerLimits {
                max_wait_blocks: 1,
                fridge_bound: 0,
                max_direct_payload_bytes: 0,
            }
        }

        fn admit_user_op(
            &self,
            _sender: Address,
            _op: &UserOp,
            _fee: u16,
        ) -> Result<Admission, AppError> {
            self.admit_calls.set(self.admit_calls.get() + 1);
            Ok(self.admission.clone())
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
        fn from_canonical_state(
            _state_file: &Path,
            _l2_tx_count: L2TxCount,
        ) -> Result<Self, AppError> {
            unreachable!("not used")
        }
    }

    fn user_op(nonce: u32, max_fee: u16) -> UserOp {
        UserOp {
            nonce,
            max_fee,
            data: Vec::new().into(),
        }
    }

    fn user_tx(nonce: u32) -> L2Tx {
        L2Tx::UserOp(UserOpTx::new(Address::ZERO, &user_op(nonce, 0), 0, 9))
    }

    fn direct(block: u64) -> L2Tx {
        L2Tx::Direct(Direct {
            input_index: 0,
            sender: Address::ZERO,
            block,
            timestamp: 0,
            payload: Vec::new(),
        })
    }

    fn underfunded() -> PolicyRefusal {
        PolicyRefusal::Underfunded {
            required: U256::from(10),
            available: U256::from(3),
        }
    }

    fn panic_message(run: impl FnOnce()) -> String {
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
            .expect_err("the run must panic");
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    #[test]
    fn refusal_texts_are_byte_identical_to_the_served_422_bodies() {
        let cases = [
            (
                Refusal::Skip(SkipReason::WrongNonce {
                    expected: 3,
                    got: 7,
                }),
                "bad nonce: expected 3, got 7",
            ),
            (
                Refusal::FeeCapBelowPrice {
                    max_fee: 5,
                    price: 9,
                },
                "max fee 5 below base fee 9",
            ),
            (
                Refusal::Skip(SkipReason::NonceExhausted),
                "nonce 4294967295 has no successor",
            ),
            (
                Refusal::Policy(underfunded()),
                "insufficient balance for fee: required 10, available 3",
            ),
            (
                Refusal::Policy(PolicyRefusal::Other("sponsor quota spent".into())),
                "sponsor quota spent",
            ),
        ];
        for (refusal, text) in cases {
            assert_eq!(refusal.to_string(), text);
        }
        assert_eq!(
            SkipReason::NonceExhausted.to_string(),
            "nonce 4294967295 has no successor"
        );
        assert_eq!(
            underfunded().to_string(),
            "insufficient balance for fee: required 10, available 3"
        );
    }

    #[test]
    fn the_count_defines_indexes_and_only_user_ops_move_the_nonce() {
        let mut app = ProgressApp::new(0);
        let Executed::Done { index, .. } = execute(&mut app, &user_tx(0)).unwrap() else {
            panic!("a user op at its sender's nonce executes")
        };
        assert_eq!(index, L2TxCount::ZERO);
        assert_eq!(app.l2_tx_count(), L2TxCount::new(1));
        assert_eq!(app.nonce, 1);

        for expected in [1, 2] {
            let Executed::Done { index, outputs } = execute(&mut app, &direct(12)).unwrap() else {
                panic!("a direct always executes")
            };
            assert_eq!(index, L2TxCount::new(expected));
            assert!(
                outputs.is_empty(),
                "executed no-ops still advance the count"
            );
            assert_eq!(app.l2_tx_count(), L2TxCount::new(expected + 1));
        }
        assert_eq!(app.nonce, 1, "a direct never moves a nonce");
    }

    #[test]
    fn a_skipped_op_changes_nothing_and_never_applies() {
        for (app_nonce, op_nonce, reason) in [
            (
                1,
                0,
                SkipReason::WrongNonce {
                    expected: 1,
                    got: 0,
                },
            ),
            (u32::MAX, u32::MAX, SkipReason::NonceExhausted),
        ] {
            let mut app = ProgressApp::new(7);
            app.nonce = app_nonce;
            assert_eq!(
                execute(&mut app, &user_tx(op_nonce)).unwrap(),
                Executed::Skipped(reason)
            );
            assert_eq!(app.applied, 0);
            assert_eq!(app.l2_tx_count(), L2TxCount::new(7));
            assert_eq!(app.nonce, app_nonce);
        }
    }

    #[test]
    fn a_skip_needs_no_count_successor() {
        let mut app = ProgressApp::new(u64::MAX);
        app.nonce = 1;
        assert!(matches!(
            execute(&mut app, &user_tx(0)).unwrap(),
            Executed::Skipped(SkipReason::WrongNonce { .. })
        ));
    }

    #[test]
    fn a_nonce_read_error_propagates_without_executing() {
        let mut app = ProgressApp::new(7);
        app.fail_nonce = true;
        assert!(matches!(
            execute(&mut app, &user_tx(0)),
            Err(AppError::Internal { reason }) if reason == "nonce read failed"
        ));
        assert!(matches!(
            check_live_user_op(&app, Address::ZERO, &user_op(0, 0), 0),
            Err(AppError::Internal { reason }) if reason == "nonce read failed"
        ));
        assert_eq!(app.applied, 0);
        assert_eq!(app.admit_calls.get(), 0);
        assert_eq!(app.l2_tx_count(), L2TxCount::new(7));
    }

    #[test]
    fn mismatch_is_reported_before_exhaustion() {
        let mut app = ProgressApp::new(0);
        let stale = SkipReason::WrongNonce {
            expected: 0,
            got: u32::MAX,
        };
        assert_eq!(
            execute(&mut app, &user_tx(u32::MAX)).unwrap(),
            Executed::Skipped(stale),
            "an op carrying u32::MAX for a fresh sender is a wrong nonce"
        );
        assert_eq!(
            check_live_user_op(&app, Address::ZERO, &user_op(u32::MAX, 0), 0).unwrap(),
            Some(Refusal::Skip(stale))
        );

        app.nonce = u32::MAX;
        assert_eq!(
            execute(&mut app, &user_tx(3)).unwrap(),
            Executed::Skipped(SkipReason::WrongNonce {
                expected: u32::MAX,
                got: 3
            }),
            "an exhausted sender's stale op still reads as a wrong nonce"
        );
        assert_eq!(
            execute(&mut app, &user_tx(u32::MAX)).unwrap(),
            Executed::Skipped(SkipReason::NonceExhausted)
        );
        // The lane refuses what the executor would skip, before admission.
        assert_eq!(
            check_live_user_op(&app, Address::ZERO, &user_op(u32::MAX, 0), 0).unwrap(),
            Some(Refusal::Skip(SkipReason::NonceExhausted))
        );
        assert_eq!(app.admit_calls.get(), 0);
        assert_eq!(app.applied, 0);
    }

    #[test]
    fn the_fee_cap_is_checked_before_the_nonce_and_admission() {
        let app = ProgressApp {
            fail_nonce: true,
            admission: Admission::Refuse(underfunded()),
            ..ProgressApp::new(0)
        };
        assert_eq!(
            check_live_user_op(&app, Address::ZERO, &user_op(5, 0), 1).unwrap(),
            Some(Refusal::FeeCapBelowPrice {
                max_fee: 0,
                price: 1
            })
        );
        assert_eq!(app.admit_calls.get(), 0);
    }

    #[test]
    fn the_nonce_is_checked_before_admission() {
        let app = ProgressApp {
            nonce: 1,
            admission: Admission::Refuse(underfunded()),
            ..ProgressApp::new(0)
        };
        assert_eq!(
            check_live_user_op(&app, Address::ZERO, &user_op(0, 1), 1).unwrap(),
            Some(Refusal::Skip(SkipReason::WrongNonce {
                expected: 1,
                got: 0
            }))
        );
        assert_eq!(app.admit_calls.get(), 0);
    }

    #[test]
    fn an_admission_refusal_surfaces_without_executing() {
        let mut app = ProgressApp {
            admission: Admission::Refuse(underfunded()),
            ..ProgressApp::new(4)
        };
        assert_eq!(
            check_live_user_op(&app, Address::ZERO, &user_op(0, 1), 1).unwrap(),
            Some(Refusal::Policy(underfunded()))
        );
        assert_eq!(app.admit_calls.get(), 1);
        assert_eq!(app.applied, 0);
        assert_eq!(app.l2_tx_count(), L2TxCount::new(4));
        assert_eq!(app.nonce, 0);

        app.admission = Admission::Admit;
        assert_eq!(
            check_live_user_op(&app, Address::ZERO, &user_op(0, 1), 1).unwrap(),
            None
        );
    }

    #[test]
    fn execute_never_consults_admission() {
        let mut app = ProgressApp {
            admission: Admission::Refuse(underfunded()),
            ..ProgressApp::new(0)
        };
        assert!(matches!(
            execute(&mut app, &user_tx(0)).unwrap(),
            Executed::Done { .. }
        ));
        assert_eq!(app.admit_calls.get(), 0);
        assert_eq!(app.nonce, 1);
    }

    #[test]
    fn execution_error_is_propagated_without_reading_the_failed_instance() {
        for tx in [direct(12), user_tx(0)] {
            let mut app = ProgressApp::new(7);
            app.fail_apply = true;
            assert!(matches!(
                execute(&mut app, &tx),
                Err(AppError::Internal { reason }) if reason == "execution failed"
            ));
            assert_eq!(app.applied, 1);
        }
    }

    #[test]
    fn count_exhaustion_fails_before_application_mutation() {
        for tx in [direct(1), user_tx(0)] {
            let mut app = ProgressApp::new(u64::MAX);
            let message = panic_message(|| {
                let _ = execute(&mut app, &tx);
            });
            assert_eq!(message, "L2 tx count overflow: no canonical successor");
            assert_eq!(app.applied, 0, "overflow must preflight the app hook");
            assert_eq!(app.l2_tx_count(), L2TxCount::new(u64::MAX));
        }
    }

    #[test]
    fn successful_hooks_must_advance_the_count_exactly_once() {
        for steps in [0, 2] {
            for tx in [direct(12), user_tx(0)] {
                let mut app = ProgressApp::new(1);
                app.count_steps = steps;
                let message = panic_message(|| {
                    let _ = execute(&mut app, &tx);
                });
                assert!(
                    message.contains("must advance the count exactly once"),
                    "{message}"
                );
            }
        }
    }

    #[test]
    fn successful_user_ops_must_advance_the_nonce_exactly_once() {
        for steps in [0, 2] {
            let mut app = ProgressApp::new(1);
            app.nonce_steps = steps;
            let message = panic_message(|| {
                let _ = execute(&mut app, &user_tx(0));
            });
            assert!(
                message.contains("must advance its sender's nonce exactly once"),
                "{message}"
            );
        }
    }
}
