// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The canonical loop: feed an input to the scheduler, then run each
//! transaction it hands out through the executor. The guest harness and the
//! tests drive an application through it, and it answers the machine's export.

use super::{InputOutcome, L1Input, Scheduled, Scheduler, SchedulerConfig, SchedulerExport};
use crate::application::{
    AppError, AppOutputs, Application, CanonicalState, Executed, SkipReason, execute,
};
use crate::history::ExecutedInputCount;

/// The `state` inspect query: the application's comparison bytes, whole. An
/// empty query means the same.
pub const STATE_INSPECT_QUERY: &[u8] = b"state";

/// The `scheduler` inspect query: the [`SchedulerExport`] report, 48 bytes.
pub const SCHEDULER_INSPECT_QUERY: &[u8] = b"scheduler";

/// An inspect query the machine cannot answer. It gets a report, never a halt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectError {
    UnsupportedQuery,
    Application(String),
}

/// What one input did, in execution order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advanced {
    pub outcome: InputOutcome,
    /// Every executed transaction's outputs, concatenated.
    pub outputs: AppOutputs,
    /// Every handed-out transaction, and whether it executed.
    pub txs: Vec<TxRecord>,
}

/// One handed-out transaction and its fate; its outputs are in
/// [`Advanced::outputs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxRecord {
    pub scheduled: Scheduled,
    pub fate: TxFate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxFate {
    /// It executed; `offset` is the count before it, its history offset.
    Done { offset: ExecutedInputCount },
    /// The executor skipped a user op for its nonce. Nothing changed.
    Skipped(SkipReason),
}

/// A scheduler and the application it feeds, kept in step input by input.
pub struct CanonicalRunner<A: Application> {
    scheduler: Scheduler,
    app: A,
    /// Set by an `AppError`. The scheduler alone cannot tell: the failing
    /// transaction may have been its input's last.
    failed: bool,
}

impl<A: Application> CanonicalRunner<A> {
    /// The genesis machine: both sides at zero.
    pub fn genesis(app: A, config: SchedulerConfig) -> Self {
        assert_eq!(
            app.executed_input_count(),
            ExecutedInputCount::ZERO,
            "a genesis application must start at executed_input_count = 0"
        );
        Self::from_parts(Scheduler::genesis(config), app)
    }

    /// Pair a scheduler with an application at the same point, such as a
    /// resumed scheduler and the application projected from the same machine.
    pub fn from_parts(scheduler: Scheduler, app: A) -> Self {
        Self {
            scheduler,
            app,
            failed: false,
        }
    }

