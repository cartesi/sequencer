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
    CM_REG_HTIF_TOHOST_REASON,
};
use cartesi_machine::config::runtime::RuntimeConfig;
use cartesi_machine::constants::{break_reason, cmio::tohost};
use cartesi_machine::error::MachineError;
use cartesi_machine::types::cmio::{
    AutomaticReason, CmioRequest, CmioResponseReason, ManualReason,
};
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
    let not_between = between_inputs(&mut machine).map_err(|source| MachineExportError::Load {
        path: machine_dir.to_path_buf(),
        source,
    })?;
    if let Some(state) = not_between {
        return Err(MachineExportError::NotBetweenInputs {
            path: machine_dir.to_path_buf(),
            state,
        });
    }

    let export = SchedulerExport::decode(&inspect(
        &mut machine,
        "scheduler",
        SCHEDULER_INSPECT_QUERY,
    )?)?;
    let state = inspect(&mut machine, "state", STATE_INSPECT_QUERY)?;
    write_state(state_file, &state)?;
    Ok(export)
}

/// Why the machine is not at an `RX_ACCEPTED` manual yield, if it is not,
/// read as the step reads it (the flags, then tohost's device, command and
/// reason), so a malformed yield is an answer, never a panic in the bindings.
fn between_inputs(machine: &mut cartesi_machine::Machine) -> Result<Option<String>, MachineError> {
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

fn tohost_header(machine: &mut cartesi_machine::Machine) -> Result<(u64, u64, u64), MachineError> {
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
    machine: &mut cartesi_machine::Machine,
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
        .send_cmio_response(CmioResponseReason::Inspect, query, None)
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
fn manual_yield(
    machine: &mut cartesi_machine::Machine,
    reason: u64,
) -> Result<String, MachineError> {
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
    use super::*;

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
}
