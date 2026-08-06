// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `Application` over an engine implementing the application-engine C API, linked at build time.
//! The contract is that header; see `docs/protocol/c-application-binding.md`.
//!
//! `canonical_snapshot_bytes` and `export_state` stay defaulted: the C API declares neither, so
//! the watchdog's compare has to reach the bytes through `state_file_in_dump` instead.

pub mod sys;

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256};
use sequencer_core::application::{AppError, AppOutput, AppOutputs, InvalidReason};
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
use sequencer_core::user_op::UserOp;

/// Re-exported so a host binary can name the trait's methods without depending on
/// `sequencer-core` itself, which `sequencer`'s root does not re-export.
pub use sequencer_core::application::Application;

fn path_to_cstring(path: &Path) -> CString {
    CString::new(path.as_os_str().as_encoded_bytes())
        .unwrap_or_else(|_| panic!("path contains an interior NUL: {}", path.display()))
}

/// Wrap an address the way the C API carries it.
fn abi_address(address: Address) -> sys::ApplicationEngineEthereumAddress {
    sys::ApplicationEngineEthereumAddress {
        bytes: address.into_array(),
    }
}

/// Borrow a byte slice the way the C API carries it. The span borrows, so the slice must
/// outlive the call it is handed to.
fn abi_span(bytes: &[u8]) -> sys::ApplicationEngineByteSpan {
    sys::ApplicationEngineByteSpan {
        data: bytes.as_ptr(),
        size: u64::try_from(bytes.len()).expect("length exceeds the ABI's u64 width"),
    }
}

/// Copy a payload span the engine handed back. The pointer belongs to the engine and the next
/// drain releases it, so this runs before draining again.
fn payload_from(span: sys::ApplicationEngineByteSpan) -> Vec<u8> {
    // A zero-length C++ vector may hand out a null data pointer, which Rust slices reject even
    // for empty slices
    if span.size == 0 {
        return Vec::new();
    }
    assert!(
        !span.data.is_null(),
        "non-empty output payload with a null pointer"
    );
    let len = usize::try_from(span.size).expect("length exceeds usize on this host");
    unsafe { std::slice::from_raw_parts(span.data, len) }.to_vec()
}

/// Copy the engine's message for the call that just failed. The engine overwrites the storage
/// on the next fallible call, so this must run before any further call, on the failing thread.
fn last_error_message() -> String {
    let message = unsafe { sys::application_engine_get_last_error_message() };
    if message.is_null() {
        // The ABI promises a non-null string, treat a broken promise as no detail
        return String::new();
    }
    unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

/// Turn a lifecycle call status into the `AppError` its caller propagates.
///
/// The engine tells a failed filesystem operation apart from a failed invariant, so a full disk
/// or a vanished dump becomes `Io` and everything else `Internal`.
fn check(status: i32, what: &str) -> Result<(), AppError> {
    if status == sys::APPLICATION_ENGINE_STATUS_OK {
        return Ok(());
    }
    let reason = format!("engine {what} failed: {}", last_error_message());
    if status == sys::APPLICATION_ENGINE_STATUS_IO_ERROR {
        return Err(AppError::Io(std::io::Error::other(reason)));
    }
    Err(AppError::Internal { reason })
}

/// Enforce the declared death policy on an engine `INTERNAL` status from an execution path.
///
/// Returning `AppError::Internal` here would hand the error to the canonical scheduler fold,
/// which catches application errors and continues, diverging from the machine where the same
/// engine throw terminates processing on possibly partially mutated state. Abort instead of
/// panicking so no unwind handler can resume past it.
fn die_on_internal(context: &str, detail: &str) -> ! {
    // Not eprintln!, which panics when stderr is a dead pipe. That panic would unwind out of the
    // lane task and the process would survive on partially mutated state, which is the outcome
    // this policy exists to prevent.
    let _ = std::io::Write::write_all(
        &mut std::io::stderr(),
        format!("fatal engine internal error in {context}: {detail}\n").as_bytes(),
    );
    std::process::abort();
}

/// The engine-backed application over the C ABI.
///
/// Owns the engine handle exclusively, the handle is freed on drop.
pub struct EngineApp {
    engine: *mut sys::ApplicationEngine,
}

// SAFETY: the engine handle is exclusively owned and only moved between threads, never
// shared for concurrent access. `Sync` licenses any consumer to share `&EngineApp` across
// threads, which the engine forbids, so soundness rests on the runtime's bound being
// declarative, no reference ever crosses threads. Nothing pins that, so it is a property the
// runtime has to keep, like `Clone` below.
unsafe impl Send for EngineApp {}
unsafe impl Sync for EngineApp {}

impl Drop for EngineApp {
    fn drop(&mut self) {
        unsafe { sys::application_engine_destroy(self.engine) };
    }
}

impl Clone for EngineApp {
    /// Fail-loud by design: the runtime's entry chain declares `Clone` but never exercises it,
    /// and silently aliasing or forking a live engine that maps its state would be a determinism
    /// hazard. A panic here means the runtime started cloning, and the seam then needs a
    /// deliberate fork or reopen entry point rather than a plausible-looking copy.
    fn clone(&self) -> Self {
        unimplemented!(
            "EngineApp cannot be cloned, the sequencer runtime owns exactly one engine \
             instance (upstream contract change detected)"
        )
    }
}

impl EngineApp {
    /// Drain the outputs of the execution that just ran into `AppOutputs`, in emission order.
    ///
    /// The count comes from that execution, and taking exactly it is what attributes the batch
    /// to the input that produced it. A kind this host was not built against is fatal rather than
    /// guessed past.
    fn drain_outputs(&mut self, count: u64) -> AppOutputs {
        let mut outputs = AppOutputs::new();
        for _ in 0..count {
            // Fresh per iteration on purpose. The engine writes it whole on OK, but reusing one
            // buffer would leave the previous drain's payload pointer readable if some engine
            // ever wrote only part of it, and that pointer is already freed by then.
            let mut output = sys::ApplicationEngineOutput {
                kind: 0,
                values: sys::ApplicationEngineOutputValues {
                    notice: abi_span(&[]),
                },
            };
            let status = unsafe { sys::application_engine_drain_output(self.engine, &mut output) };
            if status != sys::APPLICATION_ENGINE_STATUS_OK {
                die_on_internal("drain_outputs", &last_error_message());
            }
            // Each arm reads the union member its kind names, which is what makes the reads sound
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
                other => die_on_internal(
                    "drain_outputs",
                    &format!("engine reported unknown output kind {other}"),
                ),
            });
        }
        outputs
    }
}

