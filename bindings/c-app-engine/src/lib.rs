// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! An exclusively owned application engine behind the C ABI in `application-engine.h`.

pub mod sys;

#[cfg(test)]
mod tests;

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use alloy_primitives::{Address, U256};
use sequencer_core::application::{
    Admission, AppError, AppOutput, AppOutputs, PolicyRefusal, SchedulerLimits,
};
use sequencer_core::history::L2TxCount;
use sequencer_core::l2_tx::{Direct, UserOpTx};
use sequencer_core::user_op::UserOp;

pub use sequencer_core::application::Application;

fn path_to_cstring(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes())
        .unwrap_or_else(|_| panic!("path contains an interior NUL: {}", path.display()))
}

fn abi_address(address: Address) -> sys::ApplicationEngineEthereumAddress {
    sys::ApplicationEngineEthereumAddress {
        bytes: address.into_array(),
    }
}

fn abi_span(bytes: &[u8]) -> sys::ApplicationEngineByteSpan {
    sys::ApplicationEngineByteSpan {
        data: bytes.as_ptr(),
        size: u64::try_from(bytes.len()).expect("length exceeds the ABI's u64 width"),
    }
}

// The engine retains each payload until its next drain call, so copy before calling again.
fn payload_from(span: sys::ApplicationEngineByteSpan) -> Vec<u8> {
    if span.size == 0 {
        return Vec::new();
    }
    assert!(
        !span.data.is_null(),
        "non-empty output payload has a null pointer"
    );
    let len = usize::try_from(span.size).expect("output length exceeds usize");
    unsafe { std::slice::from_raw_parts(span.data, len) }.to_vec()
}

