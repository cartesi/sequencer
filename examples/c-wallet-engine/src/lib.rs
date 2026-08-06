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
use sequencer_core::application::{AppError, AppOutput, AppOutputs, Application, InvalidReason};
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
use sequencer_core::user_op::UserOp;

// The header carries no default for the ingress bound, so a build supplies it. This is what
// makes the two agree: a build that told the host a different number than the wallet implements
// fails here rather than at the boundary.
const _: () = assert!(
    sys::APPLICATION_ENGINE_MAX_METHOD_PAYLOAD_BYTES as usize
        == WalletApp::MAX_METHOD_PAYLOAD_BYTES,
    "the payload bound this build declares to the host is not the wallet's own"
);

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

/// Report a lifecycle failure, telling a refusing filesystem apart from a broken invariant.
///
/// The split is the point of the second status: a full disk or a vanished dump is a condition a
/// caller may act on, an engine that failed its own invariant is not.
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
/// `engine` must be a live handle from `application_engine_from_dump`, not yet destroyed.
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

/// # Safety
/// `prefix` is a NUL terminated path and `out_engine` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_from_dump(
    prefix: *const c_char,
    out_engine: *mut *mut ApplicationEngine,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let prefix = unsafe { path_from(prefix) };
    match WalletApp::from_dump(&prefix) {
        Ok(app) => {
            let engine = Box::new(ApplicationEngine {
                app,
                pending: VecDeque::new(),
                drained_payload: Vec::new(),
            });
            unsafe { *out_engine = Box::into_raw(engine) };
            sys::APPLICATION_ENGINE_STATUS_OK
        }
        Err(err) => report(&err, "from_dump"),
    }
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
pub unsafe extern "C" fn application_engine_validate_user_op(
    engine: *const ApplicationEngine,
    sender: *const sys::ApplicationEngineEthereumAddress,
    user_op: *const sys::ApplicationEngineUserOp,
    current_fee: u16,
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

    match engine.app.validate_user_op(sender, &op, current_fee) {
        Ok(()) => sys::APPLICATION_ENGINE_STATUS_OK,
        Err(reason) => {
            let invalid = match reason {
                InvalidReason::InvalidNonce { expected, got } => sys::ApplicationEngineInvalid {
                    reason: sys::APPLICATION_ENGINE_INVALID_NONCE,
                    values: sys::ApplicationEngineInvalidValues {
                        nonce: sys::ApplicationEngineInvalidNonce { expected, got },
                    },
                },
                InvalidReason::InsufficientFeeBalance {
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
                // The caller owns the max-fee guard and this entry point never checks it, so the
                // app cannot produce this reason. Reporting it would be a lie about which union
                // member carries the diagnostics.
                InvalidReason::InvalidMaxFee { .. } => {
                    set_error("the app reported a caller-owned max fee rejection");
                    return sys::APPLICATION_ENGINE_STATUS_INTERNAL_ERROR;
                }
            };
            unsafe { *out_invalid = invalid };
            sys::APPLICATION_ENGINE_STATUS_INVALID
        }
    }
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
        // An app error on an execution path is fatal-no-resume, and the host aborts on it. The
        // state may already be partially mutated, so there is nothing to resume from.
        Err(err) => report(&err, "execute"),
    }
}

/// # Safety
/// Every pointer is non-null and its pointee outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_execute_valid_user_op(
    engine: *mut ApplicationEngine,
    user_op: *const sys::ApplicationEngineValidUserOp,
    safe_block: u64,
    out_output_count: *mut u64,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let engine = unsafe { engine_mut(engine) };
    let user_op = unsafe { &*user_op };
    let op = ValidUserOp {
        sender: Address::from(user_op.sender.bytes),
        fee: user_op.fee,
        data: unsafe { slice_from(&user_op.data) }.to_vec(),
    };
    // SAFETY: the caller guarantees `out_output_count` is writable.
    unsafe {
        execute(engine, out_output_count, |app| {
            app.execute_valid_user_op(&op, safe_block)
        })
    }
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
    let input = unsafe { &*input };
    let direct = DirectInput {
        sender: Address::from(input.sender.bytes),
        block_number: input.block_number,
        payload: unsafe { slice_from(&input.payload) }.to_vec(),
    };
    // SAFETY: the caller guarantees `out_output_count` is writable.
    unsafe {
        execute(engine, out_output_count, |app| {
            app.execute_direct_input(&direct)
        })
    }
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
pub unsafe extern "C" fn application_engine_last_executed_safe_block(
    engine: *const ApplicationEngine,
) -> u64 {
    unsafe { engine_ref(engine) }.app.last_executed_safe_block()
}

/// # Safety
/// `engine` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_executed_input_count(
    engine: *const ApplicationEngine,
) -> u64 {
    unsafe { engine_ref(engine) }.app.executed_input_count()
}

/// # Safety
/// `engine` is a live handle and `prefix` is a NUL terminated path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_create_dump(
    engine: *const ApplicationEngine,
    prefix: *const c_char,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let engine = unsafe { engine_ref(engine) };
    let prefix = unsafe { path_from(prefix) };
    match engine.app.create_dump(&prefix) {
        Ok(()) => sys::APPLICATION_ENGINE_STATUS_OK,
        Err(err) => report(&err, "create_dump"),
    }
}

/// # Safety
/// `prefix` is a NUL terminated path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn application_engine_delete_dump(
    prefix: *const c_char,
) -> sys::ApplicationEngineStatus {
    clear_error();
    let prefix = unsafe { path_from(prefix) };
    match WalletApp::delete_dump(&prefix) {
        Ok(()) => sys::APPLICATION_ENGINE_STATUS_OK,
        Err(err) => report(&err, "delete_dump"),
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
    match CString::new(state_file.as_os_str().as_encoded_bytes()) {
        Ok(path) => STATE_FILE.with(|slot| {
            *slot.borrow_mut() = path;
            slot.borrow().as_ptr()
        }),
        Err(_) => {
            set_error("the dump prefix contains an interior NUL");
            std::ptr::null()
        }
    }
}

/* -- genesis, the one entry point that is not part of the seam -- */

/// Write a genesis state at `prefix`, an empty wallet with the given deployment configuration.
///
/// The seam has no create path, so every application ships a genesis tool. This is the wallet's.
pub fn write_genesis(prefix: &Path, config: WalletConfig) -> Result<(), AppError> {
    WalletApp::new(config).create_dump(prefix)
}
