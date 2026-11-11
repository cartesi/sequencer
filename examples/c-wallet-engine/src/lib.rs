// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `app-core`'s wallet exported through the application-engine C API, building to
//! `libc_wallet_engine.a`. The rules it implements are stated in that header.
//!
//! Records come from `c-app-engine::sys`, generated from the same header, so producer and
//! consumer read one declaration. A `panic!` here aborts, which is the policy the header wants.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{CString, OsStr, c_char};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use app_core::application::{WalletApp, WalletConfig};
use c_app_engine::sys;
use sequencer_core::application::{
    Admission, AppError, AppOutput, AppOutputs, Application, PolicyRefusal,
};
use sequencer_core::history::L2TxCount;
use sequencer_core::l2_tx::{Direct, UserOpTx};
use sequencer_core::user_op::UserOp;

#[unsafe(no_mangle)]
pub extern "C" fn application_engine_abi_version() -> u32 {
    sys::APPLICATION_ENGINE_ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn application_engine_max_method_payload_bytes() -> u64 {
    u64::try_from(WalletApp::max_method_payload_bytes()).expect("wallet payload bound exceeds u64")
}

/// # Safety
/// `out_limits` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_scheduler_limits(
    out_limits: *mut sys::ApplicationEngineSchedulerLimits,
) {
    let limits = WalletApp::scheduler_limits();
    unsafe {
        *out_limits = sys::ApplicationEngineSchedulerLimits {
            max_wait_blocks: limits.max_wait_blocks,
            fridge_bound: limits.fridge_bound,
            max_direct_payload_bytes: limits.max_direct_payload_bytes,
        }
    };
}

thread_local! {
    /// The last failure's message, and the buffer `state_file_in_dump` answers out of.
    ///
    /// Thread local because the header requires the handle-free entry points to be reentrant.
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
    static STATE_FILE: RefCell<CString> = RefCell::new(CString::default());
}

fn clear_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = CString::default());
}

fn set_error(message: impl AsRef<str>) {
    // A NUL inside a diagnostic is not worth failing over, truncate at it
    let message = message.as_ref();
    let bytes = message.split('\0').next().unwrap_or_default().as_bytes();
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(bytes).unwrap_or_default();
    });
}

/// Admission, nonce reads and execution: NOT_FOUND and INVALID_DUMP are lifecycle-only statuses.
fn report(error: &AppError, what: &str) -> sys::ApplicationEngineStatus {
    match error {
        AppError::Io(err) => {
            set_error(format!("{what} failed: {err}"));
            sys::APPLICATION_ENGINE_STATUS_IO_ERROR
        }
        other => {
            set_error(format!("{what} failed: {other:?}"));
            sys::APPLICATION_ENGINE_STATUS_INTERNAL_ERROR
        }
    }
}

/// Opening a dump or a canonical state file, and creating a dump: preserve the missing/corrupt
/// classification across the ABI. These are the kinds `referenced_artifact_io_is_terminal` and
/// the host's AppBootstrap arm treat as terminal; other I/O, PermissionDenied included, stays
/// IO_ERROR, which the host classifies operational. Diagnostics are not parsed.
fn report_lifecycle(error: &AppError, what: &str) -> sys::ApplicationEngineStatus {
    let AppError::Io(err) = error else {
        return report(error, what);
    };
    let status = match err.kind() {
        std::io::ErrorKind::NotFound => sys::APPLICATION_ENGINE_STATUS_NOT_FOUND,
        std::io::ErrorKind::InvalidData
        | std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::NotADirectory
        | std::io::ErrorKind::IsADirectory => sys::APPLICATION_ENGINE_STATUS_INVALID_DUMP,
        _ => return report(error, what),
    };
    set_error(format!("{what} failed: {err}"));
    status
}

