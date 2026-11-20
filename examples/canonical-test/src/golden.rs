// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Golden guest cases from `tests/fixtures/consensus_vectors_v2.json`: every
//! guest scenario replayed in the built image from genesis. Pinned payloads go
//! in unchanged (nothing is signed or encoded here), and the raw outputs, the
//! `scheduler` report after every input and the final `state` report must equal
//! the pinned bytes; a mismatch is a finding. A rejected batch's report is a
//! diagnostic, so only whether one appears is checked.

use std::collections::BTreeMap;

use app_core::application::{DEVNET_SEQUENCER_ADDRESS, WalletApp};
use canonical_app::{INVALID_BATCH_REPORT, SCHEDULER_INSPECT_QUERY, STATE_INSPECT_QUERY};
use rollups_types::alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use sequencer_core::scheduler::{SchedulerConfig, SchedulerExport};
use serde::Deserialize;
use testsi::{InputBuilder, Output, TestResult};

use crate::{TEST_CHAIN_ID, TEST_DAPP_ADDRESS, devnet_machine, inspect_report};

const CONSENSUS_VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/consensus_vectors_v2.json"
));

/// The pinned output that marks the signed transfer as included.
const SIGNED_TRANSFER_NOTICE: &str = "transfer_notice_alice_bob";

/// The scenarios the earlier template held, restated under strict frames.
const RESTATED: [&str; 4] = ["deposit", "transfer", "withdrawal", "spill_batch"];

/// Scenarios named with this prefix carry one signature variant each.
const SIGNATURE_PREFIX: &str = "signature_";

#[derive(Debug, Deserialize)]
struct ConsensusVectors {
    signature: PinnedSignature,
    guest: GuestVectors,
}

#[derive(Debug, Deserialize)]
struct PinnedSignature {
    address: Address,
    variants: Vec<PinnedVariant>,
}

#[derive(Debug, Deserialize)]
struct PinnedVariant {
    name: String,
    guest: Option<Address>,
}

