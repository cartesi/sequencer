// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! sequencer-core's reference [`Scheduler`](sequencer_core::scheduler::Scheduler) exported
//! through `include/scheduler.h`, building to `libc_scheduler.a`. The header states the
//! contract; this file only maps it onto the reference.
//!
//! The reference panics on impossible states. Here every call checks the ones it can name
//! before calling in (an input out of order, a feed before DONE, an exhausted counter) and
//! catches any other panic, so none unwinds into C. A build with `panic = "abort"` cannot
//! catch: there the batch-nonce halt aborts the process, as the reference would.

pub mod sys;

use std::any::Any;
use std::cell::RefCell;
use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};

use alloy_primitives::Address;
use sequencer_core::l2_tx::L2Tx;
use sequencer_core::scheduler::{
    BatchRejectReason, InputOutcome, L1Input, Scheduled, Scheduler as Reference, SchedulerConfig,
    SchedulerState,
};

/// The exported functions must have exactly the header's signatures: the bindgen declarations
/// and these definitions coerce to one function-pointer type each, or this fails to compile.
const _: () = {
    let _: [unsafe extern "C" fn() -> u32; 2] = [sys::scheduler_abi_version, scheduler_abi_version];
    let _: [unsafe extern "C" fn() -> *const c_char; 2] = [
        sys::scheduler_get_last_error_message,
        scheduler_get_last_error_message,
    ];
    let _: [unsafe extern "C" fn(
        *const sys::SchedulerConfig,
        *mut *mut sys::Scheduler,
    ) -> sys::SchedulerStatus; 2] = [sys::scheduler_create, scheduler_create];
    let _: [unsafe extern "C" fn(
        *const sys::SchedulerConfig,
        *const sys::SchedulerState,
        *const sys::SchedulerInput,
        u64,
        *mut *mut sys::Scheduler,
    ) -> sys::SchedulerStatus; 2] = [sys::scheduler_resume, scheduler_resume];
    let _: [unsafe extern "C" fn(*mut sys::Scheduler); 2] =
        [sys::scheduler_destroy, scheduler_destroy];
    let _: [unsafe extern "C" fn(
        *const sys::Scheduler,
        *mut sys::SchedulerState,
    ) -> sys::SchedulerStatus; 2] = [sys::scheduler_state, scheduler_state];
    let _: [unsafe extern "C" fn(
        *mut sys::Scheduler,
        *const sys::SchedulerInput,
        *mut sys::SchedulerInputOutcome,
    ) -> sys::SchedulerStatus; 2] = [sys::scheduler_feed, scheduler_feed];
    let _: [unsafe extern "C" fn(
        *mut sys::Scheduler,
        *mut sys::SchedulerTx,
    ) -> sys::SchedulerStatus; 2] = [sys::scheduler_next_tx, scheduler_next_tx];
};

thread_local! {
    /// The last failure's message, thread local as the header promises.
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn clear_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = CString::default());
}

fn set_error(message: &str) {
    // A NUL inside a diagnostic is not worth failing over: truncate at it.
    let bytes = message.split('\0').next().unwrap_or_default().as_bytes();
    LAST_ERROR.with(|slot| *slot.borrow_mut() = CString::new(bytes).unwrap_or_default());
}

/// A failed call: the status the header names for it and the diagnostic behind it.
struct Failure {
    status: sys::SchedulerStatus,
    message: String,
}

impl Failure {
    fn misuse(message: impl Into<String>) -> Self {
        Self {
            status: sys::SCHEDULER_STATUS_MISUSE,
            message: message.into(),
        }
    }

    fn no_successor(message: impl Into<String>) -> Self {
        Self {
            status: sys::SCHEDULER_STATUS_NO_SUCCESSOR,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: sys::SCHEDULER_STATUS_INTERNAL_ERROR,
            message: message.into(),
        }
    }

    fn report(self) -> sys::SchedulerStatus {
        set_error(&self.message);
        self.status
    }
}