/// Borrow a path the way the C API carries it, raw bytes rather than text.
///
/// A Unix path is bytes. Decoding it as UTF-8 would answer for a different path than the caller
/// named whenever it is not valid UTF-8.
///
/// # Safety
/// `path` must be a non-null NUL terminated string that outlives the call.
unsafe fn path_from(path: *const c_char) -> PathBuf {
    assert!(!path.is_null(), "the C API forbids a null path");
    let bytes = unsafe { std::ffi::CStr::from_ptr(path) }.to_bytes();
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// Borrow a byte span the way the C API carries it.
///
/// # Safety
/// The span must describe a readable range that outlives the call, or be empty.
unsafe fn slice_from<'a>(span: &sys::ApplicationEngineByteSpan) -> &'a [u8] {
    if span.size == 0 {
        // An empty span may carry a null pointer, which Rust slices reject even when empty
        return &[];
    }
    assert!(!span.data.is_null(), "non-empty span with a null pointer");
    let len = usize::try_from(span.size).expect("span length exceeds usize on this host");
    unsafe { std::slice::from_raw_parts(span.data, len) }
}

/// The engine instance behind the opaque handle.
pub struct ApplicationEngine {
    app: WalletApp,
    /// What the last execution produced, in emission order, still to be drained.
    pending: VecDeque<AppOutput>,
    /// The payload the last drain handed out. Held here because the span the caller receives
    /// borrows it, and the contract keeps it alive until the next drain releases it.
    drained_payload: Vec<u8>,
}

/// Take a handle the C API was given.
///
/// # Safety
/// `engine` must be a live handle from `application_engine_from_dump` or
/// `application_engine_from_canonical_state`, not yet destroyed.
unsafe fn engine_ref<'a>(engine: *const ApplicationEngine) -> &'a ApplicationEngine {
    assert!(!engine.is_null(), "the C API forbids a null engine handle");
    unsafe { &*engine }
}

/// # Safety
/// As [`engine_ref`], and no other reference to the engine may be live.
unsafe fn engine_mut<'a>(engine: *mut ApplicationEngine) -> &'a mut ApplicationEngine {
    assert!(!engine.is_null(), "the C API forbids a null engine handle");
    unsafe { &mut *engine }
}

#[unsafe(no_mangle)]
pub extern "C" fn application_engine_get_last_error_message() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Hand out a freshly opened wallet, or classify why it could not be opened.
///
/// # Safety
/// `out_engine` is writable.
unsafe fn open(
    opened: Result<WalletApp, AppError>,
    out_engine: *mut *mut ApplicationEngine,
    what: &str,
) -> sys::ApplicationEngineStatus {
    match opened {
        Ok(app) => {
            let engine = Box::new(ApplicationEngine {
                app,
                pending: VecDeque::new(),
                drained_payload: Vec::new(),
            });
            unsafe { *out_engine = Box::into_raw(engine) };
            sys::APPLICATION_ENGINE_STATUS_OK
        }
        // The wallet's constructors only read and decode, and a count that disagrees with the
        // bytes is a decode failure too, so their Internal errors identify malformed bytes.
        Err(AppError::Internal { reason }) => {
            set_error(format!("{what} failed: {reason}"));
            sys::APPLICATION_ENGINE_STATUS_INVALID_DUMP
        }
        Err(err) => report_lifecycle(&err, what),
    }
}

/// # Safety
/// `prefix` is a NUL terminated path and `out_engine` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_from_dump(
    prefix: *const c_char,
    out_engine: *mut *mut ApplicationEngine,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let prefix = unsafe { path_from(prefix) };
    unsafe { open(WalletApp::from_dump(&prefix), out_engine, "from_dump") }
}

/// # Safety
/// `state_file` is a NUL terminated path and `out_engine` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_from_canonical_state(
    state_file: *const c_char,
    l2_tx_count: u64,
    out_engine: *mut *mut ApplicationEngine,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let state_file = unsafe { path_from(state_file) };
    let opened = WalletApp::from_canonical_state(&state_file, L2TxCount::new(l2_tx_count));
    unsafe { open(opened, out_engine, "from_canonical_state") }
}

/// # Safety
/// `engine` is a live handle, and it is not used again afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_destroy(engine: *mut ApplicationEngine) {
    if engine.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(engine) });
}

/// # Safety
/// Every pointer is non-null and its pointee outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_user_nonce(
    engine: *const ApplicationEngine,
    sender: *const sys::ApplicationEngineEthereumAddress,
    out_nonce: *mut u32,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let engine = unsafe { engine_ref(engine) };
    let sender = Address::from(unsafe { (*sender).bytes });
    match engine.app.user_nonce(sender) {
        Ok(nonce) => {
            unsafe { *out_nonce = nonce };
            sys::APPLICATION_ENGINE_STATUS_OK
        }
        Err(err) => report(&err, "user_nonce"),
    }
}