fn last_error_message() -> String {
    let message = unsafe { sys::application_engine_get_last_error_message() };
    assert!(!message.is_null(), "engine returned a null error message");
    unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

fn internal(reason: impl Into<String>) -> AppError {
    AppError::Internal {
        reason: reason.into(),
    }
}

/// Refuse an engine built against another header. A changed meaning, such as
/// an execution that must no longer fail, links without complaint, so only the
/// reported version tells the two apart.
fn abi_version_matches(reported: u32) -> Result<(), AppError> {
    if reported == sys::APPLICATION_ENGINE_ABI_VERSION {
        return Ok(());
    }
    Err(internal(format!(
        "linked engine reports ABI version {reported}, but this host speaks version {}",
        sys::APPLICATION_ENGINE_ABI_VERSION
    )))
}

/// Asked once per process, before any other engine call: every handle comes
/// from a constructor that checks it, and the handle-free getters check it too.
fn check_abi_version() -> Result<(), AppError> {
    static CHECKED: OnceLock<Result<(), String>> = OnceLock::new();
    CHECKED
        .get_or_init(|| {
            abi_version_matches(unsafe { sys::application_engine_abi_version() })
                .map_err(|err| err.to_string())
        })
        .clone()
        .map_err(internal)
}

/// The infallible getters have no error to return, and an engine of another
/// version must not be asked anything.
fn assert_abi_version() {
    if let Err(err) = check_abi_version() {
        panic!("{err}");
    }
}

/// Admission's answer. Only the fee-balance and policy reasons are admission's
/// to give; the retired ones, or one this host does not know, break the contract.
fn admission_from(
    status: i32,
    invalid: &sys::ApplicationEngineInvalid,
) -> Result<Admission, AppError> {
    if status != sys::APPLICATION_ENGINE_STATUS_INVALID {
        check(status, "admit_user_op")?;
        return Ok(Admission::Admit);
    }
    let refusal = match invalid.reason {
        sys::APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE => {
            let balance = unsafe { invalid.values.fee_balance };
            PolicyRefusal::Underfunded {
                required: U256::from_be_bytes(balance.required.bytes),
                available: U256::from_be_bytes(balance.available.bytes),
            }
        }
        sys::APPLICATION_ENGINE_REFUSED_BY_POLICY => PolicyRefusal::Other(last_error_message()),
        other => {
            return Err(internal(format!(
                "engine admission reported unsupported invalid reason {other}"
            )));
        }
    };
    Ok(Admission::Refuse(refusal))
}

/// Admission, nonce reads, execution and draining. NOT_FOUND and INVALID_DUMP
/// describe lifecycle failures only, so here they break the contract and are fatal.
fn check(status: i32, operation: &str) -> Result<(), AppError> {
    check_status(status, operation, false)
}

/// `from_dump`, `from_canonical_state` and `create_dump`, where a missing or
/// invalid dump is an operational I/O failure.
fn check_lifecycle(status: i32, operation: &str) -> Result<(), AppError> {
    check_status(status, operation, true)
}

fn check_status(status: i32, operation: &str, lifecycle: bool) -> Result<(), AppError> {
    if status == sys::APPLICATION_ENGINE_STATUS_OK {
        return Ok(());
    }
    let reason = format!(
        "engine {operation} failed (status {status}): {}",
        last_error_message()
    );
    let kind = match status {
        sys::APPLICATION_ENGINE_STATUS_IO_ERROR => std::io::ErrorKind::Other,
        sys::APPLICATION_ENGINE_STATUS_NOT_FOUND if lifecycle => std::io::ErrorKind::NotFound,
        sys::APPLICATION_ENGINE_STATUS_INVALID_DUMP if lifecycle => std::io::ErrorKind::InvalidData,
        _ => return Err(internal(reason)),
    };
    Err(AppError::Io(std::io::Error::new(kind, reason)))
}

/// Owns one engine handle. Calls on that handle never overlap.
pub struct EngineApp {
    engine: *mut sys::ApplicationEngine,
}

// SAFETY: the ABI permits moving a handle between threads. Ownership is exclusive;
// EngineApp is neither Clone nor Sync, and mutation requires &mut self.
unsafe impl Send for EngineApp {}

impl Drop for EngineApp {
    fn drop(&mut self) {
        unsafe { sys::application_engine_destroy(self.engine) };
    }
}

impl EngineApp {
    fn drain_outputs(&mut self, count: u64) -> Result<AppOutputs, AppError> {
        let mut outputs = AppOutputs::new();
        for _ in 0..count {
            let mut output = sys::ApplicationEngineOutput {
                kind: 0,
                values: sys::ApplicationEngineOutputValues {
                    notice: abi_span(&[]),
                },
            };
            check(
                unsafe { sys::application_engine_drain_output(self.engine, &mut output) },
                "drain_output",
            )?;
            outputs.push(match output.kind {
                sys::APPLICATION_ENGINE_OUTPUT_VOUCHER => {
                    let voucher = unsafe { output.values.voucher };
                    AppOutput::Voucher {
                        destination: Address::from(voucher.destination.bytes),
                        value: U256::from_be_bytes(voucher.value.bytes),
                        payload: payload_from(voucher.payload),
                    }
                }
                sys::APPLICATION_ENGINE_OUTPUT_NOTICE => {
                    AppOutput::Notice(payload_from(unsafe { output.values.notice }))
                }
                other => {
                    return Err(internal(format!(
                        "engine reported unknown output kind {other}"
                    )));
                }
            });
        }
        Ok(outputs)
    }

    /// Take the handle a successful constructor wrote.
    fn from_handle(engine: *mut sys::ApplicationEngine) -> Self {
        assert!(
            !engine.is_null(),
            "engine reported successful load without a handle"
        );
        Self { engine }
    }

    /// Run one execution entry point and take the outputs it queued.
    fn execute_with(
        &mut self,
        operation: &str,
        run: impl FnOnce(*mut sys::ApplicationEngine, &mut u64) -> i32,
    ) -> Result<AppOutputs, AppError> {
        let mut count = 0;
        check(run(self.engine, &mut count), operation)?;
        self.drain_outputs(count)
    }
}

impl Application for EngineApp {
    fn user_nonce(&self, sender: Address) -> Result<u32, AppError> {
        let mut nonce = 0;
        check(
            unsafe {
                sys::application_engine_user_nonce(self.engine, &abi_address(sender), &mut nonce)
            },
            "user_nonce",
        )?;
        Ok(nonce)
    }

    fn apply_user_op(&mut self, op: &UserOpTx) -> Result<AppOutputs, AppError> {
        let op = sys::ApplicationEngineUserOpTx {
            sender: abi_address(op.sender),
            nonce: op.nonce,
            fee: op.fee,
            safe_block: op.block,
            data: abi_span(&op.data),
        };
        self.execute_with("execute_user_op", |engine, count| unsafe {
            sys::application_engine_execute_user_op(engine, &op, count)
        })
    }

    fn apply_direct(&mut self, direct: &Direct) -> Result<AppOutputs, AppError> {
        let direct = sys::ApplicationEngineDirectInput {
            sender: abi_address(direct.sender),
            input_index: direct.input_index,
            block_number: direct.block,
            timestamp: direct.timestamp,
            payload: abi_span(&direct.payload),
        };
        self.execute_with("execute_direct_input", |engine, count| unsafe {
            sys::application_engine_execute_direct_input(engine, &direct, count)
        })
    }

    fn l2_tx_count(&self) -> L2TxCount {
        L2TxCount::new(unsafe { sys::application_engine_l2_tx_count(self.engine) })
    }

    fn max_method_payload_bytes() -> usize {
        assert_abi_version();
        usize::try_from(unsafe { sys::application_engine_max_method_payload_bytes() })
            .expect("engine payload bound exceeds usize")
    }

    fn scheduler_limits() -> SchedulerLimits {
        assert_abi_version();
        let mut limits = sys::ApplicationEngineSchedulerLimits {
            fridge_bound: 0,
            max_direct_payload_bytes: 0,
        };
        unsafe { sys::application_engine_scheduler_limits(&mut limits) };
        SchedulerLimits {
            fridge_bound: limits.fridge_bound,
            max_direct_payload_bytes: limits.max_direct_payload_bytes,
        }
    }

    fn admit_user_op(&self, sender: Address, op: &UserOp, fee: u16) -> Result<Admission, AppError> {
        let mut invalid = sys::ApplicationEngineInvalid {
            reason: 0,
            values: sys::ApplicationEngineInvalidValues {
                fee_balance: sys::ApplicationEngineInsufficientFeeBalance {
                    required: sys::ApplicationEngineUint256 { bytes: [0; 32] },
                    available: sys::ApplicationEngineUint256 { bytes: [0; 32] },
                },
            },
        };
        let op = sys::ApplicationEngineUserOp {
            nonce: op.nonce,
            max_fee: op.max_fee,
            data: abi_span(op.data.as_ref()),
        };
        let status = unsafe {
            sys::application_engine_admit_user_op(
                self.engine,
                &abi_address(sender),
                &op,
                fee,
                &mut invalid,
            )
        };
        admission_from(status, &invalid)
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        check_abi_version()?;
        let prefix = path_to_cstring(prefix);
        let mut engine = std::ptr::null_mut();
        check_lifecycle(
            unsafe { sys::application_engine_from_dump(prefix.as_ptr(), &mut engine) },
            "from_dump",
        )?;
        Ok(Self::from_handle(engine))
    }

    fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError> {
        let prefix = path_to_cstring(prefix);
        check_lifecycle(
            unsafe { sys::application_engine_create_dump(self.engine, prefix.as_ptr()) },
            "create_dump",
        )
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        assert_abi_version();
        let prefix = path_to_cstring(prefix);
        let state_file = unsafe { sys::application_engine_state_file_in_dump(prefix.as_ptr()) };
        assert!(
            !state_file.is_null(),
            "engine could not name its state file: {}",
            last_error_message()
        );
        PathBuf::from(OsStr::from_bytes(
            unsafe { CStr::from_ptr(state_file) }.to_bytes(),
        ))
    }

    fn from_canonical_state(state_file: &Path, l2_tx_count: L2TxCount) -> Result<Self, AppError> {
        check_abi_version()?;
        let state_file = path_to_cstring(state_file);
        let mut engine = std::ptr::null_mut();
        check_lifecycle(
            unsafe {
                sys::application_engine_from_canonical_state(
                    state_file.as_ptr(),
                    l2_tx_count.get(),
                    &mut engine,
                )
            },
            "from_canonical_state",
        )?;
        Ok(Self::from_handle(engine))
    }
}