/// Run a call into the reference, turning a panic into its message so it never reaches C.
fn catch<T>(call: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(call)).map_err(|payload| panic_message(payload.as_ref()))
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "the reference scheduler panicked without a message".to_owned()
    }
}

/// The scheduler behind an opaque `Scheduler *`.
struct Handle {
    reference: Reference,
    /// Every transaction of the last fed input has been handed out: `next_tx` said DONE.
    pulled_all: bool,
    /// A call failed; the reference may be torn, so nothing reaches it again.
    dead: bool,
    /// The transaction the last `next_tx` handed out, which its spans borrow.
    current: Option<Scheduled>,
}

impl Handle {
    fn new(reference: Reference) -> Self {
        Self {
            reference,
            pulled_all: true,
            dead: false,
            current: None,
        }
    }

    fn into_raw(self) -> *mut sys::Scheduler {
        Box::into_raw(Box::new(self)).cast()
    }
}

/// # Safety
/// `scheduler` is null or a handle from `scheduler_create` or `scheduler_resume`, not yet
/// destroyed, that no other thread is using.
unsafe fn live<'a>(scheduler: *const sys::Scheduler) -> Result<&'a Handle, Failure> {
    let handle = unsafe { scheduler.cast::<Handle>().as_ref() }
        .ok_or_else(|| Failure::misuse("null scheduler handle"))?;
    if handle.dead {
        return Err(Failure::misuse("the scheduler failed earlier and is dead"));
    }
    Ok(handle)
}

/// Run a mutating call on a live handle; any failure leaves the handle dead.
///
/// # Safety
/// As [`live`], and no other reference to the handle is live.
unsafe fn on_live_handle(
    scheduler: *mut sys::Scheduler,
    call: impl FnOnce(&mut Handle) -> Result<sys::SchedulerStatus, Failure>,
) -> sys::SchedulerStatus {
    clear_error();
    if let Err(failure) = unsafe { live(scheduler) } {
        return failure.report();
    }
    // Live just checked it is non-null and not dead.
    let handle = unsafe { &mut *scheduler.cast::<Handle>() };
    match call(handle) {
        Ok(status) => status,
        Err(failure) => {
            handle.dead = true;
            handle.current = None;
            failure.report()
        }
    }
}

fn address_from(address: &sys::ApplicationEngineEthereumAddress) -> Address {
    Address::from(address.bytes)
}

fn address_record(address: Address) -> sys::ApplicationEngineEthereumAddress {
    sys::ApplicationEngineEthereumAddress {
        bytes: address.into_array(),
    }
}

/// # Safety
/// `config` is null or readable.
unsafe fn config_from(config: *const sys::SchedulerConfig) -> Result<SchedulerConfig, Failure> {
    let config = unsafe { config.as_ref() }.ok_or_else(|| Failure::misuse("null config"))?;
    // The reference asserts this; checked here so a bad build reads as a status, not a panic.
    if config.max_wait_blocks == 0 {
        return Err(Failure::misuse("max_wait_blocks must be at least 1"));
    }
    Ok(SchedulerConfig {
        sequencer_address: address_from(&config.sequencer_address),
        max_wait_blocks: config.max_wait_blocks,
        fridge_bound: config.fridge_bound,
        max_direct_payload_bytes: config.max_direct_payload_bytes,
    })
}

fn state_from(state: &sys::SchedulerState) -> SchedulerState {
    SchedulerState {
        l1_input_count: state.l1_input_count,
        next_batch_nonce: state.next_batch_nonce,
        clock: state.clock,
        first_queued_input: state.first_queued_input,
        fridge_len: state.fridge_len,
    }
}

fn state_record(state: SchedulerState) -> sys::SchedulerState {
    sys::SchedulerState {
        l1_input_count: state.l1_input_count,
        next_batch_nonce: state.next_batch_nonce,
        clock: state.clock,
        first_queued_input: state.first_queued_input,
        fridge_len: state.fridge_len,
    }
}

