// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Drives the exported functions through the header's own declarations and compares them,
//! transaction by transaction and state by state, with the Rust reference on the same inputs.

use std::ffi::CStr;

use alloy_primitives::Address;
use alloy_sol_types::Eip712Domain;
use c_scheduler::sys;
use k256::ecdsa::SigningKey;
use sequencer_core::batch::{Batch, Frame, WireUserOp};
use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
use sequencer_core::scheduler::{
    BatchRejectReason, InputOutcome, L1Input, Scheduled, Scheduler, SchedulerConfig, SchedulerState,
};
use sequencer_core::user_op::UserOp;

const SEQUENCER: Address = Address::repeat_byte(0x11);
const USER: Address = Address::repeat_byte(0x22);
const CHAIN_ID: u64 = 1;
const APP_CONTRACT: Address = Address::repeat_byte(0x33);

fn config(
    max_wait_blocks: u64,
    fridge_bound: u64,
    max_direct_payload_bytes: u64,
) -> SchedulerConfig {
    SchedulerConfig {
        sequencer_address: SEQUENCER,
        max_wait_blocks,
        fridge_bound,
        max_direct_payload_bytes,
    }
}

fn domain() -> Eip712Domain {
    sequencer_core::build_input_domain(CHAIN_ID, APP_CONTRACT)
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes((&[seed; 32]).into()).expect("signing key")
}

fn op(key: &SigningKey, nonce: u32, max_fee: u16, data: &[u8]) -> WireUserOp {
    let user_op = UserOp {
        nonce,
        max_fee,
        data: data.to_vec().into(),
    };
    let signature = sequencer_core::test_support::sign_user_op(key, &domain(), &user_op);
    WireUserOp {
        nonce,
        max_fee,
        data: data.to_vec(),
        signature: signature.as_bytes().to_vec(),
    }
}

fn frame(safe_block: u64, fee_price: u16, user_ops: Vec<WireUserOp>) -> Frame {
    Frame {
        user_ops,
        safe_block,
        fee_price,
    }
}

/// Inputs in InputBox order from index `base`: each builder takes the next index.
struct Script {
    base: u64,
    inputs: Vec<L1Input>,
}

impl Script {
    fn new() -> Self {
        Self::from_index(0)
    }

    fn from_index(base: u64) -> Self {
        Self {
            base,
            inputs: Vec::new(),
        }
    }

    fn push(&mut self, sender: Address, block: u64, payload: Vec<u8>) -> &mut Self {
        let index = self.base + u64::try_from(self.inputs.len()).expect("fits");
        self.inputs.push(L1Input {
            index,
            sender,
            block,
            timestamp: block * 12,
            chain_id: CHAIN_ID,
            app_contract: APP_CONTRACT,
            payload,
        });
        self
    }

    fn direct(&mut self, block: u64, payload: &[u8]) -> &mut Self {
        self.push(USER, block, payload.to_vec())
    }

    fn batch(&mut self, block: u64, nonce: u64, frames: Vec<Frame>) -> &mut Self {
        self.push(
            SEQUENCER,
            block,
            ssz::Encode::as_ssz_bytes(&Batch { nonce, frames }),
        )
    }

    fn raw_batch(&mut self, block: u64, payload: &[u8]) -> &mut Self {
        self.push(SEQUENCER, block, payload.to_vec())
    }

    fn inputs(&self) -> &[L1Input] {
        &self.inputs
    }
}

type CallError = (sys::SchedulerStatus, String);

fn last_error() -> String {
    let message = unsafe { CStr::from_ptr(sys::scheduler_get_last_error_message()) };
    message.to_string_lossy().into_owned()
}

fn check(status: sys::SchedulerStatus) -> Result<sys::SchedulerStatus, CallError> {
    if status < 0 {
        Err((status, last_error()))
    } else {
        Ok(status)
    }
}

fn address_record(address: Address) -> sys::ApplicationEngineEthereumAddress {
    sys::ApplicationEngineEthereumAddress {
        bytes: address.into_array(),
    }
}

