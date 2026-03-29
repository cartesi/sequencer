// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use crate::{ScenarioFn, ScenarioResult};
use alloy_primitives::{Address, U256};
use rollups_harness::{
    ManagedSequencer, ReplayWalletApp, TestSigner, WalletL1Client, WsClient, sign_user_op_hex,
};
use sequencer_core::api::{TxRequest, WsTxMessage};
use sequencer_core::fee::fee_to_linear;
use sequencer_core::user_op::UserOp;
use sequencer_rust_client::SequencerClient;

const NO_WS_MESSAGE_WAIT: Duration = Duration::from_secs(1);

/// Default log_recommended_fee exponent (0 + 20 + 419 + 621 = 1060).
const DEFAULT_FRAME_FEE: u16 = 1060;

/// Max fee used for raw TxRequest construction. Must be >= DEFAULT_FRAME_FEE.
const DEFAULT_MAX_FEE: u16 = 1200;

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
        ("fee_below_minimum_rejected_test", |runtime| {
            Box::pin(run_fee_below_minimum_rejected_test(runtime))
        }),
        ("forged_signature_rejected_test", |runtime| {
            Box::pin(run_forged_signature_rejected_test(runtime))
        }),
        ("concurrent_user_ops_test", |runtime| {
            Box::pin(run_concurrent_user_ops_test(runtime))
        }),
        ("multi_deposit_same_block_test", |runtime| {
            Box::pin(run_multi_deposit_same_block_test(runtime))
        }),
        ("shutdown_during_inflight_test", |runtime| {
            Box::pin(run_shutdown_during_inflight_test(runtime))
        }),
        ("recovery_after_stale_batches_test", |runtime| {
            Box::pin(run_recovery_after_stale_batches_test(runtime))
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
    // WS replay is cursor-based and exclusive: `from_offset` means
    // "start after this already-consumed DB offset".
    let reconnect_offset = deposit_message.offset();
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

async fn run_fee_below_minimum_rejected_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut replay = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    apply_safe_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount).await?;

    // Submit user-op with max_fee=0, which is below the default frame fee (1060).
    let client = SequencerClient::new(runtime.endpoint())?;
    let domain = eip712_domain(runtime);
    let user_op = UserOp {
        nonce: 0,
        max_fee: 0,
        data: ssz_encode_transfer(alice_address, U256::from(100_u64)).into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(alice.signing_key(), &domain, &user_op)?,
        sender: alice_address.to_string(),
        message: user_op,
    };
    let (status, body) = client.submit_tx_with_status(&request).await?;
    assert_eq!(
        status, 422,
        "expected 422 for fee below minimum, got {status}: {body}"
    );

    ws.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_eq!(replay.current_user_balance(alice_address), deposit_amount);
    assert_eq!(replay.current_user_nonce(alice_address), 0);
    Ok(())
}

async fn run_forged_signature_rejected_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;

    // Sign with Alice's key but claim sender is Bob.
    let client = SequencerClient::new(runtime.endpoint())?;
    let domain = eip712_domain(runtime);
    let user_op = UserOp {
        nonce: 0,
        max_fee: DEFAULT_MAX_FEE,
        data: ssz_encode_transfer(bob_address, U256::from(100_u64)).into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(alice.signing_key(), &domain, &user_op)?,
        sender: bob_address.to_string(),
        message: user_op,
    };
    let (status, body) = client.submit_tx_with_status(&request).await?;
    assert_eq!(
        status, 400,
        "expected 400 for forged signature, got {status}: {body}"
    );
    assert!(
        body.contains("sender mismatch") || body.contains("INVALID_SIGNATURE"),
        "expected sender mismatch error, got: {body}"
    );

    ws.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;
    Ok(())
}

async fn run_concurrent_user_ops_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let signers: Vec<TestSigner> = (1..=4)
        .map(TestSigner::from_default)
        .collect::<Result<_, _>>()?;
    let addresses: Vec<Address> = signers.iter().map(|s| s.address()).collect();

    let mut ws = runtime.ws(0).await?;
    let mut replay = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(100_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    // Fund all signers via L1 deposits.
    for signer in &signers {
        let l1 = runtime.wallet_l1(signer.clone()).await?;
        apply_safe_supported_deposit(runtime, &mut ws, &mut replay, &l1, deposit_amount).await?;
    }

    // Submit transfers concurrently from all signers (each sends to signer 0).
    let recipient = addresses[0];
    let wallets: Vec<_> = signers
        .iter()
        .map(|s| runtime.wallet_l2(s.clone()))
        .collect::<Result<_, _>>()?;

    let mut handles = Vec::new();
    for mut wallet in wallets {
        handles.push(tokio::spawn(async move {
            wallet.transfer(recipient, transfer_amount).await
        }));
    }
    let results = futures::future::join_all(handles).await;
    for (i, result) in results.iter().enumerate() {
        result
            .as_ref()
            .map_err(|err| format!("signer {i} task panicked: {err}"))?
            .as_ref()
            .map_err(|err| format!("signer {i} transfer failed: {err}"))?;
    }

    // Collect all WS user-op messages.
    let mut seen_senders = std::collections::HashSet::new();
    for _ in 0..signers.len() {
        let msg = ws.next_message().await?;
        match &msg {
            WsTxMessage::UserOp { sender, .. } => {
                let addr: Address = sender.parse()?;
                seen_senders.insert(addr);
                replay.apply(msg)?;
            }
            other => return Err(format!("expected user op, got {other:?}").into()),
        }
    }

    // All signers should have broadcast their ops.
    for addr in &addresses {
        assert!(
            seen_senders.contains(addr),
            "missing WS message from {addr}"
        );
    }

    // Each signer: deposited 600k, transferred 100k out, paid gas. Signer 0 receives 4x100k.
    for (i, addr) in addresses.iter().enumerate() {
        let expected_balance = if *addr == recipient {
            // Signer 0: deposit - transfer - gas + 4 * transfer_amount
            deposit_amount - transfer_amount - gas + U256::from(signers.len()) * transfer_amount
        } else {
            deposit_amount - transfer_amount - gas
        };
        assert_eq!(
            replay.current_user_balance(*addr),
            expected_balance,
            "balance mismatch for signer {i}"
        );
        assert_eq!(replay.current_user_nonce(*addr), 1);
    }
    Ok(())
}

