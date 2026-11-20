// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Reads a stored canonical machine's export: the `scheduler` and `state`
//! inspect reports. Setup's base comes through here. The machine is loaded
//! privately (every file mapped copy-on-write), so the stored directory is
//! never written; the queries run on the in-memory copy, which is dropped.
//! `docs/cartesi-machine.md` ("Reading a stored machine's export") has the
//! emulator facts this relies on.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use cartesi_machine::cartesi_machine_sys::{
    CM_HTIF_DEV_YIELD, CM_HTIF_YIELD_CMD_MANUAL, CM_REG_HTIF_TOHOST_CMD, CM_REG_HTIF_TOHOST_DEV,
    CM_REG_HTIF_TOHOST_REASON, CM_REG_IFLAGS_H, CM_REG_IFLAGS_Y, CM_REG_MCYCLE,
};
use cartesi_machine::config::runtime::RuntimeConfig;
use cartesi_machine::constants::{break_reason, cmio::tohost};
use cartesi_machine::error::MachineError;
use cartesi_machine::types::cmio::{
    AutomaticReason, CmioRequest, CmioResponseReason, ManualReason,
};
use cartesi_machine::types::{BreakReason, Register};
use sequencer_core::scheduler::{
    SCHEDULER_INSPECT_QUERY, STATE_INSPECT_QUERY, SchedulerExport, SchedulerExportLengthError,
};
use thiserror::Error;

/// Cycles one query may run before the machine counts as stuck: minutes of
/// emulation, far past what encoding a report that fits the 2 MiB TX buffer
/// costs. It bounds a guest that never answers.
const QUERY_MCYCLE_BUDGET: u64 = 1 << 36;

/// The longest exception message an error carries; the guest's can be 2 MiB.
const MAX_EXCEPTION_MESSAGE_CHARS: usize = 512;