/// `count` records of `size` bytes as a slice length, if the host can address them:
/// `from_raw_parts` requires the whole range to fit `isize::MAX` bytes.
fn addressable(count: u64, size: usize) -> Option<usize> {
    let len = usize::try_from(count).ok()?;
    let bytes = len.checked_mul(size)?;
    (bytes <= isize::MAX as usize).then_some(len)
}

/// Borrow an input's payload, refusing a span that cannot describe readable memory.
///
/// # Safety
/// A non-empty span with a non-null pointer describes a readable range that outlives the call.
unsafe fn payload_from<'a>(span: &sys::ApplicationEngineByteSpan) -> Result<&'a [u8], Failure> {
    if span.size == 0 {
        // An empty span may carry a null pointer, which a Rust slice rejects even when empty.
        return Ok(&[]);
    }
    if span.data.is_null() {
        return Err(Failure::misuse(
            "non-empty payload span with a null pointer",
        ));
    }
    let len = addressable(span.size, 1)
        .ok_or_else(|| Failure::misuse("payload length exceeds this host's address space"))?;
    Ok(unsafe { std::slice::from_raw_parts(span.data, len) })
}

/// Copy an input record into the reference's own.
///
/// # Safety
/// As [`payload_from`] for the input's payload.
unsafe fn input_from(input: &sys::SchedulerInput) -> Result<L1Input, Failure> {
    Ok(L1Input {
        index: input.index,
        sender: address_from(&input.sender),
        block: input.block_number,
        timestamp: input.timestamp,
        chain_id: input.chain_id,
        app_contract: address_from(&input.app_contract),
        payload: unsafe { payload_from(&input.payload) }?.to_vec(),
    })
}

fn outcome_record(outcome: InputOutcome) -> sys::SchedulerInputOutcome {
    let mut record = sys::SchedulerInputOutcome::default();
    match outcome {
        InputOutcome::DirectQueued => record.kind = sys::SCHEDULER_DIRECT_QUEUED,
        InputOutcome::DirectDropped => record.kind = sys::SCHEDULER_DIRECT_DROPPED,
        InputOutcome::BatchAccepted => record.kind = sys::SCHEDULER_BATCH_ACCEPTED,
        InputOutcome::BatchStale => record.kind = sys::SCHEDULER_BATCH_STALE,
        InputOutcome::BatchRejected(reason) => {
            record.kind = sys::SCHEDULER_BATCH_REJECTED;
            record.reject_reason = match reason {
                BatchRejectReason::DecodeFailed => sys::SCHEDULER_REJECT_DECODE_FAILED,
                BatchRejectReason::WrongNonce { expected, got } => {
                    record.reject_values.wrong_nonce = sys::SchedulerWrongNonce { expected, got };
                    sys::SCHEDULER_REJECT_WRONG_NONCE
                }
                BatchRejectReason::SafeBlockNotBelowInclusionBlock => {
                    sys::SCHEDULER_REJECT_SAFE_BLOCK_NOT_BELOW_INCLUSION_BLOCK
                }
                BatchRejectReason::NonMonotonicSafeBlocks => {
                    sys::SCHEDULER_REJECT_NON_MONOTONIC_SAFE_BLOCKS
                }
                BatchRejectReason::FeePriceOutOfRange => {
                    sys::SCHEDULER_REJECT_FEE_PRICE_OUT_OF_RANGE
                }
                BatchRejectReason::FirstFrameBelowClock { safe_block, clock } => {
                    record.reject_values.first_frame_below_clock =
                        sys::SchedulerFirstFrameBelowClock { safe_block, clock };
                    sys::SCHEDULER_REJECT_FIRST_FRAME_BELOW_CLOCK
                }
            };
        }
    }
    record
}