impl Application for EngineApp {
    /// The ingress bound for a single method payload, declared by the application's build and
    /// read here from the header, so the bound this host enforces and the bound the engine
    /// parses under are one number rather than two that can drift. Raising it widens what every
    /// caller can push through the host, which is why it is a declaration the application makes
    /// rather than something negotiated at runtime.
    const MAX_METHOD_PAYLOAD_BYTES: usize =
        sys::APPLICATION_ENGINE_MAX_METHOD_PAYLOAD_BYTES as usize;

    fn validate_user_op(
        &self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u16,
    ) -> Result<(), InvalidReason> {
        // Written whole and only on INVALID, the reason selecting which member carries the
        // diagnostics. Any variant initializes it, the engine overwrites what it reports.
        let mut invalid = sys::ApplicationEngineInvalid {
            reason: 0,
            values: sys::ApplicationEngineInvalidValues {
                nonce: sys::ApplicationEngineInvalidNonce {
                    expected: 0,
                    got: 0,
                },
            },
        };
        let abi_user_op = sys::ApplicationEngineUserOp {
            nonce: user_op.nonce,
            max_fee: user_op.max_fee,
            data: abi_span(&user_op.data),
        };
        let status = unsafe {
            sys::application_engine_validate_user_op(
                self.engine,
                &abi_address(sender),
                &abi_user_op,
                current_fee,
                &mut invalid,
            )
        };
        match status {
            sys::APPLICATION_ENGINE_STATUS_OK => Ok(()),
            // Decode exhaustively against the constants generated from the header. The engine
            // emits only InvalidNonce and InsufficientFeeBalance, so any other value means a
            // reason this host was not built against, die rather than fabricate a rejection.
            // Each arm reads the union member its reason names, which makes the reads sound.
            sys::APPLICATION_ENGINE_STATUS_INVALID => match invalid.reason {
                sys::APPLICATION_ENGINE_INVALID_NONCE => {
                    let nonce = unsafe { invalid.values.nonce };
                    Err(InvalidReason::InvalidNonce {
                        expected: nonce.expected,
                        got: nonce.got,
                    })
                }
                sys::APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE => {
                    // Both amounts come from the engine at their on-chain width, so no fee
                    // table lookup happens here, which also keeps a hostile batch submitter from
                    // reaching the panicking converter on a fee it reports as all ones.
                    let fee_balance = unsafe { invalid.values.fee_balance };
                    Err(InvalidReason::InsufficientFeeBalance {
                        required: U256::from_be_bytes(fee_balance.required.bytes),
                        available: U256::from_be_bytes(fee_balance.available.bytes),
                    })
                }
                sys::APPLICATION_ENGINE_INVALID_MAX_FEE => die_on_internal(
                    "validate_user_op",
                    "engine reported InvalidMaxFee, a caller-owned reason it never produces",
                ),
                other => die_on_internal(
                    "validate_user_op",
                    &format!("engine reported unknown invalid reason {other}"),
                ),
            },
            _ => die_on_internal("validate_user_op", &last_error_message()),
        }
    }