fn config_record(config: &SchedulerConfig) -> sys::SchedulerConfig {
    sys::SchedulerConfig {
        sequencer_address: address_record(config.sequencer_address),
        max_wait_blocks: config.max_wait_blocks,
        fridge_bound: config.fridge_bound,
        max_direct_payload_bytes: config.max_direct_payload_bytes,
    }
}

fn state_record(state: &SchedulerState) -> sys::SchedulerState {
    sys::SchedulerState {
        l1_input_count: state.l1_input_count,
        next_batch_nonce: state.next_batch_nonce,
        clock: state.clock,
        first_queued_input: state.first_queued_input,
        fridge_len: state.fridge_len,
    }
}

/// An input record borrowing `input`'s payload, which must outlive the call it is passed to.
fn input_record(input: &L1Input) -> sys::SchedulerInput {
    sys::SchedulerInput {
        index: input.index,
        sender: address_record(input.sender),
        block_number: input.block,
        timestamp: input.timestamp,
        chain_id: input.chain_id,
        app_contract: address_record(input.app_contract),
        payload: sys::ApplicationEngineByteSpan {
            data: input.payload.as_ptr(),
            size: u64::try_from(input.payload.len()).expect("fits"),
        },
    }
}

/// Copy a span the export handed out; it borrows the handle until the next call.
fn bytes(span: &sys::ApplicationEngineByteSpan) -> Vec<u8> {
    if span.size == 0 {
        return Vec::new();
    }
    let len = usize::try_from(span.size).expect("fits");
    unsafe { std::slice::from_raw_parts(span.data, len) }.to_vec()
}

fn outcome_from(record: &sys::SchedulerInputOutcome) -> InputOutcome {
    match record.kind {
        sys::SCHEDULER_DIRECT_QUEUED => InputOutcome::DirectQueued,
        sys::SCHEDULER_DIRECT_DROPPED => InputOutcome::DirectDropped,
        sys::SCHEDULER_BATCH_ACCEPTED => InputOutcome::BatchAccepted,
        sys::SCHEDULER_BATCH_STALE => InputOutcome::BatchStale,
        sys::SCHEDULER_BATCH_REJECTED => InputOutcome::BatchRejected(match record.reject_reason {
            sys::SCHEDULER_REJECT_DECODE_FAILED => BatchRejectReason::DecodeFailed,
            sys::SCHEDULER_REJECT_WRONG_NONCE => {
                let values = unsafe { record.reject_values.wrong_nonce };
                BatchRejectReason::WrongNonce {
                    expected: values.expected,
                    got: values.got,
                }
            }
            sys::SCHEDULER_REJECT_SAFE_BLOCK_NOT_BELOW_INCLUSION_BLOCK => {
                BatchRejectReason::SafeBlockNotBelowInclusionBlock
            }
            sys::SCHEDULER_REJECT_NON_MONOTONIC_SAFE_BLOCKS => {
                BatchRejectReason::NonMonotonicSafeBlocks
            }
            sys::SCHEDULER_REJECT_FEE_PRICE_OUT_OF_RANGE => BatchRejectReason::FeePriceOutOfRange,
            sys::SCHEDULER_REJECT_FIRST_FRAME_BELOW_CLOCK => {
                let values = unsafe { record.reject_values.first_frame_below_clock };
                BatchRejectReason::FirstFrameBelowClock {
                    safe_block: values.safe_block,
                    clock: values.clock,
                }
            }
            other => panic!("unknown reject reason {other}"),
        }),
        other => panic!("unknown outcome kind {other}"),
    }
}

fn scheduled_from(record: &sys::SchedulerTx) -> Scheduled {
    let tx = match record.kind {
        sys::SCHEDULER_TX_DIRECT => {
            let direct = unsafe { record.values.direct };
            L2Tx::Direct(Direct {
                input_index: direct.input_index,
                sender: Address::from(direct.sender.bytes),
                block: direct.block_number,
                timestamp: direct.timestamp,
                payload: bytes(&direct.payload),
            })
        }
        sys::SCHEDULER_TX_USER_OP => {
            let op = unsafe { record.values.user_op };
            L2Tx::UserOp(UserOpTx {
                sender: Address::from(op.sender.bytes),
                nonce: op.nonce,
                fee: op.fee,
                block: op.safe_block,
                data: bytes(&op.data),
            })
        }
        other => panic!("unknown transaction kind {other}"),
    };
    Scheduled {
        batch_nonce: record.batch_nonce,
        tx,
    }
}

