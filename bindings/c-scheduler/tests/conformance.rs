// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Replays the guest and scheduler-only scenarios of `tests/fixtures/consensus_vectors_v2.json`
//! through the C functions of scheduler.h and nothing else: each input's outcome, every
//! transaction it hands out and the state after it must match the file, and each resume case
//! must resume or be refused as pinned. The file was computed once from the reference, so this is
//! the check a C port of the header is held to. It links this crate's export; a port replays the
//! file the same way in its own harness until the check is packaged to link another.

use std::collections::BTreeMap;
use std::ffi::CStr;

use alloy_primitives::{Address, hex};
use c_scheduler::sys;
use serde_json::{Value, json};

const CONSENSUS_VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/consensus_vectors_v2.json"
));

fn text(value: &Value) -> &str {
    value.as_str().expect("pinned string")
}

fn number(value: &Value) -> u64 {
    value.as_u64().expect("pinned number")
}

fn bytes(value: &Value) -> Vec<u8> {
    hex::decode(text(value)).expect("pinned hex bytes")
}

fn address(value: &Value) -> Address {
    text(value).parse().expect("pinned address")
}

fn hex_of(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn address_record(address: Address) -> sys::ApplicationEngineEthereumAddress {
    sys::ApplicationEngineEthereumAddress {
        bytes: address.into_array(),
    }
}

/// Panics with the implementation's message on a failure status.
fn check(status: sys::SchedulerStatus, call: &str) -> sys::SchedulerStatus {
    if status < 0 {
        let message = unsafe { CStr::from_ptr(sys::scheduler_get_last_error_message()) };
        panic!("{call} failed with {status}: {}", message.to_string_lossy());
    }
    status
}

/// Copy a span the implementation handed out; it borrows the handle until the next call.
fn span_bytes(span: &sys::ApplicationEngineByteSpan) -> Vec<u8> {
    if span.size == 0 {
        return Vec::new();
    }
    let len = usize::try_from(span.size).expect("a span fits this host");
    unsafe { std::slice::from_raw_parts(span.data, len) }.to_vec()
}

/// One input of a scenario, kept alive while its record borrows the payload.
struct Input {
    sender: Address,
    block: u64,
    timestamp: u64,
    payload: Vec<u8>,
}

/// The outcome in the file's form: `outcome`, and `reject` for a rejected batch.
fn rendered_outcome(record: &sys::SchedulerInputOutcome) -> Value {
    let outcome = match record.kind {
        sys::SCHEDULER_DIRECT_QUEUED => "direct_queued",
        sys::SCHEDULER_DIRECT_DROPPED => "direct_dropped",
        sys::SCHEDULER_BATCH_ACCEPTED => "batch_accepted",
        sys::SCHEDULER_BATCH_STALE => "batch_stale",
        sys::SCHEDULER_BATCH_REJECTED => "batch_rejected",
        other => panic!("unknown outcome kind {other}"),
    };
    if record.kind != sys::SCHEDULER_BATCH_REJECTED {
        return json!({"outcome": outcome});
    }
    let reject = match record.reject_reason {
        sys::SCHEDULER_REJECT_DECODE_FAILED => json!({"reason": "decode_failed"}),
        sys::SCHEDULER_REJECT_WRONG_NONCE => {
            let values = unsafe { record.reject_values.wrong_nonce };
            json!({"reason": "wrong_nonce", "expected": values.expected, "got": values.got})
        }
        sys::SCHEDULER_REJECT_SAFE_BLOCK_NOT_BELOW_INCLUSION_BLOCK => {
            json!({"reason": "safe_block_not_below_inclusion_block"})
        }
        sys::SCHEDULER_REJECT_NON_MONOTONIC_SAFE_BLOCKS => {
            json!({"reason": "non_monotonic_safe_blocks"})
        }
        sys::SCHEDULER_REJECT_FEE_PRICE_OUT_OF_RANGE => json!({"reason": "fee_price_out_of_range"}),
        sys::SCHEDULER_REJECT_FIRST_FRAME_BELOW_CLOCK => {
            let values = unsafe { record.reject_values.first_frame_below_clock };
            json!({
                "reason": "first_frame_below_clock",
                "safe_block": values.safe_block,
                "clock": values.clock,
            })
        }
        other => panic!("unknown reject reason {other}"),
    };
    json!({"outcome": outcome, "reject": reject})
}

/// A handed-out transaction in the file's form, without its fate, which the executor decides.
/// A direct must carry back the input it was admitted from, whole.
fn rendered_tx(record: &sys::SchedulerTx, inputs: &BTreeMap<u64, Input>) -> Value {
    match record.kind {
        sys::SCHEDULER_TX_DIRECT => {
            let direct = unsafe { record.values.direct };
            let index = direct.input_index;
            let input = inputs
                .get(&index)
                .unwrap_or_else(|| panic!("direct {index} names no input given"));
            assert_eq!(
                Address::from(direct.sender.bytes),
                input.sender,
                "direct {index}"
            );
            assert_eq!(direct.block_number, input.block, "direct {index}");
            assert_eq!(direct.timestamp, input.timestamp, "direct {index}");
            assert_eq!(span_bytes(&direct.payload), input.payload, "direct {index}");
            json!({"direct": direct.input_index, "batch_nonce": record.batch_nonce})
        }
        sys::SCHEDULER_TX_USER_OP => {
            let op = unsafe { record.values.user_op };
            json!({
                "user_op": {
                    "sender": Address::from(op.sender.bytes).to_string(),
                    "nonce": op.nonce,
                    "fee": op.fee,
                    "block": op.safe_block,
                    "data": hex_of(&span_bytes(&op.data)),
                },
                "batch_nonce": record.batch_nonce,
            })
        }
        other => panic!("unknown transaction kind {other}"),
    }
}

/// A handle from the implementation, destroyed on every exit.
struct Handle(*mut sys::Scheduler);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { sys::scheduler_destroy(self.0) };
    }
}