/// # Safety
/// Every pointer is non-null and its pointee outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_admit_user_op(
    engine: *const ApplicationEngine,
    sender: *const sys::ApplicationEngineEthereumAddress,
    user_op: *const sys::ApplicationEngineUserOp,
    fee: u16,
    out_invalid: *mut sys::ApplicationEngineInvalid,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let engine = unsafe { engine_ref(engine) };
    let sender = Address::from(unsafe { (*sender).bytes });
    let user_op = unsafe { &*user_op };
    let op = UserOp {
        nonce: user_op.nonce,
        max_fee: user_op.max_fee,
        data: unsafe { slice_from(&user_op.data) }.to_vec().into(),
    };

    let refusal = match engine.app.admit_user_op(sender, &op, fee) {
        Ok(Admission::Admit) => return sys::APPLICATION_ENGINE_STATUS_OK,
        Ok(Admission::Refuse(refusal)) => refusal,
        Err(err) => return report(&err, "admit_user_op"),
    };
    let invalid = match refusal {
        PolicyRefusal::Underfunded {
            required,
            available,
        } => sys::ApplicationEngineInvalid {
            reason: sys::APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE,
            values: sys::ApplicationEngineInvalidValues {
                fee_balance: sys::ApplicationEngineInsufficientFeeBalance {
                    required: sys::ApplicationEngineUint256 {
                        bytes: required.to_be_bytes(),
                    },
                    available: sys::ApplicationEngineUint256 {
                        bytes: available.to_be_bytes(),
                    },
                },
            },
        },
        // The header gives this reason no union member: its text travels in the last error
        // message, and the record carries the reason alone.
        PolicyRefusal::Other(reason) => {
            set_error(reason);
            sys::ApplicationEngineInvalid {
                reason: sys::APPLICATION_ENGINE_REFUSED_BY_POLICY,
                values: sys::ApplicationEngineInvalidValues {
                    fee_balance: sys::ApplicationEngineInsufficientFeeBalance {
                        required: sys::ApplicationEngineUint256 { bytes: [0; 32] },
                        available: sys::ApplicationEngineUint256 { bytes: [0; 32] },
                    },
                },
            }
        }
    };
    unsafe { *out_invalid = invalid };
    sys::APPLICATION_ENGINE_STATUS_INVALID
}

/// Take an execution's outputs, refusing to run over ones still queued.
///
/// The refusal is what makes the count an execution reports its own. Discarding them instead
/// would drop outputs bound for the chain.
/// # Safety
/// `out_output_count` is writable and outlives the call.
unsafe fn execute(
    engine: &mut ApplicationEngine,
    out_output_count: *mut u64,
    run: impl FnOnce(&mut WalletApp) -> Result<AppOutputs, AppError>,
) -> sys::ApplicationEngineStatus {
    if !engine.pending.is_empty() {
        set_error("an earlier execution's outputs are still queued");
        return sys::APPLICATION_ENGINE_STATUS_INTERNAL_ERROR;
    }
    match run(&mut engine.app) {
        Ok(outputs) => {
            engine.pending = outputs.into();
            // SAFETY: the caller guarantees the pointer is writable.
            unsafe { *out_output_count = engine.pending.len() as u64 };
            sys::APPLICATION_ENGINE_STATUS_OK
        }
        // Application errors define no successor; the caller must discard this instance.
        Err(err) => report(&err, "execute"),
    }
}

/// The user op a record describes, its data copied. The wallet reads neither the nonce nor the
/// safe block, so only this crate's tests would see them dropped or swapped.
/// # Safety
/// The record's data span is readable.
unsafe fn user_op_from(user_op: &sys::ApplicationEngineUserOpTx) -> UserOpTx {
    UserOpTx {
        sender: Address::from(user_op.sender.bytes),
        nonce: user_op.nonce,
        fee: user_op.fee,
        block: user_op.safe_block,
        data: unsafe { slice_from(&user_op.data) }.to_vec(),
    }
}

/// The direct a record describes, its payload copied. The wallet reads neither the index, the
/// block nor the timestamp, so only this crate's tests would see them dropped or swapped.
/// # Safety
/// The record's payload span is readable.
unsafe fn direct_from(input: &sys::ApplicationEngineDirectInput) -> Direct {
    Direct {
        input_index: input.input_index,
        sender: Address::from(input.sender.bytes),
        block: input.block_number,
        timestamp: input.timestamp,
        payload: unsafe { slice_from(&input.payload) }.to_vec(),
    }
}

/// # Safety
/// Every pointer is non-null and its pointee outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_execute_user_op(
    engine: *mut ApplicationEngine,
    user_op: *const sys::ApplicationEngineUserOpTx,
    out_output_count: *mut u64,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let engine = unsafe { engine_mut(engine) };
    let op = unsafe { user_op_from(&*user_op) };
    // SAFETY: the caller guarantees `out_output_count` is writable.
    unsafe { execute(engine, out_output_count, |app| app.apply_user_op(&op)) }
}

/// # Safety
/// Every pointer is non-null and its pointee outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_execute_direct_input(
    engine: *mut ApplicationEngine,
    input: *const sys::ApplicationEngineDirectInput,
    out_output_count: *mut u64,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let engine = unsafe { engine_mut(engine) };
    let direct = unsafe { direct_from(&*input) };
    // SAFETY: the caller guarantees `out_output_count` is writable.
    unsafe { execute(engine, out_output_count, |app| app.apply_direct(&direct)) }
}

