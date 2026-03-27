// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use crate::{ScenarioFn, ScenarioResult};
use alloy_primitives::{Address, U256};
use rollups_harness::{ManagedSequencer, ReplayWalletApp, TestSigner, WalletL1Client, WsClient};
use sequencer_core::api::WsTxMessage;
use sequencer_core::fee::fee_to_linear;

const NO_WS_MESSAGE_WAIT: Duration = Duration::from_secs(1);

/// Default log_recommended_fee exponent (0 + 20 + 419 + 621 = 1060).
const DEFAULT_FRAME_FEE: u16 = 1060;

struct ExpectedWalletState {
    address: Address,
    balance: U256,
    nonce: u32,
}

pub fn test_cases() -> Vec<(&'static str, ScenarioFn)> {
    vec![
        ("deposit_transfer_withdrawal_test", |runtime| {
            Box::pin(run_deposit_transfer_withdrawal_test(runtime))
        }),
        ("direct_input_not_safe_yet_test", |runtime| {
            Box::pin(run_direct_input_not_safe_yet_test(runtime))
        }),
        ("rejected_user_op_not_broadcast_test", |runtime| {
            Box::pin(run_rejected_user_op_not_broadcast_test(runtime))
        }),
        ("reconnect_from_offset_test", |runtime| {
            Box::pin(run_reconnect_from_offset_test(runtime))
        }),
        ("restart_and_replay_test", |runtime| {
            Box::pin(run_restart_and_replay_test(runtime))
        }),
        ("unsupported_token_deposit_noop_test", |runtime| {
            Box::pin(run_unsupported_token_deposit_noop_test(runtime))
        }),
    ]
}

async fn run_deposit_transfer_withdrawal_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut bob_l2 = runtime.wallet_l2(bob)?;
    let mut replay = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(400_000_u64);
    let withdrawal_amount = U256::from(150_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    apply_safe_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount).await?;

    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    bob_l2.withdraw(withdrawal_amount).await?;
    replay.apply(ws.expect_user_op_from(bob_address).await?)?;

    // Alice: 600_000 - 400_000 - gas. Bob: 400_000 - 150_000 - gas.
    assert_wallet_state(
        &replay,
        ExpectedWalletState {
            address: alice_address,
            balance: deposit_amount - transfer_amount - gas,
            nonce: 1,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: transfer_amount - withdrawal_amount - gas,
            nonce: 1,
        },
        3,
    );
    Ok(())
}

async fn run_direct_input_not_safe_yet_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay = ReplayWalletApp::devnet();

    let initial_funding_amount = U256::from(700_000_u64);
    let pending_deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(400_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        initial_funding_amount,
    )
    .await?;

    alice_l1
        .mint_supported_token(pending_deposit_amount)
        .await?;
    alice_l1
        .deposit_supported_token(pending_deposit_amount)
        .await?;

    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    runtime.mine_l1_blocks(1).await?;
    replay.apply(
        ws.expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;

    // Alice: 700_000 - 400_000 - gas + 600_000. Bob: 400_000 (no ops from Bob).
    assert_wallet_state(
        &replay,
        ExpectedWalletState {
            address: alice_address,
            balance: initial_funding_amount - transfer_amount - gas + pending_deposit_amount,
            nonce: 1,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: transfer_amount,
            nonce: 0,
        },
        3,
    );
    Ok(())
}

async fn run_rejected_user_op_not_broadcast_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut stale_alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(100_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    apply_safe_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount).await?;

    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    let rejected = stale_alice_l2
        .transfer(bob_address, U256::from(50_000_u64))
        .await
        .expect_err("stale nonce transfer should be rejected");
    let rejected_text = rejected.to_string();
    assert!(
        rejected_text.contains("bad nonce") || rejected_text.contains("422"),
        "expected stale nonce rejection, got: {rejected_text}"
    );
    ws.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    // Alice: 600_000 - 100_000 - gas. Bob: 100_000 (rejected op not charged).
    assert_wallet_state(
        &replay,
        ExpectedWalletState {
            address: alice_address,
            balance: deposit_amount - transfer_amount - gas,
            nonce: 1,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: transfer_amount,
            nonce: 0,
        },
        2,
    );
    Ok(())
}