/// A handle from the export, used only through the header's declarations.
struct CScheduler(*mut sys::Scheduler);

impl CScheduler {
    fn create(config: &SchedulerConfig) -> Result<Self, CallError> {
        let mut handle = std::ptr::null_mut();
        check(unsafe { sys::scheduler_create(&config_record(config), &mut handle) })?;
        assert!(!handle.is_null());
        Ok(Self(handle))
    }

    fn resume(
        config: &SchedulerConfig,
        state: &SchedulerState,
        queued: &[L1Input],
    ) -> Result<Self, CallError> {
        let records: Vec<_> = queued.iter().map(input_record).collect();
        let mut handle = std::ptr::null_mut();
        check(unsafe {
            sys::scheduler_resume(
                &config_record(config),
                &state_record(state),
                if records.is_empty() {
                    std::ptr::null()
                } else {
                    records.as_ptr()
                },
                u64::try_from(records.len()).expect("fits"),
                &mut handle,
            )
        })?;
        assert!(!handle.is_null());
        Ok(Self(handle))
    }

    fn state(&self) -> Result<SchedulerState, CallError> {
        let mut record = sys::SchedulerState::default();
        check(unsafe { sys::scheduler_state(self.0, &mut record) })?;
        Ok(SchedulerState {
            l1_input_count: record.l1_input_count,
            next_batch_nonce: record.next_batch_nonce,
            clock: record.clock,
            first_queued_input: record.first_queued_input,
            fridge_len: record.fridge_len,
        })
    }

    fn feed(&mut self, input: &L1Input) -> Result<InputOutcome, CallError> {
        let mut record = sys::SchedulerInputOutcome::default();
        check(unsafe { sys::scheduler_feed(self.0, &input_record(input), &mut record) })?;
        Ok(outcome_from(&record))
    }

    fn next_tx(&mut self) -> Result<Option<Scheduled>, CallError> {
        let mut record = sys::SchedulerTx::default();
        match check(unsafe { sys::scheduler_next_tx(self.0, &mut record) })? {
            sys::SCHEDULER_STATUS_OK => Ok(Some(scheduled_from(&record))),
            sys::SCHEDULER_STATUS_DONE => Ok(None),
            other => panic!("unknown status {other}"),
        }
    }
}

impl Drop for CScheduler {
    fn drop(&mut self) {
        unsafe { sys::scheduler_destroy(self.0) };
    }
}

/// Feed both, comparing every outcome, transaction and state; returns the outcomes and the
/// transactions handed out, for the scenario's own assertions.
fn run_both(
    reference: &mut Scheduler,
    exported: &mut CScheduler,
    inputs: &[L1Input],
) -> (Vec<InputOutcome>, Vec<Scheduled>) {
    let mut outcomes = Vec::new();
    let mut handed_out = Vec::new();
    for input in inputs {
        let expected = reference.feed(input.clone());
        let got = exported.feed(input).expect("feed");
        assert_eq!(got, expected, "outcome of input {}", input.index);
        outcomes.push(expected);
        loop {
            let expected = reference.next_tx();
            let got = exported.next_tx().expect("next_tx");
            assert_eq!(got, expected, "transaction of input {}", input.index);
            let Some(scheduled) = expected else { break };
            handed_out.push(scheduled);
        }
        assert_eq!(
            exported.state().expect("state"),
            reference.state(),
            "state after input {}",
            input.index
        );
    }
    (outcomes, handed_out)
}

fn run_from_genesis(
    config: &SchedulerConfig,
    inputs: &[L1Input],
) -> (Vec<InputOutcome>, Vec<Scheduled>) {
    let mut reference = Scheduler::genesis(*config);
    let mut exported = CScheduler::create(config).expect("create");
    assert_eq!(exported.state().expect("state"), reference.state());
    run_both(&mut reference, &mut exported, inputs)
}

