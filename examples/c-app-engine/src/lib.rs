// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! An exclusively owned application engine behind the C ABI in `application-engine.h`.

pub mod sys;

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256};
use sequencer_core::application::{
    AppError, AppOutput, AppOutputs, ApplicationProgress, InvalidReason, ValidationOutcome,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
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

fn check(status: i32, operation: &str) -> Result<(), AppError> {
    if status == sys::APPLICATION_ENGINE_STATUS_OK {
        return Ok(());
    }
    let reason = format!(
        "engine {operation} failed (status {status}): {}",
        last_error_message()
    );
    let kind = match status {
        sys::APPLICATION_ENGINE_STATUS_IO_ERROR => std::io::ErrorKind::Other,
        sys::APPLICATION_ENGINE_STATUS_NOT_FOUND => std::io::ErrorKind::NotFound,
        sys::APPLICATION_ENGINE_STATUS_INVALID_DUMP => std::io::ErrorKind::InvalidData,
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
}

impl Application for EngineApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize =
        sys::APPLICATION_ENGINE_MAX_METHOD_PAYLOAD_BYTES as usize;

    fn validate_user_op(
        &self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u16,
    ) -> Result<ValidationOutcome, AppError> {
        let mut invalid = sys::ApplicationEngineInvalid {
            reason: 0,
            values: sys::ApplicationEngineInvalidValues {
                nonce: sys::ApplicationEngineInvalidNonce {
                    expected: 0,
                    got: 0,
                },
            },
        };
        let op = sys::ApplicationEngineUserOp {
            nonce: user_op.nonce,
            max_fee: user_op.max_fee,
            data: abi_span(user_op.data.as_ref()),
        };
        let status = unsafe {
            sys::application_engine_validate_user_op(
                self.engine,
                &abi_address(sender),
                &op,
                current_fee,
                &mut invalid,
            )
        };
        if status != sys::APPLICATION_ENGINE_STATUS_INVALID {
            check(status, "validate_user_op")?;
            return Ok(ValidationOutcome::Accept);
        }
        let reason = match invalid.reason {
            sys::APPLICATION_ENGINE_INVALID_NONCE => {
                let nonce = unsafe { invalid.values.nonce };
                InvalidReason::InvalidNonce {
                    expected: nonce.expected,
                    got: nonce.got,
                }
            }
            sys::APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE => {
                let balance = unsafe { invalid.values.fee_balance };
                InvalidReason::InsufficientFeeBalance {
                    required: U256::from_be_bytes(balance.required.bytes),
                    available: U256::from_be_bytes(balance.available.bytes),
                }
            }
            // Max-fee rejection belongs to the shared execution boundary.
            other => {
                return Err(internal(format!(
                    "engine reported unsupported invalid reason {other}"
                )));
            }
        };
        Ok(ValidationOutcome::Reject(reason))
    }

    fn apply_valid_user_op(
        &mut self,
        user_op: &ValidUserOp,
        safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        let op = sys::ApplicationEngineValidUserOp {
            sender: abi_address(user_op.sender),
            fee: user_op.fee,
            data: abi_span(&user_op.data),
        };
        let mut count = 0;
        check(
            unsafe {
                sys::application_engine_execute_valid_user_op(
                    self.engine,
                    &op,
                    safe_block,
                    &mut count,
                )
            },
            "execute_valid_user_op",
        )?;
        self.drain_outputs(count)
    }

    fn apply_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
        let direct = sys::ApplicationEngineDirectInput {
            sender: abi_address(input.sender),
            block_number: input.block_number,
            payload: abi_span(&input.payload),
        };
        let mut count = 0;
        check(
            unsafe {
                sys::application_engine_execute_direct_input(self.engine, &direct, &mut count)
            },
            "execute_direct_input",
        )?;
        self.drain_outputs(count)
    }

    fn progress(&self) -> ApplicationProgress {
        let count = unsafe { sys::application_engine_executed_input_count(self.engine) };
        let clock = unsafe { sys::application_engine_last_executed_safe_block(self.engine) };
        ApplicationProgress::try_new(ExecutedInputCount::new(count), clock)
            .expect("engine returned incoherent application progress")
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let prefix = path_to_cstring(prefix);
        let mut engine = std::ptr::null_mut();
        check(
            unsafe { sys::application_engine_from_dump(prefix.as_ptr(), &mut engine) },
            "from_dump",
        )?;
        assert!(
            !engine.is_null(),
            "engine reported successful load without a handle"
        );
        Ok(Self { engine })
    }

    fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError> {
        let prefix = path_to_cstring(prefix);
        check(
            unsafe { sys::application_engine_create_dump(self.engine, prefix.as_ptr()) },
            "create_dump",
        )
    }

    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        let prefix = path_to_cstring(prefix);
        check(
            unsafe { sys::application_engine_delete_dump(prefix.as_ptr()) },
            "delete_dump",
        )
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
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
}