async fn run_reconnect_from_offset_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut bob_l2 = runtime.wallet_l2(bob)?;
    let mut replay = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(250_000_u64);
    let withdrawal_amount = U256::from(100_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    let deposit_message =
        apply_safe_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
            .await?;
    let reconnect_offset = deposit_message.offset().saturating_add(1);
    drop(ws);

    alice_l2.transfer(bob_address, transfer_amount).await?;
    bob_l2.withdraw(withdrawal_amount).await?;

    let mut resumed_ws = runtime.ws(reconnect_offset).await?;
    replay.apply(resumed_ws.expect_user_op_from(alice_address).await?)?;
    replay.apply(resumed_ws.expect_user_op_from(bob_address).await?)?;

    // Alice: 600_000 - 250_000 - gas. Bob: 250_000 - 100_000 - gas.
    assert_wallet_state(
        &replay,
        ExpectedWalletState {
            address: alice_address,
            balance: deposit_amount - transfer_amount - gas,
            nonce: 1,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: transfer_amount - withdrawal_amount - gas,
            nonce: 1,
        },
        3,
    );
    Ok(())
}

async fn run_restart_and_replay_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut bob_l2 = runtime.wallet_l2(bob.clone())?;
    let mut replay_before_restart = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(400_000_u64);
    let withdrawal_amount = U256::from(150_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before_restart,
        &alice_l1,
        deposit_amount,
    )
    .await?;
    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay_before_restart.apply(ws.expect_user_op_from(alice_address).await?)?;
    bob_l2.withdraw(withdrawal_amount).await?;
    replay_before_restart.apply(ws.expect_user_op_from(bob_address).await?)?;

    drop(ws);
    runtime.restart().await?;

    let mut ws_after_restart = runtime.ws(0).await?;
    let mut replay_after_restart = ReplayWalletApp::devnet();
    replay_after_restart.apply(
        ws_after_restart
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    replay_after_restart.apply(ws_after_restart.expect_user_op_from(alice_address).await?)?;
    replay_after_restart.apply(ws_after_restart.expect_user_op_from(bob_address).await?)?;
    ws_after_restart
        .expect_no_message_for(NO_WS_MESSAGE_WAIT)
        .await?;

    // Alice: 600_000 - 400_000 - gas. Bob: 400_000 - 150_000 - gas.
    let expected_alice = deposit_amount - transfer_amount - gas;
    let expected_bob = transfer_amount - withdrawal_amount - gas;
    assert_wallet_state(
        &replay_before_restart,
        ExpectedWalletState {
            address: alice_address,
            balance: expected_alice,
            nonce: 1,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: expected_bob,
            nonce: 1,
        },
        3,
    );
    assert_wallet_state(
        &replay_after_restart,
        ExpectedWalletState {
            address: alice_address,
            balance: expected_alice,
            nonce: 1,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: expected_bob,
            nonce: 1,
        },
        3,
    );
    Ok(())
}

async fn run_unsupported_token_deposit_noop_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();

    let unsupported_token = runtime.deploy_extra_mock_erc20().await?;
    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice).await?;
    let mut replay = ReplayWalletApp::devnet();

    alice_l1
        .mint_token(unsupported_token, U256::from(123_000_u64))
        .await?;
    alice_l1
        .deposit_token(unsupported_token, U256::from(123_000_u64))
        .await?;
    runtime.mine_l1_blocks(1).await?;

    replay.apply(
        ws.expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;

    assert_eq!(replay.current_user_balance(alice_address), U256::ZERO);
    assert_eq!(replay.current_user_nonce(alice_address), 0);
    assert_eq!(replay.executed_input_count(), 1);
    Ok(())
}

async fn apply_safe_supported_deposit(
    runtime: &ManagedSequencer,
    ws: &mut WsClient,
    replay: &mut ReplayWalletApp,
    wallet_l1: &WalletL1Client,
    amount: U256,
) -> ScenarioResult<WsTxMessage> {
    wallet_l1.mint_supported_token(amount).await?;
    wallet_l1.deposit_supported_token(amount).await?;
    runtime.mine_l1_blocks(1).await?;

    let message = ws
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    replay.apply(message.clone())?;
    Ok(message)
}

fn assert_wallet_state(
    replay: &ReplayWalletApp,
    first: ExpectedWalletState,
    second: ExpectedWalletState,
    executed_input_count: u64,
) {
    assert_eq!(replay.current_user_balance(first.address), first.balance);
    assert_eq!(replay.current_user_nonce(first.address), first.nonce);
    assert_eq!(replay.current_user_balance(second.address), second.balance);
    assert_eq!(replay.current_user_nonce(second.address), second.nonce);
    assert_eq!(replay.executed_input_count(), executed_input_count);
}