fn user_op_data(scheduled: &[Scheduled]) -> Vec<Vec<u8>> {
    scheduled
        .iter()
        .filter_map(|scheduled| match &scheduled.tx {
            L2Tx::UserOp(op) => Some(op.data.clone()),
            L2Tx::Direct(_) => None,
        })
        .collect()
}

#[test]
fn abi_version_is_the_headers() {
    assert_eq!(
        unsafe { sys::scheduler_abi_version() },
        sys::SCHEDULER_ABI_VERSION
    );
}

#[test]
fn accepted_batches_hand_out_the_same_transactions() {
    let alice = key(1);
    let bob = key(2);
    let mut script = Script::new();
    script
        .direct(1, b"d1")
        .direct(2, b"d2")
        .direct(4, b"d4")
        .batch(
            6,
            0,
            vec![
                frame(2, 3, vec![op(&alice, 0, 3, b"a0"), op(&bob, 0, 9, b"b0")]),
                frame(4, 5, vec![op(&alice, 1, 5, b"a1")]),
            ],
        )
        .direct(7, b"d7")
        .batch(8, 1, Vec::new())
        .batch(9, 2, vec![frame(7, 0, vec![op(&bob, 7, 0, b"b7")])]);
    let (outcomes, handed_out) = run_from_genesis(&config(100, 100, 100), script.inputs());

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == InputOutcome::BatchAccepted)
            .count(),
        3
    );
    // Every op is handed out, whatever its nonce: that is the executor's check.
    assert_eq!(
        user_op_data(&handed_out),
        [b"a0".to_vec(), b"b0".into(), b"a1".into(), b"b7".into()]
    );
    assert_eq!(handed_out.len(), 8);
}

#[test]
fn fee_cap_and_signature_failures_are_never_handed_out() {
    let alice = key(1);
    // A zero r recovers no signer.
    let mut bad_signature = op(&alice, 1, 9, b"bad-sig");
    bad_signature.signature[..32].fill(0);
    let mut short_signature = op(&alice, 1, 9, b"short-sig");
    short_signature.signature.pop();
    let mut script = Script::new();
    script.batch(
        5,
        0,
        vec![frame(
            1,
            4,
            vec![
                op(&alice, 0, 3, b"under-cap"),
                op(&alice, 0, 4, b"at-cap"),
                bad_signature,
                short_signature,
                op(&alice, 1, 5, b"over-cap"),
            ],
        )],
    );
    let (_, handed_out) = run_from_genesis(&config(100, 100, 100), script.inputs());
    assert_eq!(
        user_op_data(&handed_out),
        [b"at-cap".to_vec(), b"over-cap".into()]
    );
}

#[test]
fn rejected_and_stale_batches_change_nothing_alike() {
    let alice = key(1);
    let mut script = Script::new();
    script
        .direct(1, b"d1")
        .raw_batch(2, b"not ssz")
        .batch(3, 5, Vec::new())
        .batch(4, 0, vec![frame(4, 0, Vec::new())])
        .batch(5, 0, vec![frame(3, 0, Vec::new()), frame(2, 0, Vec::new())])
        .batch(6, 0, vec![frame(1, u16::MAX, Vec::new())])
        .batch(8, 0, vec![frame(3, 0, vec![op(&alice, 0, 0, b"a0")])])
        .batch(9, 1, vec![frame(2, 0, Vec::new())])
        .direct(30, b"d30")
        .batch(40, 1, vec![frame(20, 0, Vec::new())]);
    let (outcomes, _) = run_from_genesis(&config(10, 100, 100), script.inputs());
    assert_eq!(
        outcomes,
        [
            InputOutcome::DirectQueued,
            InputOutcome::BatchRejected(BatchRejectReason::DecodeFailed),
            InputOutcome::BatchRejected(BatchRejectReason::WrongNonce {
                expected: 0,
                got: 5
            }),
            InputOutcome::BatchRejected(BatchRejectReason::SafeBlockNotBelowInclusionBlock),
            InputOutcome::BatchRejected(BatchRejectReason::NonMonotonicSafeBlocks),
            InputOutcome::BatchRejected(BatchRejectReason::FeePriceOutOfRange),
            InputOutcome::BatchAccepted,
            InputOutcome::BatchRejected(BatchRejectReason::FirstFrameBelowClock {
                safe_block: 2,
                clock: 3
            }),
            InputOutcome::DirectQueued,
            InputOutcome::BatchStale,
        ]
    );
}

