// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::PathBuf;

use app_core::application::{
    DepositNotice, Method, Transfer, TransferNotice, WalletConfig, Withdrawal,
};
use canonical_app::{DEVNET_SEQUENCER_ADDRESS, SchedulerConfig};
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::hazmat::PrehashSigner;
use sequencer_core::batch::{Batch, Frame, WireUserOp};
use sequencer_core::user_op::UserOp;
use testsi::{InputBuilder, Machine, MachineBuilder, OutputsForInput, TestResult};
use types::Erc20Transfer;
use types::alloy_primitives::Signature;
use types::alloy_primitives::{Address, U256, address};
use types::alloy_sol_types::{Eip712Domain, SolCall, SolStruct};

testsi::testsi_main!();

const INVALID_BATCH_REPORT: &[u8] = b"scheduler dropped invalid batch";
const TEST_CHAIN_ID: u64 = 31337;
const TEST_DAPP_ADDRESS: Address = Address::ZERO;

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_reports_invalid_batch_from_guest() -> TestResult {
    let mut machine = devnet_machine()?;

    let (outputs, reports) = machine.advance_state(sequencer_input(10, &[0xff, 0xee, 0xdd]))?;

    assert_invalid_batch_step(&outputs, &reports);
    Ok(())
}

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_rejected_batch_does_not_consume_nonce() -> TestResult {
    let mut machine = devnet_machine()?;

    let (outputs, reports) = machine.advance_state(sequencer_input(10, &[0xff, 0xee, 0xdd]))?;
    assert_invalid_batch_step(&outputs, &reports);

    let (outputs, reports) = machine.advance_state(batch_input(11, empty_batch(0)))?;
    assert_no_outputs_or_reports(&outputs, &reports);

    let (outputs, reports) = machine.advance_state(batch_input(12, empty_batch(0)))?;
    assert_invalid_batch_step(&outputs, &reports);
    Ok(())
}

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_stale_batch_consumes_nonce_without_report() -> TestResult {
    let mut machine = devnet_machine()?;
    let stale_trigger_block = SchedulerConfig::devnet().max_wait_blocks as usize + 1;

    let (outputs, reports) = machine.advance_state(batch_input(
        stale_trigger_block,
        batch_with_safe_blocks(0, &[1]),
    ))?;
    assert_no_outputs_or_reports(&outputs, &reports);

    let (outputs, reports) =
        machine.advance_state(batch_input(stale_trigger_block + 1, empty_batch(0)))?;
    assert_invalid_batch_step(&outputs, &reports);

    let (outputs, reports) =
        machine.advance_state(batch_input(stale_trigger_block + 2, empty_batch(1)))?;
    assert_no_outputs_or_reports(&outputs, &reports);
    Ok(())
}

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_reports_wrong_nonce_batch_from_guest() -> TestResult {
    let mut machine = devnet_machine()?;

    let (outputs, reports) = machine.advance_state(batch_input(10, empty_batch(1)))?;

    assert_invalid_batch_step(&outputs, &reports);
    Ok(())
}

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_reports_non_monotonic_safe_blocks_from_guest() -> TestResult {
    let mut machine = devnet_machine()?;

    let (outputs, reports) =
        machine.advance_state(batch_input(10, batch_with_safe_blocks(0, &[8, 7])))?;

    assert_invalid_batch_step(&outputs, &reports);
    Ok(())
}

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_reports_safe_block_above_inclusion_block_from_guest() -> TestResult {
    let mut machine = devnet_machine()?;

    let (outputs, reports) =
        machine.advance_state(batch_input(10, batch_with_safe_blocks(0, &[11])))?;

    assert_invalid_batch_step(&outputs, &reports);
    Ok(())
}

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_emits_deposit_notice_from_guest() -> TestResult {
    let mut machine = devnet_machine()?;
    let wallet_config = WalletConfig::devnet();
    let depositor = address!("0x7777777777777777777777777777777777777777");
    let amount = U256::from(250_u64);

    let (outputs, reports) = machine.advance_state(portal_input(
        10,
        wallet_config.supported_erc20_token,
        depositor,
        amount,
    ))?;
    assert_no_outputs_or_reports(&outputs, &reports);

    let (outputs, reports) =
        machine.advance_state(batch_input(10, batch_with_safe_blocks(0, &[10])))?;
    assert_deposit_notice(
        &outputs,
        &reports,
        wallet_config.supported_erc20_token,
        depositor,
        amount,
    );
    Ok(())
}

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_emits_transfer_notice_from_guest() -> TestResult {
    let mut machine = devnet_machine()?;
    let wallet_config = WalletConfig::devnet();
    let alice_key = signing_key(1);
    let alice = address_from_signing_key(&alice_key);
    let bob = address!("0x8888888888888888888888888888888888888888");
    let deposit_amount = U256::from(250_u64);
    let transfer_amount = U256::from(125_u64);

    let (outputs, reports) = machine.advance_state(portal_input(
        10,
        wallet_config.supported_erc20_token,
        alice,
        deposit_amount,
    ))?;
    assert_no_outputs_or_reports(&outputs, &reports);

    let (outputs, reports) =
        machine.advance_state(batch_input(10, batch_with_safe_blocks(0, &[10])))?;
    assert_deposit_notice(
        &outputs,
        &reports,
        wallet_config.supported_erc20_token,
        alice,
        deposit_amount,
    );

    let transfer = signed_user_op(
        &alice_key,
        0,
        ssz::Encode::as_ssz_bytes(&Method::Transfer(Transfer {
            amount: transfer_amount,
            to: bob,
        })),
    );
    let (outputs, reports) =
        machine.advance_state(batch_input(11, batch_with_frame(1, 10, vec![transfer])))?;
    assert_transfer_notice(&outputs, &reports, alice, bob, transfer_amount);
    Ok(())
}