/// The file's inputs: senders and payloads by name from the guest section.
struct Pinned<'a> {
    guest: &'a Value,
    chain_id: u64,
    app_contract: Address,
}

impl Pinned<'_> {
    fn input(&self, record: &Value) -> Input {
        Input {
            sender: address(&self.guest["senders"][text(&record["from"])]),
            block: number(&record["block"]),
            timestamp: number(&record["timestamp"]),
            payload: bytes(&self.guest["payloads"][text(&record["payload"])]),
        }
    }

    /// The header's record of `input`, borrowing its payload.
    fn record(&self, index: u64, input: &Input) -> sys::SchedulerInput {
        sys::SchedulerInput {
            index,
            sender: address_record(input.sender),
            block_number: input.block,
            timestamp: input.timestamp,
            chain_id: self.chain_id,
            app_contract: address_record(self.app_contract),
            payload: sys::ApplicationEngineByteSpan {
                data: input.payload.as_ptr(),
                size: u64::try_from(input.payload.len()).expect("fits"),
            },
        }
    }
}

fn pinned_config(pinned: &Value) -> sys::SchedulerConfig {
    sys::SchedulerConfig {
        sequencer_address: address_record(address(&pinned["sequencer_address"])),
        max_wait_blocks: number(&pinned["max_wait_blocks"]),
        fridge_bound: number(&pinned["fridge_bound"]),
        max_direct_payload_bytes: number(&pinned["max_direct_payload_bytes"]),
    }
}

fn state_fields(state: &sys::SchedulerState) -> [u64; 5] {
    [
        state.l1_input_count,
        state.next_batch_nonce,
        state.clock,
        state.first_queued_input,
        state.fridge_len,
    ]
}

fn pinned_state_fields(pinned: &Value) -> [u64; 5] {
    [
        number(&pinned["l1_input_count"]),
        number(&pinned["next_batch_nonce"]),
        number(&pinned["clock"]),
        number(&pinned["first_queued_input"]),
        number(&pinned["fridge_len"]),
    ]
}

fn read_state(handle: &Handle) -> sys::SchedulerState {
    let mut state = sys::SchedulerState::default();
    check(
        unsafe { sys::scheduler_state(handle.0, &mut state) },
        "scheduler_state",
    );
    state
}

/// Feeds the step as input `index` and compares its outcome, every transaction it hands out
/// and the state after it with the file.
fn replay_step(
    pinned: &Pinned,
    handle: &Handle,
    inputs: &mut BTreeMap<u64, Input>,
    index: u64,
    step: &Value,
    label: &str,
) {
    let input = inputs.entry(index).or_insert(pinned.input(step));
    let record = pinned.record(index, input);
    let mut outcome = sys::SchedulerInputOutcome::default();
    check(
        unsafe { sys::scheduler_feed(handle.0, &record, &mut outcome) },
        "scheduler_feed",
    );
    let mut expected = json!({"outcome": step["outcome"].clone()});
    if let Some(reject) = step.get("reject") {
        expected["reject"] = reject.clone();
    }
    assert_eq!(
        rendered_outcome(&outcome),
        expected,
        "{label}, step {index}"
    );

    let mut handed_out = Vec::new();
    loop {
        let mut tx = sys::SchedulerTx::default();
        let status = check(
            unsafe { sys::scheduler_next_tx(handle.0, &mut tx) },
            "scheduler_next_tx",
        );
        match status {
            sys::SCHEDULER_STATUS_OK => handed_out.push(rendered_tx(&tx, inputs)),
            sys::SCHEDULER_STATUS_DONE => break,
            other => panic!("unknown status {other}"),
        }
    }
    let pinned_txs: Vec<Value> = step["txs"]
        .as_array()
        .expect("pinned txs")
        .iter()
        .map(|tx| {
            let mut tx = tx.clone();
            tx.as_object_mut().expect("pinned tx").remove("fate");
            tx
        })
        .collect();
    assert_eq!(handed_out, pinned_txs, "{label}, step {index}");

    assert_eq!(
        state_fields(&read_state(handle)),
        pinned_state_fields(&step["scheduler"]),
        "{label}, step {index}: l1_input_count, next_batch_nonce, clock, \
         first_queued_input, fridge_len"
    );
}