#[test]
fn a_full_fridge_drains_its_oldest_blocks() {
    let mut script = Script::new();
    script
        .direct(1, b"a")
        .direct(1, b"b")
        .direct(2, b"c")
        .direct(3, b"d")
        .direct(3, b"e")
        .direct(3, b"f")
        .direct(4, b"g")
        .direct(5, &[0; 9])
        .direct(5, b"h");
    let (outcomes, handed_out) = run_from_genesis(&config(1_000, 2, 8), script.inputs());
    assert!(outcomes.contains(&InputOutcome::DirectDropped));
    // The bound is soft by one block: the arriving block's directs never drain.
    assert!(!handed_out.is_empty());
    assert!(handed_out.iter().all(|scheduled| match &scheduled.tx {
        L2Tx::Direct(direct) => direct.block < 5,
        L2Tx::UserOp(_) => false,
    }));
}

/// The inputs `[state.first_queued_input, state.l1_input_count)` from a run starting at 0.
fn queued_range<'a>(inputs: &'a [L1Input], state: &SchedulerState) -> &'a [L1Input] {
    let start = usize::try_from(state.first_queued_input).expect("fits");
    let end = usize::try_from(state.l1_input_count).expect("fits");
    &inputs[start..end]
}

#[test]
fn resume_with_queued_directs_continues_alike() {
    let alice = key(1);
    let config = config(20, 100, 4);
    let mut script = Script::new();
    script
        .direct(1, b"d1")
        .batch(3, 0, vec![frame(1, 0, vec![op(&alice, 0, 0, b"a0")])])
        .direct(4, b"d4")
        .direct(5, b"long!")
        .batch(6, 7, Vec::new())
        .direct(6, b"d6")
        .batch(7, 1, vec![frame(5, 0, Vec::new())])
        // The cut: d6 is still queued, behind a batch and a dropped direct.
        .direct(8, b"d8")
        .batch(9, 2, vec![frame(8, 0, vec![op(&alice, 1, 0, b"a1")])])
        .direct(40, b"d40");
    let inputs = script.inputs();
    let cut = 7;

    let mut uninterrupted = Scheduler::genesis(config);
    for input in &inputs[..cut] {
        uninterrupted.feed(input.clone());
        while uninterrupted.next_tx().is_some() {}
    }
    let state = uninterrupted.state();
    assert!(state.fridge_len > 0, "the cut must leave directs queued");
    let queued = queued_range(inputs, &state);
    assert!(queued.len() > usize::try_from(state.fridge_len).expect("fits"));

    let mut reference = Scheduler::resume(config, state, queued.to_vec()).expect("resume");
    let mut exported = CScheduler::resume(&config, &state, queued).expect("resume");
    assert_eq!(exported.state().expect("state"), state);
    let (_, resumed) = run_both(&mut reference, &mut exported, &inputs[cut..]);

    // And the resumed export continues exactly as the uninterrupted reference.
    let mut continued = Vec::new();
    for input in &inputs[cut..] {
        uninterrupted.feed(input.clone());
        continued.extend(std::iter::from_fn(|| uninterrupted.next_tx()));
    }
    assert_eq!(resumed, continued);
    assert_eq!(reference.state(), uninterrupted.state());
}