#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_emits_withdrawal_voucher_from_guest() -> TestResult {
    let mut machine = devnet_machine()?;
    let wallet_config = WalletConfig::devnet();
    let alice_key = signing_key(2);
    let alice = address_from_signing_key(&alice_key);
    let withdrawal_amount = U256::from(125_u64);
    let deposit_amount = U256::from(250_u64);

    let (outputs, reports) = machine.advance_state(portal_input(
        10,
        wallet_config.supported_erc20_token,
        alice,
        deposit_amount,
    ))?;
    assert_no_outputs_or_reports(&outputs, &reports);

    let (outputs, reports) =
        machine.advance_state(batch_input(10, batch_with_safe_blocks(0, &[10])))?;
    assert_deposit_notice(
        &outputs,
        &reports,
        wallet_config.supported_erc20_token,
        alice,
        deposit_amount,
    );

    let withdrawal = signed_user_op(
        &alice_key,
        0,
        ssz::Encode::as_ssz_bytes(&Method::Withdrawal(Withdrawal {
            amount: withdrawal_amount,
        })),
    );
    let (outputs, reports) =
        machine.advance_state(batch_input(11, batch_with_frame(1, 10, vec![withdrawal])))?;
    assert_withdrawal_voucher(
        &outputs,
        &reports,
        wallet_config.supported_erc20_token,
        alice,
        withdrawal_amount,
    );
    Ok(())
}

fn devnet_machine() -> Result<Machine, Box<dyn std::error::Error + Send + Sync>> {
    let machine_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../canonical-app/out/canonical-machine-image");
    let machine = MachineBuilder::load_from(machine_path)
        .at_chain(TEST_CHAIN_ID as usize)
        .deployed_at(TEST_DAPP_ADDRESS)
        .no_console_putchar(false)
        .try_build()?;
    Ok(machine)
}

fn input_domain() -> Eip712Domain {
    Eip712Domain {
        name: None,
        version: None,
        chain_id: Some(U256::from(TEST_CHAIN_ID)),
        verifying_contract: Some(TEST_DAPP_ADDRESS),
        salt: None,
    }
}

fn signing_key(byte: u8) -> SigningKey {
    let mut bytes = [0_u8; 32];
    bytes.fill(byte);
    SigningKey::from_slice(&bytes).expect("build signing key")
}

fn address_from_signing_key(signing_key: &SigningKey) -> Address {
    let verifying = signing_key.verifying_key().to_encoded_point(false);
    Address::from_raw_public_key(&verifying.as_bytes()[1..])
}

fn signed_user_op(signing_key: &SigningKey, nonce: u32, data: Vec<u8>) -> WireUserOp {
    let user_op = UserOp {
        nonce,
        max_fee: 0,
        data: data.clone().into(),
    };
    let signing_hash = user_op.eip712_signing_hash(&input_domain());
    let k256_sig = signing_key
        .sign_prehash(signing_hash.as_slice())
        .expect("sign user op hash");
    let sender = address_from_signing_key(signing_key);
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|candidate| {
            candidate
                .recover_address_from_prehash(&signing_hash)
                .ok()
                .map(|value| value == sender)
                .unwrap_or(false)
        })
        .expect("recoverable parity for signature");

    WireUserOp {
        nonce,
        max_fee: 0,
        data,
        signature: signature.as_bytes().to_vec(),
    }
}