    pub fn into_parts(self) -> (Scheduler, A) {
        (self.scheduler, self.app)
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    pub fn app(&self) -> &A {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut A {
        &mut self.app
    }

    /// The machine's `scheduler` report.
    pub fn export(&self) -> SchedulerExport {
        SchedulerExport::new(self.scheduler.state(), self.app.executed_input_count())
    }

    /// Feed one input and execute every transaction it hands out. An
    /// `AppError` is fatal: it leaves the input half done, so the runner must
    /// be discarded, and a later `process_input` panics.
    pub fn process_input(&mut self, input: L1Input) -> Result<Advanced, AppError> {
        assert!(
            !self.failed,
            "an application error left this runner half done: discard it"
        );
        let outcome = self.scheduler.feed(input);
        let mut outputs = Vec::new();
        let mut txs = Vec::new();
        while let Some(scheduled) = self.scheduler.next_tx() {
            let executed =
                execute(&mut self.app, &scheduled.tx).inspect_err(|_| self.failed = true)?;
            let fate = match executed {
                Executed::Done {
                    offset,
                    outputs: tx_outputs,
                } => {
                    outputs.extend(tx_outputs);
                    TxFate::Done { offset }
                }
                Executed::Skipped(reason) => TxFate::Skipped(reason),
            };
            txs.push(TxRecord { scheduled, fate });
        }
        Ok(Advanced {
            outcome,
            outputs,
            txs,
        })
    }
}

impl<A: Application + CanonicalState> CanonicalRunner<A> {
    /// Answer an inspect query: the application's comparison bytes, unchanged
    /// from today, or the scheduler's report, which the harness owns for every
    /// application.
    pub fn inspect(&self, query: &[u8]) -> Result<Vec<u8>, InspectError> {
        if query.is_empty() || query == STATE_INSPECT_QUERY {
            self.app
                .canonical_snapshot_bytes()
                .map_err(|err| InspectError::Application(err.to_string()))
        } else if query == SCHEDULER_INSPECT_QUERY {
            Ok(self.export().encode().to_vec())
        } else {
            Err(InspectError::UnsupportedQuery)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{Admission, AppOutput, PolicyRefusal, SchedulerLimits};
    use crate::l2_tx::{Direct, L2Tx, UserOpTx};
    use crate::scheduler::fixtures::*;
    use crate::scheduler::{BatchRejectReason, SchedulerState};
    use crate::user_op::UserOp;
    use alloy_primitives::{Address, U256};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        UserOp(u8),
        Direct(u8),
    }

    /// A small fee-charging application. It charges what the sender has, as
    /// the trait recommends, so an underfunded op still executes.
    #[derive(Default)]
    struct RecordingApp {
        events: Vec<Event>,
        balances: BTreeMap<Address, U256>,
        nonces: BTreeMap<Address, u32>,
        count: ExecutedInputCount,
        fail_on: Option<Event>,
    }

    impl RecordingApp {
        fn balance_of(&self, sender: Address) -> U256 {
            self.balances.get(&sender).copied().unwrap_or_default()
        }

        fn credit(&mut self, sender: Address, amount: u64) {
            let balance = self.balance_of(sender).saturating_add(U256::from(amount));
            self.balances.insert(sender, balance);
        }

        fn record(&mut self, event: Event) -> Result<AppOutputs, AppError> {
            if self.fail_on == Some(event) {
                return Err(AppError::Internal {
                    reason: format!("refusing {event:?}"),
                });
            }
            self.events.push(event);
            self.count.advance();
            let output = match event {
                Event::UserOp(marker) => vec![b'u', marker],
                Event::Direct(marker) => vec![b'd', marker],
            };
            Ok(vec![AppOutput::Notice(output)])
        }
    }

    impl Application for RecordingApp {
        fn user_nonce(&self, sender: Address) -> Result<u32, AppError> {
            Ok(self.nonces.get(&sender).copied().unwrap_or(0))
        }

        fn apply_user_op(&mut self, op: &UserOpTx) -> Result<AppOutputs, AppError> {
            let outputs = self.record(Event::UserOp(op.data.first().copied().unwrap_or(0)))?;
            let fee = crate::fee::fee_to_linear(op.fee);
            let balance = self.balance_of(op.sender);
            self.balances.insert(op.sender, balance.saturating_sub(fee));
            let nonce = self.user_nonce(op.sender)?;
            self.nonces.insert(op.sender, nonce + 1);
            Ok(outputs)
        }

        fn apply_direct(&mut self, direct: &Direct) -> Result<AppOutputs, AppError> {
            self.record(Event::Direct(direct.payload.first().copied().unwrap_or(0)))
        }

        fn executed_input_count(&self) -> ExecutedInputCount {
            self.count
        }

        fn max_method_payload_bytes() -> usize {
            1 + 32 + 20
        }

        fn scheduler_limits() -> SchedulerLimits {
            SchedulerLimits {
                fridge_bound: 7,
                max_direct_payload_bytes: 9,
            }
        }

        fn admit_user_op(
            &self,
            sender: Address,
            _op: &UserOp,
            fee: u16,
        ) -> Result<Admission, AppError> {
            let required = crate::fee::fee_to_linear(fee);
            let available = self.balance_of(sender);
            Ok(if available < required {
                Admission::Refuse(PolicyRefusal::Underfunded {
                    required,
                    available,
                })
            } else {
                Admission::Admit
            })
        }

        fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
            unimplemented!("not used")
        }

        fn create_dump(&mut self, _prefix: &Path) -> Result<(), AppError> {
            unimplemented!("not used")
        }

        fn state_file_in_dump(prefix: &Path) -> PathBuf {
            prefix.to_path_buf()
        }

        fn from_canonical_state(
            _state_file: &Path,
            _executed_input_count: ExecutedInputCount,
        ) -> Result<Self, AppError> {
            unimplemented!("not used")
        }
    }

    impl CanonicalState for RecordingApp {
        fn canonical_snapshot_bytes(&self) -> Result<Vec<u8>, AppError> {
            Ok(format!("events:{}", self.events.len()).into_bytes())
        }
    }

    fn runner(app: RecordingApp, max_wait_blocks: u64) -> CanonicalRunner<RecordingApp> {
        CanonicalRunner::genesis(app, config(max_wait_blocks))
    }

    fn advance(
        runner: &mut CanonicalRunner<RecordingApp>,
        mut input: L1Input,
    ) -> Result<Advanced, AppError> {
        input.index = runner.scheduler().state().l1_input_count;
        runner.process_input(input)
    }

    fn fates(advanced: &Advanced) -> Vec<TxFate> {
        advanced.txs.iter().map(|record| record.fate).collect()
    }

    #[test]
    fn for_application_takes_the_protocol_window_and_the_template_limits() {
        assert_eq!(
            SchedulerConfig::for_application::<RecordingApp>(SEQUENCER),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: crate::MAX_WAIT_BLOCKS,
                fridge_bound: 7,
                max_direct_payload_bytes: 9,
            }
        );
    }

    #[test]
    #[should_panic(expected = "a genesis application must start at executed_input_count = 0")]
    fn genesis_rejects_an_application_past_zero() {
        let app = RecordingApp {
            count: ExecutedInputCount::new(1),
            ..RecordingApp::default()
        };
        let _ = runner(app, 100);
    }

    /// The scheduler withholds only the op whose cap is below the price; the
    /// executor skips a wrong nonce; an underfunded op executes and counts,
    /// which turns a later op at the same nonce into a wrong-nonce skip.
    #[test]
    fn the_executor_skips_wrong_nonces_and_an_underfunded_op_executes() {
        let mut runner = runner(RecordingApp::default(), 100);
        let signer = key(9);
        let sender = address_of(&signer);
        // fee_to_linear(0) = 1: enough for the cheapest fee only.
        runner.app_mut().credit(sender, 1);
        let frames = vec![
            frame(1, 1, vec![signed(&signer, 1, 10, vec![1])]),
            frame(1, 5, vec![signed(&signer, 0, 0, vec![2])]),
            frame(1, 1000, vec![signed(&signer, 0, 1000, vec![3])]),
            frame(1, 0, vec![signed(&signer, 0, 10, vec![4])]),
        ];
        let advanced = advance(&mut runner, batch(2, 0, frames)).expect("advance");
        assert_eq!(advanced.outcome, InputOutcome::BatchAccepted);
        assert_eq!(
            fates(&advanced),
            [
                TxFate::Skipped(SkipReason::WrongNonce {
                    expected: 0,
                    got: 1
                }),
                TxFate::Done {
                    offset: ExecutedInputCount::ZERO
                },
                TxFate::Skipped(SkipReason::WrongNonce {
                    expected: 1,
                    got: 0
                }),
            ]
        );
        assert_eq!(runner.app().events, [Event::UserOp(3)]);
        assert_eq!(runner.app().balance_of(sender), U256::ZERO);
        assert_eq!(runner.app().user_nonce(sender).unwrap(), 1);
        assert_eq!(advanced.outputs, [AppOutput::Notice(vec![b'u', 3])]);
    }

    #[test]
    fn outputs_follow_execution_order() {
        let mut runner = runner(RecordingApp::default(), 5);
        let signer = key(11);
        for input in [direct(1, 1), direct(3, 2), direct(4, 3)] {
            let advanced = advance(&mut runner, input).expect("queue");
            assert!(advanced.txs.is_empty());
        }
        let frames = vec![
            frame(3, 0, vec![signed(&signer, 0, 0, vec![10])]),
            frame(4, 0, vec![signed(&signer, 1, 0, vec![11])]),
        ];
        let advanced = advance(&mut runner, batch(6, 0, frames)).expect("advance");
        let notices: Vec<Vec<u8>> = advanced
            .outputs
            .iter()
            .map(|output| match output {
                AppOutput::Notice(bytes) => bytes.clone(),
                AppOutput::Voucher { .. } => panic!("no vouchers"),
            })
            .collect();
        // The backstop's direct, then frame 1's drain and op, then frame 2's.
        assert_eq!(
            notices,
            [
                vec![b'd', 1],
                vec![b'd', 2],
                vec![b'u', 10],
                vec![b'd', 3],
                vec![b'u', 11]
            ]
        );
        let offsets: Vec<TxFate> = (0..5)
            .map(|offset| TxFate::Done {
                offset: ExecutedInputCount::new(offset),
            })
            .collect();
        assert_eq!(fates(&advanced), offsets);
        assert!(
            advanced
                .txs
                .iter()
                .all(|record| record.scheduled.batch_nonce == 0)
        );
        assert_eq!(
            runner.app().executed_input_count(),
            ExecutedInputCount::new(5)
        );
    }

    #[test]
    fn an_application_error_stops_the_input_before_later_transactions() {
        let app = RecordingApp {
            fail_on: Some(Event::UserOp(1)),
            ..RecordingApp::default()
        };
        let mut runner = runner(app, 100);
        let signer = key(17);
        let ops = vec![
            signed(&signer, 0, 1, vec![1]),
            signed(&signer, 1, 1, vec![2]),
        ];
        let error = advance(&mut runner, batch(2, 0, vec![frame(1, 0, ops)]))
            .expect_err("an application error is fatal");
        let AppError::Internal { reason } = error else {
            panic!("unexpected application error: {error}");
        };
        assert_eq!(reason, "refusing UserOp(1)");
        assert!(runner.app().events.is_empty());
        assert_eq!(
            runner.app().executed_input_count(),
            ExecutedInputCount::ZERO
        );
    }

    #[test]
    fn a_backstop_direct_error_is_fatal() {
        let app = RecordingApp {
            fail_on: Some(Event::Direct(1)),
            ..RecordingApp::default()
        };
        let mut runner = runner(app, 5);
        advance(&mut runner, direct(1, 1)).expect("queue");
        let error = advance(&mut runner, direct(6, 2)).expect_err("the overdue direct fails");
        let AppError::Internal { reason } = error else {
            panic!("unexpected application error: {error}");
        };
        assert_eq!(reason, "refusing Direct(1)");
        assert!(runner.app().events.is_empty());
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

    /// Whether the failing transaction was its input's last or not, the
    /// runner refuses the next input rather than run a discarded instance.
    #[test]
    fn a_runner_cannot_continue_after_an_error() {
        for (label, failing) in [
            ("a transaction left", 1),
            ("the input's last transaction", 2),
        ] {
            let app = RecordingApp {
                fail_on: Some(Event::Direct(failing)),
                ..RecordingApp::default()
            };
            let mut runner = runner(app, 100);
            for marker in [1, 2] {
                advance(&mut runner, direct(1, marker)).expect("queue");
            }
            advance(&mut runner, batch(2, 0, vec![empty_frame(1)])).expect_err(label);
            let events = runner.app().events.clone();
            let message = panic_message(|| {
                let _ = advance(&mut runner, direct(3, 3));
            });
            assert_eq!(
                message, "an application error left this runner half done: discard it",
                "{label}"
            );
            assert_eq!(runner.app().events, events, "{label}");
        }
    }

    /// The backstop queues an overdue direct before gate b's preflight halts
    /// on the exhausted batch nonce; the halt still comes before it executes.
    #[test]
    fn batch_nonce_overflow_halts_before_a_backstop_direct_executes() {
        let mut queued = direct(1, 1);
        queued.index = 0;
        let state = SchedulerState {
            l1_input_count: 1,
            next_batch_nonce: u64::MAX,
            clock: 0,
            first_queued_input: 0,
            fridge_len: 1,
        };
        let scheduler = Scheduler::resume(config(5), state, [queued]).expect("resume");
        let mut runner = CanonicalRunner::from_parts(scheduler, RecordingApp::default());
        let op = signed(&key(19), 0, 0, vec![7]);
        let message = panic_message(|| {
            let _ = advance(
                &mut runner,
                batch(10, u64::MAX, vec![frame(9, 0, vec![op])]),
            );
        });
        assert_eq!(message, "batch nonce overflow: no canonical successor");
        assert!(runner.app().events.is_empty());
        assert_eq!(
            runner.app().executed_input_count(),
            ExecutedInputCount::ZERO
        );
    }

    #[test]
    fn inspect_answers_the_state_and_scheduler_queries() {
        let mut runner = runner(RecordingApp::default(), 100);
        advance(&mut runner, direct(1, 7)).expect("queue");
        // The state reflects executed transactions, not the fridge.
        assert_eq!(
            runner.inspect(STATE_INSPECT_QUERY),
            Ok(b"events:0".to_vec())
        );
        assert_eq!(runner.inspect(b""), Ok(b"events:0".to_vec()));

        advance(&mut runner, batch(2, 0, vec![empty_frame(1)])).expect("drain");
        advance(&mut runner, direct(3, 8)).expect("queue");
        assert_eq!(
            runner.inspect(STATE_INSPECT_QUERY),
            Ok(b"events:1".to_vec())
        );
        let report = runner.inspect(SCHEDULER_INSPECT_QUERY).expect("report");
        assert_eq!(
            SchedulerExport::decode(&report),
            Ok(SchedulerExport {
                l1_input_count: 3,
                l2_tx_count: 1,
                next_batch_nonce: 1,
                clock: 1,
                first_queued_input: 2,
                fridge_len: 1,
            })
        );
        assert_eq!(runner.export().encode().to_vec(), report);

        for query in [&b"balances"[..], b"State", b"scheduler ", b"state\0"] {
            assert_eq!(runner.inspect(query), Err(InspectError::UnsupportedQuery));
        }
    }

    /// Two machines from the same export, one carried on and one rebuilt from
    /// its parts, execute the rest identically.
    #[test]
    fn a_runner_resumed_from_its_parts_matches_the_original() {
        let signer = key(12);
        let mut original = runner(RecordingApp::default(), 100);
        let mut inputs = [
            direct(1, 1),
            batch(
                3,
                0,
                vec![frame(2, 0, vec![signed(&signer, 0, 0, vec![5])])],
            ),
            direct(4, 2),
            direct(4, 3),
        ];
        for (index, input) in inputs.iter_mut().enumerate() {
            input.index = index as u64;
            original.process_input(input.clone()).expect("advance");
        }
        let export = original.export();
        let state: SchedulerState = export.scheduler_state();
        let queued = inputs[state.first_queued_input as usize..].to_vec();
        let scheduler =
            Scheduler::resume(*original.scheduler().config(), state, queued).expect("resume");
        let app = RecordingApp {
            events: original.app().events.clone(),
            nonces: original.app().nonces.clone(),
            count: ExecutedInputCount::new(export.l2_tx_count),
            ..RecordingApp::default()
        };
        let mut resumed = CanonicalRunner::from_parts(scheduler, app);
        let rest = [
            batch(
                6,
                1,
                vec![frame(4, 0, vec![signed(&signer, 1, 0, vec![6])])],
            ),
            direct(7, 4),
            batch(9, 1, vec![empty_frame(8)]),
        ];
        for input in rest {
            assert_eq!(
                advance(&mut resumed, input.clone()).expect("resumed"),
                advance(&mut original, input).expect("original")
            );
        }
        assert_eq!(resumed.export(), original.export());
        let (_, app) = resumed.into_parts();
        assert_eq!(
            app.events,
            [
                Event::Direct(1),
                Event::UserOp(5),
                Event::Direct(2),
                Event::Direct(3),
                Event::UserOp(6),
            ]
        );
    }

    #[test]
    fn a_rejected_batch_still_reports_its_backstop_outputs() {
        let mut runner = runner(RecordingApp::default(), 5);
        advance(&mut runner, direct(1, 1)).expect("queue");
        let advanced = advance(&mut runner, batch(8, 0, vec![empty_frame(8)])).expect("advance");
        assert_eq!(
            advanced.outcome,
            InputOutcome::BatchRejected(BatchRejectReason::SafeBlockNotBelowInclusionBlock)
        );
        assert_eq!(advanced.outputs, [AppOutput::Notice(vec![b'd', 1])]);
        assert!(matches!(
            advanced.txs.as_slice(),
            [TxRecord {
                scheduled: Scheduled {
                    batch_nonce: 0,
                    tx: L2Tx::Direct(_)
                },
                fate: TxFate::Done { .. }
            }]
        ));
    }
}