    fn execute_valid_user_op(
        &mut self,
        user_op: &ValidUserOp,
        safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        let abi_user_op = sys::ApplicationEngineValidUserOp {
            sender: abi_address(user_op.sender),
            fee: user_op.fee,
            data: abi_span(&user_op.data),
        };
        let mut output_count: u64 = 0;
        let status = unsafe {
            sys::application_engine_execute_valid_user_op(
                self.engine,
                &abi_user_op,
                safe_block,
                &mut output_count,
            )
        };
        if status != sys::APPLICATION_ENGINE_STATUS_OK {
            die_on_internal("execute_valid_user_op", &last_error_message());
        }
        Ok(self.drain_outputs(output_count))
    }

    fn execute_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
        let abi_input = sys::ApplicationEngineDirectInput {
            sender: abi_address(input.sender),
            block_number: input.block_number,
            payload: abi_span(&input.payload),
        };
        let mut output_count: u64 = 0;
        let status = unsafe {
            sys::application_engine_execute_direct_input(self.engine, &abi_input, &mut output_count)
        };
        if status != sys::APPLICATION_ENGINE_STATUS_OK {
            die_on_internal("execute_direct_input", &last_error_message());
        }
        Ok(self.drain_outputs(output_count))
    }

    fn last_executed_safe_block(&self) -> u64 {
        unsafe { sys::application_engine_last_executed_safe_block(self.engine) }
    }

    fn executed_input_count(&self) -> u64 {
        unsafe { sys::application_engine_executed_input_count(self.engine) }
    }

    /// The pure constructor the trait asks for, no working copy and no process-global context.
    /// The engine maps the dump copy on write, so opening the same dump twice, or deleting it
    /// under a live engine, stays safe. Genesis is not ours to perform, the application's genesis
    /// tool writes a state once and every engine afterwards only opens one.
    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let prefix_c = path_to_cstring(prefix);
        let mut engine: *mut sys::ApplicationEngine = std::ptr::null_mut();
        let status = unsafe { sys::application_engine_from_dump(prefix_c.as_ptr(), &mut engine) };
        check(status, "from_dump")?;
        assert!(
            !engine.is_null(),
            "engine reported success from_dump without a handle"
        );
        Ok(Self { engine })
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        let prefix_c = path_to_cstring(prefix);
        let status = unsafe { sys::application_engine_create_dump(self.engine, prefix_c.as_ptr()) };
        check(status, "create_dump")
    }

    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        let prefix_c = path_to_cstring(prefix);
        let status = unsafe { sys::application_engine_delete_dump(prefix_c.as_ptr()) };
        check(status, "delete_dump")
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        // The engine names it, so where a state file sits inside a dump follows from the shape
        // the engine gives a dump instead of from an assumption made here. Pure over the path,
        // as the infallible contract requires, and infallible in practice too: the only way the
        // call fails is an allocation it cannot make, which is not a condition to paper over.
        //
        // An engine may also answer with `prefix` itself, which the trait's "can be the same
        // file" concession allows. Nothing here exercises that shape, so an engine taking it
        // should check the runtime's behaviour rather than assume it.
        let prefix_c = path_to_cstring(prefix);
        let state_file = unsafe { sys::application_engine_state_file_in_dump(prefix_c.as_ptr()) };
        if state_file.is_null() {
            die_on_internal("state_file_in_dump", &last_error_message());
        }
        // Taken as bytes rather than decoded: a Unix path is bytes, and decoding lossily would
        // answer with a different path for a prefix this host cannot spell in UTF-8.
        PathBuf::from(OsStr::from_bytes(
            unsafe { CStr::from_ptr(state_file) }.to_bytes(),
        ))
    }
}