#[test]
fn refused_resume_writes_no_handle() {
    let config = config(20, 100, 100);
    let mut script = Script::new();
    script.direct(1, b"d1").direct(2, b"d2");
    let state = SchedulerState {
        l1_input_count: 2,
        next_batch_nonce: 0,
        clock: 0,
        first_queued_input: 0,
        fridge_len: 3,
    };
    let expected = Scheduler::resume(config, state, script.inputs().to_vec())
        .expect_err("the reference refuses it");

    let sentinel = std::ptr::dangling_mut::<sys::Scheduler>();
    let mut handle = sentinel;
    let records: Vec<_> = script.inputs().iter().map(input_record).collect();
    let status = unsafe {
        sys::scheduler_resume(
            &config_record(&config),
            &state_record(&state),
            records.as_ptr(),
            2,
            &mut handle,
        )
    };
    assert_eq!(status, sys::SCHEDULER_STATUS_RESUME_REFUSED);
    assert_eq!(handle, sentinel);
    assert_eq!(last_error(), expected.to_string());

    // A short range is refused the same way.
    let error = CScheduler::resume(
        &config,
        &SchedulerState {
            fridge_len: 2,
            ..state
        },
        &script.inputs()[..1],
    )
    .err()
    .expect("refused");
    assert_eq!(error.0, sys::SCHEDULER_STATUS_RESUME_REFUSED);
}

#[test]
fn bad_arguments_are_misuse_and_write_no_handle() {
    let sentinel = std::ptr::dangling_mut::<sys::Scheduler>();
    let mut handle = sentinel;

    let zero_window = config_record(&config(0, 1, 1));
    assert_eq!(
        unsafe { sys::scheduler_create(&zero_window, &mut handle) },
        sys::SCHEDULER_STATUS_MISUSE
    );
    assert!(last_error().contains("max_wait_blocks"));
    assert_eq!(
        unsafe { sys::scheduler_create(std::ptr::null(), &mut handle) },
        sys::SCHEDULER_STATUS_MISUSE
    );
    let good = config_record(&config(1, 1, 1));
    assert_eq!(
        unsafe { sys::scheduler_create(&good, std::ptr::null_mut()) },
        sys::SCHEDULER_STATUS_MISUSE
    );
    let state = state_record(&SchedulerState {
        l1_input_count: 1,
        next_batch_nonce: 0,
        clock: 0,
        first_queued_input: 0,
        fridge_len: 1,
    });
    assert_eq!(
        unsafe { sys::scheduler_resume(&good, &state, std::ptr::null(), 1, &mut handle) },
        sys::SCHEDULER_STATUS_MISUSE
    );
    let null_payload = sys::SchedulerInput {
        payload: sys::ApplicationEngineByteSpan {
            data: std::ptr::null(),
            size: 1,
        },
        ..Default::default()
    };
    assert_eq!(
        unsafe { sys::scheduler_resume(&good, &state, &null_payload, 1, &mut handle) },
        sys::SCHEDULER_STATUS_MISUSE
    );
    // Counts no host can address are refused before any memory is read.
    let records = std::ptr::NonNull::<sys::SchedulerInput>::dangling().as_ptr();
    assert_eq!(
        unsafe { sys::scheduler_resume(&good, &state, records, u64::MAX, &mut handle) },
        sys::SCHEDULER_STATUS_MISUSE
    );
    assert!(last_error().contains("address space"));
    let huge_payload = sys::SchedulerInput {
        payload: sys::ApplicationEngineByteSpan {
            data: std::ptr::NonNull::<u8>::dangling().as_ptr(),
            size: u64::MAX,
        },
        ..Default::default()
    };
    assert_eq!(
        unsafe { sys::scheduler_resume(&good, &state, &huge_payload, 1, &mut handle) },
        sys::SCHEDULER_STATUS_MISUSE
    );
    assert!(last_error().contains("address space"));
    assert_eq!(handle, sentinel);

    // A success clears the message.
    let exported = CScheduler::create(&config(1, 1, 1)).expect("create");
    assert_eq!(last_error(), "");
    // Reading the state with nowhere to write it leaves the handle alive.
    assert_eq!(
        unsafe { sys::scheduler_state(exported.0, std::ptr::null_mut()) },
        sys::SCHEDULER_STATUS_MISUSE
    );
    assert!(exported.state().is_ok());
    unsafe { sys::scheduler_destroy(std::ptr::null_mut()) };
}

/// After a failure every call but destroy is MISUSE.
fn assert_dead(exported: &mut CScheduler, next: &L1Input) {
    assert_eq!(
        exported.state().err().map(|error| error.0),
        Some(sys::SCHEDULER_STATUS_MISUSE)
    );
    assert_eq!(
        exported.next_tx().err().map(|error| error.0),
        Some(sys::SCHEDULER_STATUS_MISUSE)
    );
    assert_eq!(
        exported.feed(next).err().map(|error| error.0),
        Some(sys::SCHEDULER_STATUS_MISUSE)
    );
    assert!(last_error().contains("dead"));
}