async fn run_multi_deposit_same_block_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice).await?;
    let bob_l1 = runtime.wallet_l1(bob).await?;
    let mut replay = ReplayWalletApp::devnet();

    let alice_deposit = U256::from(500_000_u64);
    let bob_deposit = U256::from(300_000_u64);

    // Mint and deposit for both in quick succession (before mining).
    alice_l1
        .mint_and_deposit_supported_token(alice_deposit)
        .await?;
    bob_l1.mint_and_deposit_supported_token(bob_deposit).await?;

    // Mine to make both deposits safe.
    runtime.mine_l1_blocks(1).await?;

    // Expect two direct inputs (one per deposit).
    let portal = runtime.erc20_portal_address();
    replay.apply(ws.expect_direct_input_from(portal).await?)?;
    replay.apply(ws.expect_direct_input_from(portal).await?)?;

    assert_eq!(replay.current_user_balance(alice_address), alice_deposit);
    assert_eq!(replay.current_user_balance(bob_address), bob_deposit);
    assert_eq!(replay.executed_input_count(), 2);
    Ok(())
}

async fn run_shutdown_during_inflight_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(100_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    apply_safe_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount).await?;

    // Submit a transfer, then immediately restart.
    alice_l2.transfer(alice_address, transfer_amount).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    drop(ws);

    runtime.restart().await?;

    // Replay from offset 0 after restart and verify consistency.
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    replay_after.apply(ws_after.expect_user_op_from(alice_address).await?)?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    // Both replays should agree: deposit - gas (self-transfer doesn't change balance).
    let expected_balance = deposit_amount - gas;
    assert_eq!(replay.current_user_balance(alice_address), expected_balance);
    assert_eq!(
        replay_after.current_user_balance(alice_address),
        expected_balance
    );
    assert_eq!(replay.current_user_nonce(alice_address), 1);
    assert_eq!(replay_after.current_user_nonce(alice_address), 1);
    Ok(())
}

async fn run_recovery_after_stale_batches_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay_before = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(100_000_u64);
    let post_recovery_transfer = U256::from(200_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    // Step 1: Fund Alice via L1 deposit.
    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before,
        &alice_l1,
        deposit_amount,
    )
    .await?;

    // Step 2: Alice transfers to Bob (this will be lost after recovery).
    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;

    // Verify pre-recovery state.
    assert_eq!(
        replay_before.current_user_balance(alice_address),
        deposit_amount - transfer_amount - gas,
    );
    assert_eq!(
        replay_before.current_user_balance(bob_address),
        transfer_amount,
    );

    // Step 3: Kill the sequencer (Anvil stays up).
    drop(ws);
    runtime.stop().await?;

    // Step 4: Mine 1200 blocks to make all existing batches stale.
    // The sequencer is down, so batches are never submitted. When the sequencer
    // restarts, l1_safe_head will be >1200 blocks past the frames' safe_block.
    runtime.mine_l1_blocks(1200).await?;

    // Step 5: Respawn the sequencer. Startup recovery should detect staleness.
    runtime.respawn().await?;

    // Step 6: Replay from offset 0 after recovery.
    // The deposit should be re-drained into the recovery batch.
    // The transfer should be GONE (it was in an invalidated batch).
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();

    // Expect the re-drained deposit.
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;

    // No more messages — the transfer was invalidated.
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    // Alice should have her full deposit back (no transfer deducted).
    assert_eq!(
        replay_after.current_user_balance(alice_address),
        deposit_amount,
        "after recovery, Alice should have full deposit (transfer was invalidated)"
    );
    assert_eq!(
        replay_after.current_user_balance(bob_address),
        U256::ZERO,
        "after recovery, Bob should have zero (transfer was invalidated)"
    );
    assert_eq!(replay_after.current_user_nonce(alice_address), 0);

    // Step 8: Verify new work succeeds after recovery.
    let mut alice_l2_fresh = runtime.wallet_l2(alice)?;
    alice_l2_fresh
        .transfer(bob_address, post_recovery_transfer)
        .await?;
    replay_after.apply(ws_after.expect_user_op_from(alice_address).await?)?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        deposit_amount - post_recovery_transfer - gas,
    );
    assert_eq!(
        replay_after.current_user_balance(bob_address),
        post_recovery_transfer,
    );
    assert_eq!(replay_after.current_user_nonce(alice_address), 1);

    Ok(())
}

fn eip712_domain(runtime: &ManagedSequencer) -> alloy_sol_types::Eip712Domain {
    alloy_sol_types::Eip712Domain {
        name: Some("CartesiAppSequencer".to_string().into()),
        version: Some("1".to_string().into()),
        chain_id: Some(U256::from(runtime.domain_chain_id())),
        verifying_contract: Some(runtime.verifying_contract()),
        salt: None,
    }
}

fn ssz_encode_transfer(to: Address, amount: U256) -> Vec<u8> {
    use app_core::application::{Method, Transfer};
    ssz::Encode::as_ssz_bytes(&Method::Transfer(Transfer { amount, to }))
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