/// Replays `scenario` on a scheduler created at genesis under `config`.
fn replay_scenario(pinned: &Pinned, config: &sys::SchedulerConfig, scenario: &Value) {
    let name = text(&scenario["name"]);
    let mut raw = std::ptr::null_mut();
    check(
        unsafe { sys::scheduler_create(config, &mut raw) },
        "scheduler_create",
    );
    let handle = Handle(raw);
    let mut inputs = BTreeMap::new();
    for (index, step) in (0_u64..).zip(scenario["steps"].as_array().expect("pinned steps")) {
        replay_step(pinned, &handle, &mut inputs, index, step, name);
    }
}

fn parsed_vectors() -> Value {
    serde_json::from_str(CONSENSUS_VECTORS).expect("parse consensus_vectors_v2.json")
}

fn pinned_inputs(vectors: &Value) -> Pinned<'_> {
    let guest = &vectors["guest"];
    Pinned {
        guest,
        chain_id: number(&guest["chain_id"]),
        app_contract: address(&guest["app_contract"]),
    }
}

#[test]
fn every_guest_scenario_replays_through_the_header() {
    assert_eq!(
        unsafe { sys::scheduler_abi_version() },
        sys::SCHEDULER_ABI_VERSION
    );
    let vectors = parsed_vectors();
    let pinned = pinned_inputs(&vectors);
    let config = pinned_config(&vectors["guest"]["config"]);
    let scenarios = vectors["guest"]["scenarios"]
        .as_array()
        .expect("pinned scenarios");
    assert!(!scenarios.is_empty());
    for scenario in scenarios {
        replay_scenario(&pinned, &config, scenario);
    }
}

/// The overflow drain at a small fridge bound, which the guest scenarios never reach.
#[test]
fn every_scheduler_only_scenario_replays_through_the_header() {
    let vectors = parsed_vectors();
    let pinned = pinned_inputs(&vectors);
    let section = &vectors["scheduler_only"];
    let config = pinned_config(&section["config"]);
    let scenarios = section["scenarios"].as_array().expect("pinned scenarios");
    assert!(!scenarios.is_empty());
    for scenario in scenarios {
        replay_scenario(&pinned, &config, scenario);
    }
}

/// scheduler_resume takes or refuses each pinned state and range; a resumed scheduler then
/// continues as pinned. The header reports one refusal status, so the reason is not compared.
#[test]
fn every_resume_case_replays_through_the_header() {
    let vectors = parsed_vectors();
    let pinned = pinned_inputs(&vectors);
    let section = &vectors["scheduler_only"];
    let config = pinned_config(&section["config"]);
    let cases = section["resume"].as_array().expect("pinned resume cases");
    assert!(!cases.is_empty());
    for case in cases {
        let name = text(&case["name"]);
        let fields = pinned_state_fields(&case["state"]);
        let state = sys::SchedulerState {
            l1_input_count: fields[0],
            next_batch_nonce: fields[1],
            clock: fields[2],
            first_queued_input: fields[3],
            fridge_len: fields[4],
        };
        let mut inputs = BTreeMap::new();
        for record in case["queued"].as_array().expect("pinned range") {
            inputs.insert(number(&record["index"]), pinned.input(record));
        }
        let records: Vec<sys::SchedulerInput> = case["queued"]
            .as_array()
            .expect("pinned range")
            .iter()
            .map(|record| {
                let index = number(&record["index"]);
                pinned.record(index, &inputs[&index])
            })
            .collect();
        let mut raw = std::ptr::null_mut();
        let status = unsafe {
            sys::scheduler_resume(
                &config,
                &state,
                if records.is_empty() {
                    std::ptr::null()
                } else {
                    records.as_ptr()
                },
                u64::try_from(records.len()).expect("fits"),
                &mut raw,
            )
        };
        if text(&case["result"]) == "refused" {
            assert_eq!(status, sys::SCHEDULER_STATUS_RESUME_REFUSED, "{name}");
            assert!(raw.is_null(), "{name}: a refused resume yields no handle");
            continue;
        }
        let handle = Handle(raw);
        check(status, "scheduler_resume");
        assert_eq!(state_fields(&read_state(&handle)), fields, "{name}");
        let then = case["then"]
            .as_array()
            .expect("pinned steps after a resume");
        for (index, step) in (state.l1_input_count..).zip(then) {
            replay_step(&pinned, &handle, &mut inputs, index, step, name);
        }
    }
}