/// A span over bytes the handle keeps until its next `next_tx`, `feed` or destroy.
fn span(bytes: &[u8]) -> sys::ApplicationEngineByteSpan {
    sys::ApplicationEngineByteSpan {
        data: if bytes.is_empty() {
            std::ptr::null()
        } else {
            bytes.as_ptr()
        },
        size: u64::try_from(bytes.len()).expect("a slice length fits in u64 on supported hosts"),
    }
}

fn tx_record(scheduled: &Scheduled) -> sys::SchedulerTx {
    let mut record = sys::SchedulerTx {
        batch_nonce: scheduled.batch_nonce,
        ..Default::default()
    };
    match &scheduled.tx {
        L2Tx::Direct(direct) => {
            record.kind = sys::SCHEDULER_TX_DIRECT;
            record.values.direct = sys::ApplicationEngineDirectInput {
                sender: address_record(direct.sender),
                input_index: direct.input_index,
                block_number: direct.block,
                timestamp: direct.timestamp,
                payload: span(&direct.payload),
            };
        }
        L2Tx::UserOp(op) => {
            record.kind = sys::SCHEDULER_TX_USER_OP;
            record.values.user_op = sys::ApplicationEngineUserOpTx {
                sender: address_record(op.sender),
                nonce: op.nonce,
                fee: op.fee,
                safe_block: op.block,
                data: span(&op.data),
            };
        }
    }
    record
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_abi_version() -> u32 {
    sys::SCHEDULER_ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_get_last_error_message() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Hand a new handle out, or report why there is none; `out_scheduler` is written only on OK.
///
/// # Safety
/// `out_scheduler` is null or writable.
unsafe fn hand_out(
    out_scheduler: *mut *mut sys::Scheduler,
    made: impl FnOnce() -> Result<Reference, Failure>,
) -> sys::SchedulerStatus {
    clear_error();
    if out_scheduler.is_null() {
        return Failure::misuse("null out_scheduler").report();
    }
    match made() {
        Ok(reference) => {
            unsafe { *out_scheduler = Handle::new(reference).into_raw() };
            sys::SCHEDULER_STATUS_OK
        }
        Err(failure) => failure.report(),
    }
}

/// # Safety
/// `config` is null or readable, and `out_scheduler` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scheduler_create(
    config: *const sys::SchedulerConfig,
    out_scheduler: *mut *mut sys::Scheduler,
) -> sys::SchedulerStatus {
    unsafe {
        hand_out(out_scheduler, || {
            let config = config_from(config)?;
            catch(|| Reference::genesis(config)).map_err(Failure::internal)
        })
    }
}

/// # Safety
/// `config` and `state` are null or readable; `queued` is null or holds `queued_count` readable
/// records whose payload spans are readable; `out_scheduler` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scheduler_resume(
    config: *const sys::SchedulerConfig,
    state: *const sys::SchedulerState,
    queued: *const sys::SchedulerInput,
    queued_count: u64,
    out_scheduler: *mut *mut sys::Scheduler,
) -> sys::SchedulerStatus {
    unsafe {
        hand_out(out_scheduler, || {
            let config = config_from(config)?;
            let state = state
                .as_ref()
                .ok_or_else(|| Failure::misuse("null state"))?;
            let queued: &[sys::SchedulerInput] = if queued_count == 0 {
                &[]
            } else {
                if queued.is_null() {
                    return Err(Failure::misuse("null queued with a non-zero queued_count"));
                }
                let len = addressable(queued_count, size_of::<sys::SchedulerInput>()).ok_or_else(
                    || Failure::misuse("queued_count exceeds this host's address space"),
                )?;
                std::slice::from_raw_parts(queued, len)
            };
            // Check every span first, so the reference copies each input once, as it reads it.
            for input in queued {
                payload_from(&input.payload)?;
            }
            let inputs = queued.iter().map(|input| {
                input_from(input).unwrap_or_else(|_| unreachable!("every span was checked"))
            });
            match catch(|| Reference::resume(config, state_from(state), inputs)) {
                Ok(Ok(reference)) => Ok(reference),
                Ok(Err(refused)) => Err(Failure {
                    status: sys::SCHEDULER_STATUS_RESUME_REFUSED,
                    message: refused.to_string(),
                }),
                Err(message) => Err(Failure::internal(message)),
            }
        })
    }
}