#[test]
fn an_input_out_of_order_kills_the_handle() {
    let mut script = Script::new();
    script.direct(1, b"d0").direct(2, b"d1");
    let mut exported = CScheduler::create(&config(10, 10, 10)).expect("create");
    let error = exported
        .feed(&script.inputs()[1])
        .expect_err("out of order");
    assert_eq!(error.0, sys::SCHEDULER_STATUS_MISUSE);
    assert!(error.1.contains("not the next"), "{}", error.1);
    assert_dead(&mut exported, &script.inputs()[0]);
}

#[test]
fn a_feed_before_done_kills_the_handle() {
    let alice = key(1);
    let mut script = Script::new();
    script
        .batch(
            3,
            0,
            vec![frame(
                1,
                0,
                vec![op(&alice, 0, 0, b"a0"), op(&alice, 1, 0, b"a1")],
            )],
        )
        .direct(4, b"d4");
    let mut exported = CScheduler::create(&config(10, 10, 10)).expect("create");
    exported.feed(&script.inputs()[0]).expect("feed");
    assert!(exported.next_tx().expect("next_tx").is_some());
    let error = exported
        .feed(&script.inputs()[1])
        .expect_err("transactions left");
    assert_eq!(error.0, sys::SCHEDULER_STATUS_MISUSE);
    assert_dead(&mut exported, &script.inputs()[1]);
}

#[test]
fn done_comes_before_the_first_feed_and_repeats() {
    let mut script = Script::new();
    script.direct(1, b"d1");
    let mut exported = CScheduler::create(&config(10, 10, 10)).expect("create");
    assert_eq!(exported.next_tx().expect("next_tx"), None);
    exported.feed(&script.inputs()[0]).expect("feed");
    assert_eq!(exported.next_tx().expect("next_tx"), None);
    assert_eq!(exported.next_tx().expect("next_tx"), None);
}

#[test]
fn an_exhausted_batch_nonce_halts_like_the_reference() {
    let config = config(10, 10, 10);
    let state = SchedulerState {
        l1_input_count: 0,
        next_batch_nonce: u64::MAX,
        clock: 0,
        first_queued_input: 0,
        fridge_len: 0,
    };
    let mut script = Script::new();
    script
        .batch(3, 0, Vec::new())
        .batch(4, u64::MAX, Vec::new());

    // A rejected batch at the last nonce changes nothing and halts nothing.
    let mut reference = Scheduler::resume(config, state, Vec::new()).expect("resume");
    let mut exported = CScheduler::resume(&config, &state, &[]).expect("resume");
    run_both(&mut reference, &mut exported, &script.inputs()[..1]);

    let halted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reference.feed(script.inputs()[1].clone())
    }));
    assert!(
        halted.is_err(),
        "the reference halts on an accepted batch at u64::MAX"
    );
    let error = exported
        .feed(&script.inputs()[1])
        .expect_err("no successor");
    assert_eq!(error.0, sys::SCHEDULER_STATUS_NO_SUCCESSOR);
    assert_dead(&mut exported, &script.inputs()[1]);
}

#[test]
fn an_exhausted_input_count_halts() {
    let config = config(10, 10, 10);
    let state = SchedulerState {
        l1_input_count: u64::MAX,
        next_batch_nonce: 0,
        clock: 0,
        first_queued_input: u64::MAX,
        fridge_len: 0,
    };
    let mut script = Script::from_index(u64::MAX);
    script.direct(1, b"d");
    let mut exported = CScheduler::resume(&config, &state, &[]).expect("resume");
    let error = exported
        .feed(&script.inputs()[0])
        .expect_err("no successor");
    assert_eq!(error.0, sys::SCHEDULER_STATUS_NO_SUCCESSOR);
    assert_dead(&mut exported, &script.inputs()[0]);
}

