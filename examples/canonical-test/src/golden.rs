// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Golden guest cases from `tests/fixtures/consensus_vectors_v1.json`: pinned
//! input payloads go into the built image unchanged (nothing is signed or
//! encoded here) and its raw outputs must equal the pinned bytes. The vectors
//! are never regenerated; a mismatch is a consensus change and a finding.
//! Reports are diagnostics, not consensus, and are not compared.

use std::collections::BTreeMap;

use rollups_types::alloy_primitives::{Address, Bytes, U256};
use serde::Deserialize;
use testsi::{InputBuilder, Output, TestResult};

use crate::{TEST_CHAIN_ID, TEST_DAPP_ADDRESS, devnet_machine};

const CONSENSUS_VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/consensus_vectors_v1.json"
));

/// The pinned output that marks the signed transfer as included.
const SIGNED_TRANSFER_NOTICE: &str = "transfer_notice_alice_bob";

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
    senders: BTreeMap<String, Address>,
    first_input_envelope: Bytes,
    payloads: BTreeMap<String, Bytes>,
    outputs: BTreeMap<String, PinnedOutput>,
    scenarios: Vec<Scenario>,
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
}

#[derive(Debug, Deserialize)]
struct Step {
    from: String,
    block: usize,
    payload: String,
    outputs: Vec<String>,
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
            .at_block(step.block)
            .with_payload(&self.payloads[&step.payload])
    }

    /// Replays `name` on a fresh machine and compares every step's outputs.
    fn run(&self, name: &str) -> TestResult {
        assert_eq!(
            (self.chain_id, self.app_contract),
            (TEST_CHAIN_ID, TEST_DAPP_ADDRESS),
            "the golden inputs were pinned against the devnet machine's chain and app"
        );
        let mut machine = devnet_machine()?;
        for (index, step) in self.scenario(name).steps.iter().enumerate() {
            let (outputs, _reports) = machine.advance_state(self.input(step))?;
            let actual: Vec<PinnedOutput> = outputs.list().iter().map(pinned_form).collect();
            let expected: Vec<PinnedOutput> = step
                .outputs
                .iter()
                .map(|output| self.outputs[output].clone())
                .collect();
            assert_eq!(actual, expected, "{name}, step {index} ({})", step.payload);
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
    serde_json::from_str(CONSENSUS_VECTORS).expect("parse consensus_vectors_v1.json")
}

#[testsi::test_dapp(kind("golden"))]
pub fn golden_deposit_emits_the_pinned_notice() -> TestResult {
    let guest = consensus_vectors().guest;
    // testsi wraps each payload in an EvmAdvance at test time; pinning the
    // first envelope tells a host encoder drift apart from a guest change.
    let first = &guest.scenario("deposit").steps[0];
    assert_eq!(
        guest
            .input(first)
            .encode(TEST_CHAIN_ID as usize, U256::ZERO, TEST_DAPP_ADDRESS),
        guest.first_input_envelope.as_ref()
    );
    guest.run("deposit")
}

#[testsi::test_dapp(kind("golden"))]
pub fn golden_signed_transfer_emits_the_pinned_notice() -> TestResult {
    consensus_vectors().guest.run("transfer")
}

#[testsi::test_dapp(kind("golden"))]
pub fn golden_withdrawal_emits_the_pinned_voucher() -> TestResult {
    consensus_vectors().guest.run("withdrawal")
}

#[testsi::test_dapp(kind("golden"))]
pub fn golden_spill_batch_emits_the_pinned_outputs() -> TestResult {
    consensus_vectors().guest.run("spill_batch")
}

/// Each signature variant rides in its own batch after a deposit, followed by
/// the signer's correctly signed op at the same nonce: a variant that runs as
/// the signer emits the transfer notice and leaves the follow-up with nothing;
/// any other emits nothing and the follow-up shows the signer's nonce was not
/// consumed. The image is thus checked for running each variant as the signer
/// or not; which other sender a variant recovers is pinned natively, in
/// sequencer-core.
#[testsi::test_dapp(kind("golden"))]
pub fn golden_signature_variants_match_the_pinned_guest_outcome() -> TestResult {
    let vectors = consensus_vectors();
    for variant in &vectors.signature.variants {
        let name = format!("signature_{}", variant.name);
        vectors.guest.run(&name)?;

        // The pinned outputs agree with the pinned recovery on whether the op
        // runs as the signer.
        let included = vectors.guest.scenario(&name).steps[1]
            .outputs
            .iter()
            .any(|output| output == SIGNED_TRANSFER_NOTICE);
        assert_eq!(
            included,
            variant.guest == Some(vectors.signature.address),
            "{name}"
        );
    }
    Ok(())
}