/// # Safety
/// `scheduler` is null or a handle from `scheduler_create` or `scheduler_resume`, not yet
/// destroyed and not in use on another thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scheduler_destroy(scheduler: *mut sys::Scheduler) {
    if !scheduler.is_null() {
        drop(unsafe { Box::from_raw(scheduler.cast::<Handle>()) });
    }
}

/// # Safety
/// As [`scheduler_destroy`] for `scheduler`, and `out_state` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scheduler_state(
    scheduler: *const sys::Scheduler,
    out_state: *mut sys::SchedulerState,
) -> sys::SchedulerStatus {
    clear_error();
    let handle = match unsafe { live(scheduler) } {
        Ok(handle) => handle,
        Err(failure) => return failure.report(),
    };
    if out_state.is_null() {
        return Failure::misuse("null out_state").report();
    }
    unsafe { *out_state = state_record(handle.reference.state()) };
    sys::SCHEDULER_STATUS_OK
}

/// # Safety
/// As [`scheduler_destroy`] for `scheduler`; `input` is null or readable with a readable
/// payload span; `out_outcome` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scheduler_feed(
    scheduler: *mut sys::Scheduler,
    input: *const sys::SchedulerInput,
    out_outcome: *mut sys::SchedulerInputOutcome,
) -> sys::SchedulerStatus {
    unsafe {
        on_live_handle(scheduler, |handle| {
            let input = input
                .as_ref()
                .ok_or_else(|| Failure::misuse("null input"))?;
            if out_outcome.is_null() {
                return Err(Failure::misuse("null out_outcome"));
            }
            if !handle.pulled_all {
                return Err(Failure::misuse(
                    "the previous input still has transactions: pull until DONE before feeding",
                ));
            }
            let state = handle.reference.state();
            if input.index != state.l1_input_count {
                return Err(Failure::misuse(format!(
                    "input index {} is not the next, {}",
                    input.index, state.l1_input_count
                )));
            }
            if state.l1_input_count == u64::MAX {
                return Err(Failure::no_successor("the L1 input count has no successor"));
            }
            let input = input_from(input)?;
            // The one panic left: an accepted batch whose nonce has no successor. Only the
            // submitter's input at the last nonce can reach it.
            let nonce_exhausted = input.sender == handle.reference.config().sequencer_address
                && state.next_batch_nonce == u64::MAX;
            handle.current = None;
            let outcome = catch(|| handle.reference.feed(input)).map_err(|message| {
                if nonce_exhausted {
                    Failure::no_successor(message)
                } else {
                    Failure::internal(message)
                }
            })?;
            handle.pulled_all = false;
            *out_outcome = outcome_record(outcome);
            Ok(sys::SCHEDULER_STATUS_OK)
        })
    }
}

/// # Safety
/// As [`scheduler_destroy`] for `scheduler`, and `out_tx` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn scheduler_next_tx(
    scheduler: *mut sys::Scheduler,
    out_tx: *mut sys::SchedulerTx,
) -> sys::SchedulerStatus {
    unsafe {
        on_live_handle(scheduler, |handle| {
            if out_tx.is_null() {
                return Err(Failure::misuse("null out_tx"));
            }
            handle.current = None;
            if handle.pulled_all {
                return Ok(sys::SCHEDULER_STATUS_DONE);
            }
            match catch(|| handle.reference.next_tx()).map_err(Failure::internal)? {
                None => {
                    handle.pulled_all = true;
                    Ok(sys::SCHEDULER_STATUS_DONE)
                }
                Some(scheduled) => {
                    let scheduled = handle.current.insert(scheduled);
                    *out_tx = tx_record(scheduled);
                    Ok(sys::SCHEDULER_STATUS_OK)
                }
            }
        })
    }
}