#[test]
fn spans_stay_valid_across_a_state_read() {
    let mut script = Script::new();
    script
        .direct(1, b"payload")
        .batch(3, 0, vec![frame(2, 0, Vec::new())]);
    let mut exported = CScheduler::create(&config(10, 10, 10)).expect("create");
    exported.feed(&script.inputs()[0]).expect("feed");
    assert_eq!(exported.next_tx().expect("next_tx"), None);
    exported.feed(&script.inputs()[1]).expect("feed");
    let mut record = sys::SchedulerTx::default();
    assert_eq!(
        unsafe { sys::scheduler_next_tx(exported.0, &mut record) },
        sys::SCHEDULER_STATUS_OK
    );
    exported.state().expect("state");
    assert_eq!(bytes(&unsafe { record.values.direct }.payload), b"payload");
}

/// A small deterministic generator, so a failing run replays from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// Random inputs that exercise every outcome: directs over and under the cap, batches at the
/// right nonce and not, frames old and new, ops over and under the price, bad signatures.
fn random_inputs(rng: &mut Rng, keys: &[SigningKey], count: usize) -> Vec<L1Input> {
    let mut script = Script::new();
    let mut block = 1;
    let mut nonce = 0;
    for _ in 0..count {
        block += rng.below(4);
        if rng.chance(60) {
            let len = usize::try_from(rng.below(10)).expect("fits");
            script.direct(
                block,
                &vec![u8::try_from(rng.below(256)).expect("fits"); len],
            );
            continue;
        }
        let batch_nonce = if rng.chance(85) {
            nonce
        } else {
            nonce + 1 + rng.below(2)
        };
        let mut frames = Vec::new();
        let mut safe_block = block.saturating_sub(1 + rng.below(30));
        for _ in 0..rng.below(3) {
            let price = u16::try_from(rng.below(6)).expect("fits");
            let ops = (0..rng.below(3))
                .map(|_| {
                    let signer = &keys[usize::try_from(rng.below(3)).expect("fits")];
                    let mut wire = op(
                        signer,
                        u32::try_from(rng.below(4)).expect("fits"),
                        u16::try_from(rng.below(8)).expect("fits"),
                        &rng.next().to_le_bytes(),
                    );
                    // A flipped bit recovers another signer; a zero r recovers none.
                    if rng.chance(10) {
                        wire.signature[10] ^= 1;
                    } else if rng.chance(5) {
                        wire.signature[..32].fill(0);
                    }
                    wire
                })
                .collect();
            frames.push(frame(safe_block, price, ops));
            safe_block = (safe_block + rng.below(3)).min(block - 1);
        }
        if rng.chance(5) {
            script.raw_batch(block, &rng.next().to_le_bytes());
            continue;
        }
        script.batch(block, batch_nonce, frames);
        if batch_nonce == nonce {
            // Accepted or not, the generator guesses; a wrong guess only means more rejections.
            nonce += u64::from(rng.chance(70));
        }
    }
    script.inputs().to_vec()
}

#[test]
fn random_runs_match_the_reference_from_genesis_and_from_every_resume() {
    let keys = [key(1), key(2), key(3)];
    for seed in 0..8 {
        let mut rng = Rng(seed);
        let config = config(1 + rng.below(25), 1 + rng.below(6), rng.below(9));
        let inputs = random_inputs(&mut rng, &keys, 80);
        let (outcomes, _) = run_from_genesis(&config, &inputs);
        assert!(
            outcomes.contains(&InputOutcome::DirectQueued),
            "seed {seed}"
        );

        // Resume the export at several cuts and compare it with the reference resumed there.
        let mut uninterrupted = Scheduler::genesis(config);
        for (cut, input) in inputs.iter().enumerate() {
            if cut % 11 == 0 {
                let state = uninterrupted.state();
                let queued = queued_range(&inputs, &state);
                let mut reference =
                    Scheduler::resume(config, state, queued.to_vec()).expect("resume");
                let mut exported = CScheduler::resume(&config, &state, queued).expect("resume");
                run_both(&mut reference, &mut exported, &inputs[cut..]);
            }
            uninterrupted.feed(input.clone());
            while uninterrupted.next_tx().is_some() {}
        }
    }
}