#[derive(Debug, Error)]
pub enum MachineExportError {
    #[error("cannot load the machine at {path}: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: MachineError,
    },
    #[error("the machine at {path} is not stopped between inputs: {state}")]
    NotBetweenInputs { path: PathBuf, state: String },
    #[error("the emulator failed while the machine answered the `{query}` query: {source}")]
    Emulator {
        query: &'static str,
        #[source]
        source: MachineError,
    },
    #[error("the machine did not answer the `{query}` query: {reason}")]
    Unanswered { query: &'static str, reason: String },
    #[error("the machine answered the `{query}` query with {count} reports; expected one")]
    ReportCount { query: &'static str, count: usize },
    #[error("the machine emitted an output while answering the `{query}` query")]
    OutputEmitted { query: &'static str },
    #[error("the machine's `scheduler` report is malformed: {0}")]
    MalformedExport(#[from] SchedulerExportLengthError),
    #[error("cannot write the machine's state to {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Loads the stored machine at `machine_dir`, answers its `scheduler` query
/// and writes its `state` report to `state_file`, replacing that file. The
/// directory itself is only read. Blocking and `!Send` inside: an async
/// caller runs it on a blocking thread.
pub fn read_machine_export(
    machine_dir: &Path,
    state_file: &Path,
) -> Result<SchedulerExport, MachineExportError> {
    let mut machine = cartesi_machine::Machine::load(machine_dir, &RuntimeConfig::quiet_console())
        .map_err(|source| MachineExportError::Load {
            path: machine_dir.to_path_buf(),
            source,
        })?;
    export(&mut machine, machine_dir, state_file)
}

/// The emulator calls the loader makes. A seam so that every failure arm can
/// be driven by a scripted machine; the flag and counter reads go through
/// `read_reg`, exactly as the bindings' own getters do.
trait Emulator {
    fn read_reg(&mut self, reg: Register) -> Result<u64, MachineError>;
    fn send_cmio_response(
        &mut self,
        reason: CmioResponseReason,
        data: &[u8],
    ) -> Result<(), MachineError>;
    fn run(&mut self, mcycle_end: u64) -> Result<BreakReason, MachineError>;
    fn receive_cmio_request(&mut self) -> Result<CmioRequest, MachineError>;

    fn iflags_h(&mut self) -> Result<bool, MachineError> {
        Ok(self.read_reg(CM_REG_IFLAGS_H)? != 0)
    }
    fn iflags_y(&mut self) -> Result<bool, MachineError> {
        Ok(self.read_reg(CM_REG_IFLAGS_Y)? != 0)
    }
    fn mcycle(&mut self) -> Result<u64, MachineError> {
        self.read_reg(CM_REG_MCYCLE)
    }
}

impl Emulator for cartesi_machine::Machine {
    fn read_reg(&mut self, reg: Register) -> Result<u64, MachineError> {
        cartesi_machine::Machine::read_reg(self, reg)
    }
    fn send_cmio_response(
        &mut self,
        reason: CmioResponseReason,
        data: &[u8],
    ) -> Result<(), MachineError> {
        cartesi_machine::Machine::send_cmio_response(self, reason, data, None)
    }
    fn run(&mut self, mcycle_end: u64) -> Result<BreakReason, MachineError> {
        cartesi_machine::Machine::run(self, mcycle_end)
    }
    fn receive_cmio_request(&mut self) -> Result<CmioRequest, MachineError> {
        cartesi_machine::Machine::receive_cmio_request(self)
    }
}

fn export(
    machine: &mut impl Emulator,
    machine_dir: &Path,
    state_file: &Path,
) -> Result<SchedulerExport, MachineExportError> {
    let not_between = between_inputs(machine).map_err(|source| MachineExportError::Load {
        path: machine_dir.to_path_buf(),
        source,
    })?;
    if let Some(state) = not_between {
        return Err(MachineExportError::NotBetweenInputs {
            path: machine_dir.to_path_buf(),
            state,
        });
    }

    let export = SchedulerExport::decode(&inspect(machine, "scheduler", SCHEDULER_INSPECT_QUERY)?)?;
    let state = inspect(machine, "state", STATE_INSPECT_QUERY)?;
    write_state(state_file, &state)?;
    Ok(export)
}

/// Why the machine is not at an `RX_ACCEPTED` manual yield, if it is not,
/// read as the step reads it (the flags, then tohost's device, command and
/// reason), so a malformed yield is an answer, never a panic in the bindings.
fn between_inputs(machine: &mut impl Emulator) -> Result<Option<String>, MachineError> {
    if machine.iflags_h()? {
        return Ok(Some("it is halted".to_owned()));
    }
    if !machine.iflags_y()? {
        return Ok(Some("it is not at a manual yield".to_owned()));
    }
    let (dev, cmd, reason) = tohost_header(machine)?;
    if dev != u64::from(CM_HTIF_DEV_YIELD) || cmd != u64::from(CM_HTIF_YIELD_CMD_MANUAL) {
        return Ok(Some(format!(
            "its pending yield has device {dev} and command {cmd}"
        )));
    }
    if reason != u64::from(tohost::manual::RX_ACCEPTED) {
        return Ok(Some(format!(
            "its pending manual yield has reason {reason}, not rx-accepted"
        )));
    }
    Ok(None)
}

fn tohost_header(machine: &mut impl Emulator) -> Result<(u64, u64, u64), MachineError> {
    Ok((
        machine.read_reg(CM_REG_HTIF_TOHOST_DEV)?,
        machine.read_reg(CM_REG_HTIF_TOHOST_CMD)?,
        machine.read_reg(CM_REG_HTIF_TOHOST_REASON)?,
    ))
}

/// Sends one inspect query and runs the machine back to `RX_ACCEPTED`. The
/// guest answers with exactly one report; a second one fails at once, so a
/// runaway guest cannot grow the host's memory.
fn inspect(
    machine: &mut impl Emulator,
    name: &'static str,
    query: &[u8],
) -> Result<Vec<u8>, MachineExportError> {
    let emulator = |source| MachineExportError::Emulator {
        query: name,
        source,
    };
    let unanswered = |reason: String| MachineExportError::Unanswered {
        query: name,
        reason,
    };

    machine
        .send_cmio_response(CmioResponseReason::Inspect, query)
        .map_err(emulator)?;
    // An undeliverable response is a silent no-op; only delivery clears Y.
    if machine.iflags_y().map_err(emulator)? {
        return Err(unanswered("the machine did not take the query".to_owned()));
    }

    let mcycle_end = machine
        .mcycle()
        .map_err(emulator)?
        .checked_add(QUERY_MCYCLE_BUDGET)
        .ok_or_else(|| unanswered("its mcycle is at the end of the counter".to_owned()))?;
    let mut report = None;
    loop {
        match machine.run(mcycle_end).map_err(emulator)? {
            break_reason::YIELDED_AUTOMATICALLY => {
                let reason = machine
                    .read_reg(CM_REG_HTIF_TOHOST_REASON)
                    .map_err(emulator)?;
                if reason == u64::from(tohost::automatic::PROGRESS) {
                    continue;
                }
                if reason == u64::from(tohost::automatic::TX_OUTPUT) {
                    return Err(MachineExportError::OutputEmitted { query: name });
                }
                if reason != u64::from(tohost::automatic::TX_REPORT) {
                    return Err(unanswered(format!(
                        "it yielded automatically with unknown reason {reason}"
                    )));
                }
                if report.is_some() {
                    return Err(MachineExportError::ReportCount {
                        query: name,
                        count: 2,
                    });
                }
                match machine.receive_cmio_request().map_err(emulator)? {
                    CmioRequest::Automatic(AutomaticReason::TxReport { data }) => {
                        report = Some(data)
                    }
                    _ => return Err(unanswered("its report could not be read".to_owned())),
                }
            }
            break_reason::YIELDED_MANUALLY => {
                let reason = machine
                    .read_reg(CM_REG_HTIF_TOHOST_REASON)
                    .map_err(emulator)?;
                if reason == u64::from(tohost::manual::RX_ACCEPTED) {
                    return report.ok_or(MachineExportError::ReportCount {
                        query: name,
                        count: 0,
                    });
                }
                return Err(unanswered(manual_yield(machine, reason).map_err(emulator)?));
            }
            // A soft yield only hands control back; the next run resumes.
            break_reason::YIELDED_SOFTLY => {}
            break_reason::HALTED => return Err(unanswered("the machine halted".to_owned())),
            break_reason::MCYCLE_OVERFLOW => {
                return Err(unanswered(
                    "the machine exhausted its cycle budget".to_owned(),
                ));
            }
            break_reason::REACHED_TARGET_MCYCLE => {
                return Err(unanswered(format!(
                    "no answer within {QUERY_MCYCLE_BUDGET} cycles"
                )));
            }
            other => return Err(unanswered(format!("the run broke with reason {other}"))),
        }
    }
}

/// Describes a manual yield other than `RX_ACCEPTED`.
fn manual_yield(machine: &mut impl Emulator, reason: u64) -> Result<String, MachineError> {
    if reason == u64::from(tohost::manual::RX_REJECTED) {
        return Ok("the guest rejected the query".to_owned());
    }
    if reason == u64::from(tohost::manual::TX_EXCEPTION) {
        let message = match machine.receive_cmio_request()? {
            CmioRequest::Manual(ManualReason::TxException { message }) => message,
            _ => "unreadable".to_owned(),
        };
        let message: String = message.chars().take(MAX_EXCEPTION_MESSAGE_CHARS).collect();
        return Ok(format!("the guest raised an exception: {message}"));
    }
    Ok(format!("the guest yielded manually with reason {reason}"))
}

fn write_state(path: &Path, state: &[u8]) -> Result<(), MachineExportError> {
    let io = |source| MachineExportError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::create(path).map_err(io)?;
    file.write_all(state).map_err(io)?;
    file.flush().map_err(io)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use cartesi_machine::cartesi_machine_sys::{CM_HTIF_DEV_CONSOLE, CM_HTIF_YIELD_CMD_AUTOMATIC};

    use super::*;

    const MACHINE_DIR: &str = "/stored/machine";
    const MCYCLE: u64 = 1_000;
    const STATE: &[u8] = b"the application's state bytes";

    /// One `run` of the scripted machine: how it breaks, the tohost reason it
    /// leaves, and the request `receive_cmio_request` then returns.
    struct Step {
        break_reason: BreakReason,
        tohost_reason: u16,
        request: Option<CmioRequest>,
    }

    fn manual(reason: u16, request: Option<CmioRequest>) -> Step {
        Step {
            break_reason: break_reason::YIELDED_MANUALLY,
            tohost_reason: reason,
            request,
        }
    }

    fn automatic(reason: u16, request: Option<CmioRequest>) -> Step {
        Step {
            break_reason: break_reason::YIELDED_AUTOMATICALLY,
            tohost_reason: reason,
            request,
        }
    }

    fn broke(reason: BreakReason) -> Step {
        Step {
            break_reason: reason,
            tohost_reason: 0,
            request: None,
        }
    }

    fn accepted() -> Step {
        manual(tohost::manual::RX_ACCEPTED, None)
    }

    fn report(data: &[u8]) -> Step {
        automatic(
            tohost::automatic::TX_REPORT,
            Some(CmioRequest::Automatic(AutomaticReason::TxReport {
                data: data.to_vec(),
            })),
        )
    }

    fn exception(message: &str) -> Step {
        manual(
            tohost::manual::TX_EXCEPTION,
            Some(CmioRequest::Manual(ManualReason::TxException {
                message: message.to_owned(),
            })),
        )
    }

    fn sample_export() -> SchedulerExport {
        SchedulerExport {
            l1_input_count: 7,
            l2_tx_count: 40,
            next_batch_nonce: 3,
            clock: 28,
            first_queued_input: 6,
            fridge_len: 1,
        }
    }

    /// The `scheduler` query answered with the sample export; the `state`
    /// query's steps follow.
    fn scheduler_answered() -> Vec<Step> {
        vec![report(&sample_export().encode()), accepted()]
    }

    fn both_answered() -> Vec<Step> {
        let mut steps = scheduler_answered();
        steps.extend([report(STATE), accepted()]);
        steps
    }

    /// A machine that plays a script: its registers at load, then one step
    /// per `run`. It records what the loader sent and the run targets.
    struct Scripted {
        halted: bool,
        yielded: bool,
        tohost: [u64; 3],
        mcycle: u64,
        takes_queries: bool,
        failing_reg: Option<Register>,
        /// The call that fails, and on which of its calls, counting from 0.
        failing_call: Option<(Call, usize)>,
        calls: Vec<Call>,
        steps: VecDeque<Step>,
        pending: Option<CmioRequest>,
        queries: Vec<Vec<u8>>,
        run_targets: Vec<u64>,
    }

    impl Scripted {
        /// Stopped at an `RX_ACCEPTED` manual yield, as a stored canonical
        /// machine is.
        fn between_inputs(steps: Vec<Step>) -> Self {
            Self {
                halted: false,
                yielded: true,
                tohost: [
                    u64::from(CM_HTIF_DEV_YIELD),
                    u64::from(CM_HTIF_YIELD_CMD_MANUAL),
                    u64::from(tohost::manual::RX_ACCEPTED),
                ],
                mcycle: MCYCLE,
                takes_queries: true,
                failing_reg: None,
                failing_call: None,
                calls: Vec::new(),
                steps: steps.into(),
                pending: None,
                queries: Vec::new(),
                run_targets: Vec::new(),
            }
        }

        fn queries(&self) -> Vec<&[u8]> {
            self.queries.iter().map(Vec::as_slice).collect()
        }

        /// Records `call`, and fails it when the script says so.
        fn call(&mut self, call: Call) -> Result<(), MachineError> {
            let nth = self.calls.iter().filter(|made| **made == call).count();
            self.calls.push(call);
            if self.failing_call == Some((call, nth)) {
                return Err(MachineError {
                    code: -1,
                    message: format!("{call:?} failed"),
                });
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        SendCmioResponse,
        Run,
        ReceiveCmioRequest,
    }

    impl Emulator for Scripted {
        fn read_reg(&mut self, reg: Register) -> Result<u64, MachineError> {
            if self.failing_reg == Some(reg) {
                return Err(MachineError {
                    code: -1,
                    message: format!("register {reg} is unreadable"),
                });
            }
            Ok(match reg {
                CM_REG_IFLAGS_H => u64::from(self.halted),
                CM_REG_IFLAGS_Y => u64::from(self.yielded),
                CM_REG_HTIF_TOHOST_DEV => self.tohost[0],
                CM_REG_HTIF_TOHOST_CMD => self.tohost[1],
                CM_REG_HTIF_TOHOST_REASON => self.tohost[2],
                CM_REG_MCYCLE => self.mcycle,
                other => panic!("the loader reads no register {other}"),
            })
        }

        fn send_cmio_response(
            &mut self,
            reason: CmioResponseReason,
            data: &[u8],
        ) -> Result<(), MachineError> {
            assert_eq!(reason, CmioResponseReason::Inspect);
            self.call(Call::SendCmioResponse)?;
            self.queries.push(data.to_vec());
            if self.yielded && self.takes_queries {
                self.yielded = false;
            }
            Ok(())
        }

        fn run(&mut self, mcycle_end: u64) -> Result<BreakReason, MachineError> {
            assert!(!self.yielded, "a run starts only after the query is taken");
            self.call(Call::Run)?;
            self.run_targets.push(mcycle_end);
            let step = self.steps.pop_front().expect("the script has another step");
            self.tohost[2] = u64::from(step.tohost_reason);
            self.pending = step.request;
            match step.break_reason {
                break_reason::YIELDED_MANUALLY => {
                    self.yielded = true;
                    self.tohost[1] = u64::from(CM_HTIF_YIELD_CMD_MANUAL);
                }
                break_reason::YIELDED_AUTOMATICALLY => {
                    self.tohost[1] = u64::from(CM_HTIF_YIELD_CMD_AUTOMATIC);
                }
                _ => {}
            }
            Ok(step.break_reason)
        }

        fn receive_cmio_request(&mut self) -> Result<CmioRequest, MachineError> {
            self.call(Call::ReceiveCmioRequest)?;
            Ok(self.pending.take().expect("the step scripted a request"))
        }
    }

    /// Runs the loader on `machine`; returns its result and the state file's
    /// bytes afterwards, which start as a stale file the loader must replace.
    fn run_export(
        machine: &mut Scripted,
    ) -> (Result<SchedulerExport, MachineExportError>, Vec<u8>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("state");
        std::fs::write(&state_file, b"stale").expect("stale state");
        let result = export(machine, Path::new(MACHINE_DIR), &state_file);
        let state = std::fs::read(&state_file).expect("state file");
        (result, state)
    }

    fn not_between(machine: &mut Scripted) -> String {
        let (result, state) = run_export(machine);
        assert_eq!(state, b"stale", "a refused machine writes no state");
        assert!(
            machine.queries.is_empty(),
            "a refused machine gets no query"
        );
        match result {
            Err(MachineExportError::NotBetweenInputs { path, state }) => {
                assert_eq!(path, Path::new(MACHINE_DIR));
                state
            }
            other => panic!("expected NotBetweenInputs, got {other:?}"),
        }
    }

    /// The loader's error for a script that fails during a query; no state
    /// is written.
    fn failed(machine: &mut Scripted) -> MachineExportError {
        let (result, state) = run_export(machine);
        assert_eq!(state, b"stale", "a failed query writes no state");
        result.expect_err("the script fails a query")
    }

    fn unanswered(steps: Vec<Step>) -> (&'static str, String) {
        match failed(&mut Scripted::between_inputs(steps)) {
            MachineExportError::Unanswered { query, reason } => (query, reason),
            other => panic!("expected Unanswered, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_machine_is_a_load_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-machine");
        let state_file = dir.path().join("state");

        let err = read_machine_export(&missing, &state_file).expect_err("nothing to load");

        assert!(
            matches!(&err, MachineExportError::Load { path, .. } if *path == missing),
            "{err}"
        );
        assert!(!state_file.exists(), "a failed load writes no state");
    }

    #[test]
    fn a_machine_between_inputs_answers_both_queries_and_its_state_replaces_the_file() {
        let mut machine = Scripted::between_inputs(both_answered());

        let (result, state) = run_export(&mut machine);

        assert_eq!(result.expect("export"), sample_export());
        assert_eq!(state, STATE);
        assert_eq!(
            machine.queries(),
            [SCHEDULER_INSPECT_QUERY, STATE_INSPECT_QUERY]
        );
        let target = MCYCLE + QUERY_MCYCLE_BUDGET;
        assert_eq!(machine.run_targets, [target; 4]);
        assert!(machine.steps.is_empty());
    }

    #[test]
    fn progress_and_soft_yields_are_passed_over() {
        let progress = || {
            automatic(
                tohost::automatic::PROGRESS,
                Some(CmioRequest::Automatic(AutomaticReason::Progress {
                    mille_progress: 500,
                })),
            )
        };
        let mut machine = Scripted::between_inputs(vec![
            progress(),
            broke(break_reason::YIELDED_SOFTLY),
            report(&sample_export().encode()),
            broke(break_reason::YIELDED_SOFTLY),
            progress(),
            accepted(),
            report(STATE),
            accepted(),
        ]);

        let (result, state) = run_export(&mut machine);

        assert_eq!(result.expect("export"), sample_export());
        assert_eq!(state, STATE);
        assert!(machine.steps.is_empty());
    }

    #[test]
    fn a_halted_machine_is_not_between_inputs() {
        let mut machine = Scripted::between_inputs(Vec::new());
        machine.halted = true;
        assert_eq!(not_between(&mut machine), "it is halted");
    }

    #[test]
    fn a_machine_not_at_a_manual_yield_is_not_between_inputs() {
        let mut machine = Scripted::between_inputs(Vec::new());
        machine.yielded = false;
        assert_eq!(not_between(&mut machine), "it is not at a manual yield");
    }

    #[test]
    fn a_pending_yield_with_another_device_or_command_is_not_between_inputs() {
        let mut machine = Scripted::between_inputs(Vec::new());
        machine.tohost[0] = u64::from(CM_HTIF_DEV_CONSOLE);
        assert_eq!(
            not_between(&mut machine),
            format!(
                "its pending yield has device {CM_HTIF_DEV_CONSOLE} and command {CM_HTIF_YIELD_CMD_MANUAL}"
            )
        );

        let mut machine = Scripted::between_inputs(Vec::new());
        machine.tohost[1] = u64::from(CM_HTIF_YIELD_CMD_AUTOMATIC);
        assert_eq!(
            not_between(&mut machine),
            format!(
                "its pending yield has device {CM_HTIF_DEV_YIELD} and command {CM_HTIF_YIELD_CMD_AUTOMATIC}"
            )
        );
    }

    #[test]
    fn a_manual_yield_with_another_reason_is_not_between_inputs() {
        for reason in [tohost::manual::RX_REJECTED, tohost::manual::TX_EXCEPTION] {
            let mut machine = Scripted::between_inputs(Vec::new());
            machine.tohost[2] = u64::from(reason);
            assert_eq!(
                not_between(&mut machine),
                format!("its pending manual yield has reason {reason}, not rx-accepted")
            );
        }
    }

    #[test]
    fn an_unreadable_register_at_load_is_a_load_error() {
        let mut machine = Scripted::between_inputs(Vec::new());
        machine.failing_reg = Some(CM_REG_HTIF_TOHOST_REASON);

        let err = failed(&mut machine);

        assert!(
            matches!(&err, MachineExportError::Load { path, .. } if path == Path::new(MACHINE_DIR)),
            "{err}"
        );
        assert!(machine.queries.is_empty());
    }

    #[test]
    fn an_unreadable_register_during_a_query_is_an_emulator_error() {
        let mut machine = Scripted::between_inputs(Vec::new());
        machine.failing_reg = Some(CM_REG_MCYCLE);

        let err = failed(&mut machine);

        assert!(
            matches!(
                err,
                MachineExportError::Emulator {
                    query: "scheduler",
                    ..
                }
            ),
            "{err}"
        );
    }

    /// A failing emulator call is an emulator error naming the query it
    /// served, and writes no state.
    #[test]
    fn a_failing_emulator_call_is_an_emulator_error() {
        let cases = [
            (Call::SendCmioResponse, 0, "scheduler", both_answered()),
            (Call::Run, 0, "scheduler", both_answered()),
            (Call::ReceiveCmioRequest, 0, "scheduler", both_answered()),
            (Call::SendCmioResponse, 1, "state", both_answered()),
            (Call::Run, 2, "state", both_answered()),
            (Call::ReceiveCmioRequest, 1, "state", both_answered()),
            // The exception's message is read inside the manual yield's arm.
            (
                Call::ReceiveCmioRequest,
                0,
                "scheduler",
                vec![exception("out of gas")],
            ),
        ];
        for (call, nth, query, steps) in cases {
            let mut machine = Scripted::between_inputs(steps);
            machine.failing_call = Some((call, nth));

            let err = failed(&mut machine);

            assert!(
                matches!(&err, MachineExportError::Emulator { query: failed_query, .. }
                    if *failed_query == query),
                "{call:?} #{nth}: {err}"
            );
        }
    }

    #[test]
    fn a_query_the_machine_does_not_take_is_unanswered() {
        let mut machine = Scripted::between_inputs(Vec::new());
        machine.takes_queries = false;

        let err = failed(&mut machine);

        assert!(
            matches!(&err, MachineExportError::Unanswered { query: "scheduler", reason }
                if reason == "the machine did not take the query"),
            "{err}"
        );
        assert!(machine.run_targets.is_empty(), "nothing runs");
    }

    #[test]
    fn an_output_during_a_query_fails_it() {
        let output = automatic(
            tohost::automatic::TX_OUTPUT,
            Some(CmioRequest::Automatic(AutomaticReason::TxOutput {
                data: vec![1],
            })),
        );
        let err = failed(&mut Scripted::between_inputs(vec![output]));
        assert!(
            matches!(
                err,
                MachineExportError::OutputEmitted { query: "scheduler" }
            ),
            "{err}"
        );
    }

    #[test]
    fn an_automatic_yield_with_an_unknown_reason_is_unanswered() {
        // The request is never received: the bindings panic on an unknown reason.
        let (query, reason) = unanswered(vec![automatic(7, None)]);
        assert_eq!(query, "scheduler");
        assert_eq!(reason, "it yielded automatically with unknown reason 7");
    }

    /// The pinned bindings decode the request's variant from tohost, so a
    /// report yield always reads as a report; this arm guards a change in
    /// the bindings, and the script fakes a request they cannot return.
    #[test]
    fn a_report_that_cannot_be_read_is_unanswered() {
        let not_a_report = automatic(
            tohost::automatic::TX_REPORT,
            Some(CmioRequest::Manual(ManualReason::RxRejected)),
        );
        let (_, reason) = unanswered(vec![not_a_report]);
        assert_eq!(reason, "its report could not be read");
    }

    #[test]
    fn a_second_report_fails_the_query_at_once() {
        let mut machine = Scripted::between_inputs(vec![report(b"one"), report(b"two")]);

        let err = failed(&mut machine);

        assert!(
            matches!(
                err,
                MachineExportError::ReportCount {
                    query: "scheduler",
                    count: 2
                }
            ),
            "{err}"
        );
        assert_eq!(
            machine.pending,
            Some(CmioRequest::Automatic(AutomaticReason::TxReport {
                data: b"two".to_vec()
            })),
            "the second report is never read"
        );
    }

    #[test]
    fn no_report_fails_the_query() {
        let err = failed(&mut Scripted::between_inputs(vec![accepted()]));
        assert!(
            matches!(
                err,
                MachineExportError::ReportCount {
                    query: "scheduler",
                    count: 0
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_rejected_query_is_unanswered() {
        let (_, reason) = unanswered(vec![manual(tohost::manual::RX_REJECTED, None)]);
        assert_eq!(reason, "the guest rejected the query");
    }

    #[test]
    fn an_exception_message_is_cut_at_its_character_cap() {
        let long = "é".repeat(MAX_EXCEPTION_MESSAGE_CHARS + 100);
        let (_, reason) = unanswered(vec![exception(&long)]);
        let kept = "é".repeat(MAX_EXCEPTION_MESSAGE_CHARS);
        assert_eq!(reason, format!("the guest raised an exception: {kept}"));

        let (_, reason) = unanswered(vec![exception("out of gas")]);
        assert_eq!(reason, "the guest raised an exception: out of gas");
    }

    /// As for a report: the pinned bindings always read an exception yield as
    /// an exception, so this arm guards a change in the bindings.
    #[test]
    fn an_exception_that_cannot_be_read_is_described_as_unreadable() {
        let garbled = manual(
            tohost::manual::TX_EXCEPTION,
            Some(CmioRequest::Manual(ManualReason::RxRejected)),
        );
        let (_, reason) = unanswered(vec![garbled]);
        assert_eq!(reason, "the guest raised an exception: unreadable");
    }

    #[test]
    fn a_manual_yield_with_another_reason_during_a_query_is_unanswered() {
        let gio = manual(
            0x10,
            Some(CmioRequest::Manual(ManualReason::GIO {
                domain: 0x10,
                data: Vec::new(),
            })),
        );
        let (_, reason) = unanswered(vec![gio]);
        assert_eq!(reason, "the guest yielded manually with reason 16");
    }

    #[test]
    fn a_halt_during_a_query_is_unanswered() {
        let (_, reason) = unanswered(vec![report(b"partial"), broke(break_reason::HALTED)]);
        assert_eq!(reason, "the machine halted");
    }

    #[test]
    fn an_exhausted_cycle_budget_is_unanswered() {
        let (_, reason) = unanswered(vec![broke(break_reason::REACHED_TARGET_MCYCLE)]);
        assert_eq!(
            reason,
            format!("no answer within {QUERY_MCYCLE_BUDGET} cycles")
        );

        let (_, reason) = unanswered(vec![broke(break_reason::MCYCLE_OVERFLOW)]);
        assert_eq!(reason, "the machine exhausted its cycle budget");

        let (_, reason) = unanswered(vec![broke(break_reason::FAILED)]);
        assert_eq!(
            reason,
            format!("the run broke with reason {}", break_reason::FAILED)
        );
    }

    #[test]
    fn a_mcycle_at_the_end_of_the_counter_is_unanswered() {
        let mut machine = Scripted::between_inputs(Vec::new());
        machine.mcycle = u64::MAX - QUERY_MCYCLE_BUDGET + 1;

        let err = failed(&mut machine);

        assert!(
            matches!(&err, MachineExportError::Unanswered { query: "scheduler", reason }
                if reason == "its mcycle is at the end of the counter"),
            "{err}"
        );
        assert!(machine.run_targets.is_empty(), "nothing runs");
    }

    #[test]
    fn the_budget_may_end_on_the_counters_last_value() {
        let mut machine = Scripted::between_inputs(both_answered());
        machine.mcycle = u64::MAX - QUERY_MCYCLE_BUDGET;

        let (result, _) = run_export(&mut machine);

        assert_eq!(result.expect("export"), sample_export());
        assert!(machine.run_targets.iter().all(|&target| target == u64::MAX));
    }

    #[test]
    fn a_scheduler_report_of_the_wrong_length_is_malformed() {
        let short = &sample_export().encode()[..SchedulerExport::ENCODED_LEN - 1];
        let mut machine = Scripted::between_inputs(vec![report(short), accepted()]);

        let err = failed(&mut machine);

        assert!(
            matches!(
                err,
                MachineExportError::MalformedExport(SchedulerExportLengthError { got })
                    if got == SchedulerExport::ENCODED_LEN - 1
            ),
            "{err}"
        );
        assert_eq!(
            machine.queries(),
            [SCHEDULER_INSPECT_QUERY],
            "no state query"
        );
    }

    #[test]
    fn a_failed_state_query_names_it_and_writes_no_state() {
        let mut steps = scheduler_answered();
        steps.push(accepted());
        let mut machine = Scripted::between_inputs(steps);

        let err = failed(&mut machine);

        assert!(
            matches!(
                err,
                MachineExportError::ReportCount {
                    query: "state",
                    count: 0
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_state_file_that_cannot_be_written_is_an_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("missing-dir").join("state");
        let mut machine = Scripted::between_inputs(both_answered());

        let err = export(&mut machine, Path::new(MACHINE_DIR), &state_file)
            .expect_err("no directory to write into");

        assert!(
            matches!(&err, MachineExportError::Io { path, .. } if *path == state_file),
            "{err}"
        );
    }
}