#[derive(Debug, Deserialize)]
struct GuestVectors {
    chain_id: u64,
    app_contract: Address,
    config: PinnedConfig,
    senders: BTreeMap<String, Address>,
    first_input_envelope: Bytes,
    payloads: BTreeMap<String, Bytes>,
    outputs: BTreeMap<String, PinnedOutput>,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct PinnedConfig {
    sequencer_address: Address,
    max_wait_blocks: u64,
    fridge_bound: u64,
    max_direct_payload_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PinnedOutput {
    Notice(Bytes),
    Voucher {
        destination: Address,
        value: U256,
        payload: Bytes,
    },
}

#[derive(Debug, Deserialize)]
struct Scenario {
    name: String,
    steps: Vec<Step>,
    state: Option<Bytes>,
    state_keccak256: Option<B256>,
}

/// What the machine can show of a step. The handed-out transactions and the
/// reject reason are not observable here; the native and C checks hold them.
#[derive(Debug, Deserialize)]
struct Step {
    from: String,
    block: u64,
    timestamp: u64,
    payload: String,
    outcome: Outcome,
    outputs: Vec<String>,
    scheduler: PinnedExport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    DirectQueued,
    DirectDropped,
    BatchAccepted,
    BatchStale,
    BatchRejected,
}

#[derive(Debug, Deserialize)]
struct PinnedExport {
    l1_input_count: u64,
    l2_tx_count: u64,
    next_batch_nonce: u64,
    clock: u64,
    first_queued_input: u64,
    fridge_len: u64,
}

impl PinnedExport {
    /// The 48-byte `scheduler` report these fields encode to.
    fn report(&self) -> Vec<u8> {
        SchedulerExport {
            l1_input_count: self.l1_input_count,
            l2_tx_count: self.l2_tx_count,
            next_batch_nonce: self.next_batch_nonce,
            clock: self.clock,
            first_queued_input: self.first_queued_input,
            fridge_len: self.fridge_len,
        }
        .encode()
        .to_vec()
    }
}

impl GuestVectors {
    fn scenario(&self, name: &str) -> &Scenario {
        self.scenarios
            .iter()
            .find(|scenario| scenario.name == name)
            .unwrap_or_else(|| panic!("no golden scenario {name}"))
    }

    fn input(&self, step: &Step) -> InputBuilder {
        InputBuilder::from_address(self.senders[&step.from])
            .at_block(usize::try_from(step.block).expect("a pinned block fits usize"))
            .with_block_timestamp(
                usize::try_from(step.timestamp).expect("a pinned timestamp fits usize"),
            )
            .with_payload(&self.payloads[&step.payload])
    }

    /// Replays `scenario` on a fresh machine and compares every step.
    fn run(&self, scenario: &Scenario) -> TestResult {
        let name = &scenario.name;
        let mut machine = devnet_machine()?;
        for (index, step) in scenario.steps.iter().enumerate() {
            // testsi turns a rejected input into an error: the guest accepts all.
            let (outputs, reports) = machine.advance_state(self.input(step))?;
            let actual: Vec<PinnedOutput> = outputs.list().iter().map(pinned_form).collect();
            let expected: Vec<PinnedOutput> = step
                .outputs
                .iter()
                .map(|output| self.outputs[output].clone())
                .collect();
            assert_eq!(actual, expected, "{name}, step {index} ({})", step.payload);

            let expected_reports: &[&[u8]] = if step.outcome == Outcome::BatchRejected {
                &[INVALID_BATCH_REPORT]
            } else {
                &[]
            };
            assert_eq!(reports, expected_reports, "{name}, step {index} reports");

            assert_eq!(
                inspect_report(&mut machine, SCHEDULER_INSPECT_QUERY)?,
                step.scheduler.report(),
                "{name}, step {index}: scheduler report"
            );
        }

        let state = inspect_report(&mut machine, STATE_INSPECT_QUERY)?;
        match (&scenario.state, &scenario.state_keccak256) {
            (Some(pinned), None) => assert_eq!(state, pinned.as_ref(), "{name}: state report"),
            (None, Some(pinned)) => assert_eq!(keccak256(&state), *pinned, "{name}: state report"),
            _ => panic!("{name} must pin its state one way"),
        }
        Ok(())
    }
}

fn pinned_form(output: &Output) -> PinnedOutput {
    match output {
        Output::Notice(notice) => PinnedOutput::Notice(notice.payload.clone()),
        Output::Voucher(voucher) => PinnedOutput::Voucher {
            destination: voucher.destination,
            value: voucher.value,
            payload: voucher.payload.clone(),
        },
    }
}

fn consensus_vectors() -> ConsensusVectors {
    serde_json::from_str(CONSENSUS_VECTORS).expect("parse consensus_vectors_v2.json")
}

/// The image runs the devnet template on the devnet chain and app, so the
/// pinned deployment must be that one.
#[testsi::test_dapp(kind("golden"))]
pub fn golden_vectors_pin_the_devnet_template() -> TestResult {
    let guest = consensus_vectors().guest;
    assert_eq!(
        (guest.chain_id, guest.app_contract),
        (TEST_CHAIN_ID, TEST_DAPP_ADDRESS),
        "the golden inputs were pinned against the devnet machine's chain and app"
    );
    let pinned = &guest.config;
    assert_eq!(
        SchedulerConfig {
            sequencer_address: pinned.sequencer_address,
            max_wait_blocks: pinned.max_wait_blocks,
            fridge_bound: pinned.fridge_bound,
            max_direct_payload_bytes: pinned.max_direct_payload_bytes,
        },
        SchedulerConfig::for_application::<WalletApp>(DEVNET_SEQUENCER_ADDRESS)
    );
    assert_eq!(guest.senders["sequencer"], DEVNET_SEQUENCER_ADDRESS);

    // testsi wraps each payload in an EvmAdvance at test time; pinning the
    // first envelope tells a host encoder drift apart from a guest change.
    let first = &guest.scenarios[0].steps[0];
    assert_eq!(
        guest
            .input(first)
            .encode(TEST_CHAIN_ID as usize, U256::ZERO, TEST_DAPP_ADDRESS),
        guest.first_input_envelope.as_ref()
    );
    Ok(())
}

#[testsi::test_dapp(kind("golden"))]
pub fn golden_restated_scenarios_emit_the_pinned_outputs() -> TestResult {
    let guest = consensus_vectors().guest;
    for name in RESTATED {
        guest.run(guest.scenario(name))?;
    }
    Ok(())
}

/// Each signature variant rides in its own batch after a deposit, followed by
/// the signer's correctly signed op at the same nonce: a variant that runs as
/// the signer emits the transfer notice and leaves the follow-up skipped; any
/// other leaves the signer's nonce for the follow-up. Which sender a variant
/// recovers is pinned natively; here the image is held to the pinned outputs.
#[testsi::test_dapp(kind("golden"))]
pub fn golden_signature_variants_match_the_pinned_guest_outcome() -> TestResult {
    let vectors = consensus_vectors();
    for variant in &vectors.signature.variants {
        let name = format!("{SIGNATURE_PREFIX}{}", variant.name);
        let scenario = vectors.guest.scenario(&name);
        // The pinned outputs agree with the pinned recovery on whether the op
        // runs as the signer.
        let included = scenario.steps[1]
            .outputs
            .iter()
            .any(|output| output == SIGNED_TRANSFER_NOTICE);
        assert_eq!(
            included,
            variant.guest == Some(vectors.signature.address),
            "{name}"
        );
    }
    for scenario in &vectors.guest.scenarios {
        if scenario.name.starts_with(SIGNATURE_PREFIX) {
            vectors.guest.run(scenario)?;
        }
    }
    Ok(())
}

/// Every other scenario: the gates, the clock, the backstop, the payload cap
/// and the skipped, executed and failed user op. The nonce-exhausted skip has
/// no scenario; the executor's unit tests hold it.
#[testsi::test_dapp(kind("golden"))]
pub fn golden_scheduler_rules_match_the_pinned_steps() -> TestResult {
    let guest = consensus_vectors().guest;
    for scenario in &guest.scenarios {
        if !RESTATED.contains(&scenario.name.as_str())
            && !scenario.name.starts_with(SIGNATURE_PREFIX)
        {
            guest.run(scenario)?;
        }
    }
    Ok(())
}