/// # Safety
/// `engine` is a live handle and `out_output` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_drain_output(
    engine: *mut ApplicationEngine,
    out_output: *mut sys::ApplicationEngineOutput,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let engine = unsafe { engine_mut(engine) };
    // Taking one more than were queued is a caller bug, reported rather than answered with an
    // empty output a host might act on
    let Some(output) = engine.pending.pop_front() else {
        set_error("no output is queued");
        return sys::APPLICATION_ENGINE_STATUS_INTERNAL_ERROR;
    };

    let written = match output {
        AppOutput::Voucher {
            destination,
            value,
            payload,
        } => {
            engine.drained_payload = payload;
            sys::ApplicationEngineOutput {
                kind: sys::APPLICATION_ENGINE_OUTPUT_VOUCHER,
                values: sys::ApplicationEngineOutputValues {
                    voucher: sys::ApplicationEngineVoucher {
                        destination: sys::ApplicationEngineEthereumAddress {
                            bytes: destination.into_array(),
                        },
                        value: sys::ApplicationEngineUint256 {
                            bytes: value.to_be_bytes(),
                        },
                        payload: span_of(&engine.drained_payload),
                    },
                },
            }
        }
        AppOutput::Notice(payload) => {
            engine.drained_payload = payload;
            sys::ApplicationEngineOutput {
                kind: sys::APPLICATION_ENGINE_OUTPUT_NOTICE,
                values: sys::ApplicationEngineOutputValues {
                    notice: span_of(&engine.drained_payload),
                },
            }
        }
    };
    unsafe { *out_output = written };
    sys::APPLICATION_ENGINE_STATUS_OK
}

/// Lend a payload to the caller. It stays valid until the next drain replaces it.
fn span_of(payload: &[u8]) -> sys::ApplicationEngineByteSpan {
    sys::ApplicationEngineByteSpan {
        data: payload.as_ptr(),
        size: payload.len() as u64,
    }
}

/// # Safety
/// `engine` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_l2_tx_count(engine: *const ApplicationEngine) -> u64 {
    unsafe { engine_ref(engine) }.app.l2_tx_count().get()
}

/// # Safety
/// `engine` is a live handle and `prefix` is a NUL terminated path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_create_dump(
    engine: *mut ApplicationEngine,
    prefix: *const c_char,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let engine = unsafe { engine_mut(engine) };
    let prefix = unsafe { path_from(prefix) };
    match engine.app.create_dump(&prefix) {
        Ok(()) => sys::APPLICATION_ENGINE_STATUS_OK,
        Err(err) => report_lifecycle(&err, "create_dump"),
    }
}

/// # Safety
/// `prefix` is a NUL terminated path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_state_file_in_dump(
    prefix: *const c_char,
) -> *const c_char {
    clear_error();
    let prefix = unsafe { path_from(prefix) };
    // Pure over the prefix: it touches no filesystem and needs no engine. Where the state file
    // sits follows from the shape this app gives a dump, a directory with `state` inside it.
    let state_file = WalletApp::state_file_in_dump(&prefix);
    // The prefix came from a CStr and `join("state")` adds no NUL.
    let path =
        CString::new(state_file.as_os_str().as_encoded_bytes()).expect("prefix came from a CStr");
    STATE_FILE.with(|slot| {
        *slot.borrow_mut() = path;
        slot.borrow().as_ptr()
    })
}

/* -- genesis, the one entry point that is not part of the seam -- */

/// Write a genesis state at `prefix`, an empty wallet with the given deployment configuration.
///
/// The seam has no create path, so every application ships a genesis tool. This is the wallet's.
pub fn write_genesis(prefix: &Path, config: WalletConfig) -> Result<(), AppError> {
    WalletApp::new(config).create_dump(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(bytes: &[u8]) -> sys::ApplicationEngineByteSpan {
        sys::ApplicationEngineByteSpan {
            data: bytes.as_ptr(),
            size: bytes.len() as u64,
        }
    }

    #[test]
    fn records_unpack_every_field() {
        let payload = [1_u8, 2, 3];
        let direct = sys::ApplicationEngineDirectInput {
            sender: sys::ApplicationEngineEthereumAddress { bytes: [0x41; 20] },
            input_index: 3,
            block_number: 7,
            timestamp: 1_700_000_011,
            payload: span(&payload),
        };
        assert_eq!(
            unsafe { direct_from(&direct) },
            Direct {
                input_index: 3,
                sender: Address::repeat_byte(0x41),
                block: 7,
                timestamp: 1_700_000_011,
                payload: payload.to_vec(),
            }
        );

        let data = [4_u8, 5];
        let user_op = sys::ApplicationEngineUserOpTx {
            sender: sys::ApplicationEngineEthereumAddress { bytes: [0x42; 20] },
            nonce: 5,
            fee: 300,
            safe_block: 9,
            data: span(&data),
        };
        assert_eq!(
            unsafe { user_op_from(&user_op) },
            UserOpTx {
                sender: Address::repeat_byte(0x42),
                nonce: 5,
                fee: 300,
                block: 9,
                data: data.to_vec(),
            }
        );
    }
}