fn sequencer_input(block: usize, payload: &[u8]) -> InputBuilder {
    InputBuilder::from_address(DEVNET_SEQUENCER_ADDRESS)
        .at_block(block)
        .with_payload(&payload)
}

fn portal_input(block: usize, token: Address, sender: Address, value: U256) -> InputBuilder {
    let wallet_config = WalletConfig::devnet();
    InputBuilder::from_address(wallet_config.erc20_portal_address)
        .at_block(block)
        .with_payload(&encode_erc20_deposit_payload(token, sender, value))
}

fn batch_input(block: usize, batch: Batch) -> InputBuilder {
    let payload = ssz::Encode::as_ssz_bytes(&batch);
    sequencer_input(block, &payload)
}

fn empty_batch(nonce: u64) -> Batch {
    Batch {
        nonce,
        frames: Vec::new(),
    }
}

fn batch_with_safe_blocks(nonce: u64, safe_blocks: &[u64]) -> Batch {
    Batch {
        nonce,
        frames: safe_blocks
            .iter()
            .copied()
            .map(|safe_block| Frame {
                user_ops: Vec::new(),
                safe_block,
                fee_price: 0,
            })
            .collect(),
    }
}

fn batch_with_frame(nonce: u64, safe_block: u64, user_ops: Vec<WireUserOp>) -> Batch {
    Batch {
        nonce,
        frames: vec![Frame {
            user_ops,
            safe_block,
            fee_price: 0,
        }],
    }
}

fn encode_erc20_deposit_payload(token: Address, sender: Address, value: U256) -> Vec<u8> {
    let mut payload = Vec::with_capacity(types::ERC20_DEPOSIT_PREFIX_BYTES);
    payload.extend_from_slice(token.as_slice());
    payload.extend_from_slice(sender.as_slice());
    payload.extend_from_slice(value.to_be_bytes::<32>().as_slice());
    payload
}

fn assert_invalid_batch_step(outputs: &OutputsForInput, reports: &[Vec<u8>]) {
    assert!(
        outputs.list().is_empty(),
        "expected rejected batch to emit no outputs, got {:?}",
        outputs.list()
    );
    assert!(
        reports
            .iter()
            .any(|report| report.as_slice() == INVALID_BATCH_REPORT),
        "expected invalid batch report, got {reports:?}"
    );
}

fn assert_no_outputs_or_reports(outputs: &OutputsForInput, reports: &[Vec<u8>]) {
    assert!(
        outputs.list().is_empty(),
        "expected no outputs for step, got {:?}",
        outputs.list()
    );
    assert!(
        reports.is_empty(),
        "expected no reports for step, got {reports:?}"
    );
}

fn assert_deposit_notice(
    outputs: &OutputsForInput,
    reports: &[Vec<u8>],
    token: Address,
    sender: Address,
    amount: U256,
) {
    assert!(reports.is_empty(), "expected no reports, got {reports:?}");
    assert_eq!(outputs.list().len(), 1, "expected exactly one output");
    let notice = outputs[0].expect_notice();
    let decoded = DepositNotice::abi_decode(&notice.payload).expect("decode deposit notice");
    assert_eq!(decoded.token, token);
    assert_eq!(decoded.sender, sender);
    assert_eq!(decoded.amount, amount);
}

fn assert_transfer_notice(
    outputs: &OutputsForInput,
    reports: &[Vec<u8>],
    sender: Address,
    recipient: Address,
    amount: U256,
) {
    assert!(reports.is_empty(), "expected no reports, got {reports:?}");
    assert_eq!(outputs.list().len(), 1, "expected exactly one output");
    let notice = outputs[0].expect_notice();
    let decoded = TransferNotice::abi_decode(&notice.payload).expect("decode transfer notice");
    assert_eq!(decoded.sender, sender);
    assert_eq!(decoded.recipient, recipient);
    assert_eq!(decoded.amount, amount);
}

fn assert_withdrawal_voucher(
    outputs: &OutputsForInput,
    reports: &[Vec<u8>],
    token: Address,
    recipient: Address,
    amount: U256,
) {
    assert!(reports.is_empty(), "expected no reports, got {reports:?}");
    assert_eq!(outputs.list().len(), 1, "expected exactly one output");
    let voucher = outputs[0].expect_voucher();
    assert_eq!(voucher.destination, token);
    assert_eq!(voucher.value, U256::ZERO);
    let decoded = Erc20Transfer::abi_decode(&voucher.payload).expect("decode withdrawal voucher");
    assert_eq!(decoded.recipient, recipient);
    assert_eq!(decoded.amount, amount);
}
