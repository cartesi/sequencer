// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::PathBuf;

use app_core::application::{
    DEVNET_SEQUENCER_ADDRESS, DepositNotice, Method, Transfer, TransferNotice, WalletConfig,
    Withdrawal,
};
use canonical_app::SchedulerConfig;
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
pub fn scheduler_stale_batch_is_skipped_without_consuming_nonce() -> TestResult {
    let mut machine = devnet_machine()?;
    let stale_trigger_block =
        SchedulerConfig::new(DEVNET_SEQUENCER_ADDRESS).max_wait_blocks as usize + 1;

    // Stale batch (nonce 0, safe_block 1, inclusion block > max_wait_blocks) → skipped silently.
    let (outputs, reports) = machine.advance_state(batch_input(
        stale_trigger_block,
        batch_with_safe_blocks(0, &[1]),
    ))?;
    assert_no_outputs_or_reports(&outputs, &reports);

    // Fresh batch with nonce 0 succeeds — stale batch did NOT consume the nonce.
    let (outputs, reports) =
        machine.advance_state(batch_input(stale_trigger_block + 1, empty_batch(0)))?;
    assert_no_outputs_or_reports(&outputs, &reports);

    // Next batch with nonce 1 also succeeds.
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

/// T3: the guest's fee arithmetic must agree with the host's `fee_to_linear`.
/// Every other scheduler test runs frames at `fee_price: 0`, so the guest's gas
/// charging and max-fee skip never ran with a nonzero fee — a guest-side
/// divergence would be silent. This drives one nonzero-fee frame and pins the
/// charged gas to the host's value from BOTH sides, plus the below-fee skip:
///   - Alice transfers exactly `deposit - gas`: affordable iff guest gas <= host.
///   - Carol transfers `deposit - gas + 1`: affordable iff guest gas <  host.
///   - Dave's op has `max_fee < fee_price`: must be skipped (below the frame fee).
/// A correct guest emits exactly one transfer notice (Alice's) — so guest gas ==
/// host `fee_to_linear(fee_price)` bit-for-bit, and the max-fee skip holds.
#[testsi::test_dapp(kind("scheduler"))]
pub fn scheduler_fee_arithmetic_matches_host_from_guest() -> TestResult {
    let mut machine = devnet_machine()?;
    let token = WalletConfig::devnet().supported_erc20_token;
    let bob = address!("0x8888888888888888888888888888888888888888");
    let fee_price: u16 = 200;
    let gas = sequencer_core::fee::fee_to_linear(fee_price);
    let deposit = U256::from(10_000_u64);

    let alice_key = signing_key(11);
    let alice = address_from_signing_key(&alice_key);
    let carol_key = signing_key(12);
    let carol = address_from_signing_key(&carol_key);
    let dave_key = signing_key(13);
    let dave = address_from_signing_key(&dave_key);

    // Fund all three; one drain batch executes the three pending deposits.
    for who in [alice, carol, dave] {
        let (outputs, reports) = machine.advance_state(portal_input(10, token, who, deposit))?;
        assert_no_outputs_or_reports(&outputs, &reports);
    }
    let (outputs, reports) =
        machine.advance_state(batch_input(10, batch_with_safe_blocks(0, &[10])))?;
    assert!(reports.is_empty(), "drain reports: {reports:?}");
    assert_eq!(
        outputs.list().len(),
        3,
        "expected three deposit notices, got {:?}",
        outputs.list()
    );

    let exact = deposit - gas;
    let transfer =
        |amount| ssz::Encode::as_ssz_bytes(&Method::Transfer(Transfer { amount, to: bob }));
    let alice_exact = signed_user_op_with_fee(&alice_key, 0, fee_price, transfer(exact));
    let carol_over = signed_user_op_with_fee(
        &carol_key,
        0,
        fee_price,
        transfer(exact + U256::from(1_u64)),
    );
    let dave_below_fee =
        signed_user_op_with_fee(&dave_key, 0, fee_price - 1, transfer(U256::from(1_u64)));

    let (outputs, reports) = machine.advance_state(batch_input(
        11,
        batch_with_frame_fee(
            1,
            10,
            fee_price,
            vec![alice_exact, carol_over, dave_below_fee],
        ),
    ))?;
    assert!(reports.is_empty(), "fee-frame reports: {reports:?}");
    assert_eq!(
        outputs.list().len(),
        1,
        "expected exactly one transfer notice (Alice's exact transfer): Carol's \
         over-by-one must be unaffordable (guest charged the full host gas) and \
         Dave's below-fee op must be skipped, got {:?}",
        outputs.list()
    );
    let notice = outputs[0].expect_notice();
    let decoded = TransferNotice::abi_decode(&notice.payload).expect("decode transfer notice");
    assert_eq!(decoded.sender, alice);
    assert_eq!(decoded.recipient, bob);
    assert_eq!(decoded.amount, exact);
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
    sequencer_core::build_input_domain(TEST_CHAIN_ID, TEST_DAPP_ADDRESS)
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
    signed_user_op_with_fee(signing_key, nonce, 0, data)
}

fn signed_user_op_with_fee(
    signing_key: &SigningKey,
    nonce: u32,
    max_fee: u16,
    data: Vec<u8>,
) -> WireUserOp {
    let user_op = UserOp {
        nonce,
        max_fee,
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
        max_fee,
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
    batch_with_frame_fee(nonce, safe_block, 0, user_ops)
}

fn batch_with_frame_fee(
    nonce: u64,
    safe_block: u64,
    fee_price: u16,
    user_ops: Vec<WireUserOp>,
) -> Batch {
    Batch {
        nonce,
        frames: vec![Frame {
            user_ops,
            safe_block,
            fee_price,
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
