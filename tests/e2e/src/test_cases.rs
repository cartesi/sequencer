// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use crate::{ScenarioFn, ScenarioResult};
use alloy_primitives::{Address, U256};
use rollups_harness::{
    ManagedSequencer, ReplayWalletApp, RespawnAttemptOutcome, RespawnPolicy, TcpProxy, TestSigner,
    WalletL1Client, WsClient, sign_user_op_hex,
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

// ── Zone-math constants for §11 outage matrix + §7 recovery tests ─────────
//
// These derive from the sequencer's default config so a change to
// `MAX_WAIT_BLOCKS`, `SEQ_PREEMPTIVE_MARGIN_BLOCKS`, or `SEQ_SECONDS_PER_BLOCK`
// flows through here automatically. The compile-time asserts below catch any
// drift that would invalidate the zone framing of the tests (e.g., a per-retry
// advance that no longer crosses MAX_WAIT in the orchestrator loop).
//
// The picks (PRE / DANGER / PAST_STALE) are deliberately well inside their
// zones to give tests slack against scheduling jitter and timing drift.

/// Source of truth: shared between sequencer + scheduler via
/// `sequencer_core::MAX_WAIT_BLOCKS`.
const MAX_WAIT_BLOCKS: u64 = sequencer_core::MAX_WAIT_BLOCKS;

/// Default `SEQ_PREEMPTIVE_MARGIN_BLOCKS` from `runtime/config.rs`. If the
/// default changes, update here so `DANGER_THRESHOLD_BLOCKS` stays aligned.
const DEFAULT_PREEMPTIVE_MARGIN_BLOCKS: u64 = 75;

/// Default `SEQ_SECONDS_PER_BLOCK` from `runtime/config.rs`. The harness
/// `advance_wall_and_mine` also assumes this value internally.
const DEFAULT_SECONDS_PER_BLOCK: u64 = 12;

/// Derived: the preemptive-recovery danger threshold. Below this we're safe;
/// above it (but below `MAX_WAIT_BLOCKS`) is the danger zone where the
/// sequencer triggers flush + shutdown but no cascade.
const DANGER_THRESHOLD_BLOCKS: u64 = MAX_WAIT_BLOCKS - DEFAULT_PREEMPTIVE_MARGIN_BLOCKS;

/// Pre-danger pick — well below `DANGER_THRESHOLD_BLOCKS` so background drift
/// can't accidentally tip a test into the danger zone.
const PRE_DANGER_BLOCKS: u64 = 500;

/// Danger-zone pick — comfortably past `DANGER_THRESHOLD_BLOCKS`, comfortably
/// below `MAX_WAIT_BLOCKS`. Used by tests that want "danger detected, no
/// cascade" framing.
const DANGER_ZONE_BLOCKS: u64 = 1150;

/// Past-stale pick — comfortably past `MAX_WAIT_BLOCKS`. Startup recovery
/// must cascade at this point.
const PAST_STALE_BLOCKS: u64 = 1250;

/// Per-retry L1 + wall-clock advance for `respawn_until_stable` loops that
/// start in the danger zone. The closed in-danger batch only cascades once
/// it ages past `MAX_WAIT_BLOCKS`, so each retry has to push the system
/// across that boundary within `RespawnPolicy::max_attempts`. The
/// compile-time check below pins the load-bearing relationship.
const RESPAWN_RETRY_ADVANCE_BLOCKS: u64 = 100;

/// Convert a block count to wall-clock duration assuming the default block time.
const fn blocks_as_duration(blocks: u64) -> Duration {
    Duration::from_secs(blocks * DEFAULT_SECONDS_PER_BLOCK)
}

// Compile-time guards: drift in the constants above that breaks the test
// framing fails the build instead of failing tests at runtime.
const _: () = {
    assert!(
        DANGER_THRESHOLD_BLOCKS < MAX_WAIT_BLOCKS,
        "danger threshold must precede the staleness boundary",
    );
    assert!(
        PRE_DANGER_BLOCKS < DANGER_THRESHOLD_BLOCKS,
        "PRE_DANGER_BLOCKS must stay below DANGER_THRESHOLD_BLOCKS",
    );
    assert!(
        DANGER_ZONE_BLOCKS > DANGER_THRESHOLD_BLOCKS,
        "DANGER_ZONE_BLOCKS must clear DANGER_THRESHOLD_BLOCKS",
    );
    assert!(
        DANGER_ZONE_BLOCKS < MAX_WAIT_BLOCKS,
        "DANGER_ZONE_BLOCKS must stay below MAX_WAIT_BLOCKS (no premature cascade)",
    );
    assert!(
        PAST_STALE_BLOCKS > MAX_WAIT_BLOCKS,
        "PAST_STALE_BLOCKS must exceed MAX_WAIT_BLOCKS (cascade must fire)",
    );
    // Load-bearing for §11.1.5 / §11.3.3 / §7.5.x: starting from a closed
    // in-danger batch, one retry advance must push it past MAX_WAIT_BLOCKS
    // so cascade fires before max_attempts is exhausted. If
    // `RESPAWN_RETRY_ADVANCE_BLOCKS` shrinks or `MAX_WAIT_BLOCKS` grows
    // such that this no longer holds, tests would silently start failing
    // by exhausting their retries — the compile-time check makes the
    // breakage visible immediately.
    assert!(
        DANGER_ZONE_BLOCKS + RESPAWN_RETRY_ADVANCE_BLOCKS > MAX_WAIT_BLOCKS,
        "RESPAWN_RETRY_ADVANCE_BLOCKS must cross MAX_WAIT from DANGER_ZONE in one retry",
    );
};

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
        (
            "restart_after_committed_tx_replays_cleanly_test",
            |runtime| Box::pin(run_restart_after_committed_tx_replays_cleanly_test(runtime)),
        ),
        ("recovery_after_stale_batches_test", |runtime| {
            Box::pin(run_recovery_after_stale_batches_test(runtime))
        }),
        ("sequencer_outage_pre_danger_no_recovery_test", |runtime| {
            Box::pin(run_sequencer_outage_pre_danger_no_recovery_test(runtime))
        }),
        ("sequencer_outage_danger_zone_no_cascade_test", |runtime| {
            Box::pin(run_sequencer_outage_danger_zone_no_cascade_test(runtime))
        }),
        ("provider_outage_past_stale_cascades_test", |runtime| {
            Box::pin(run_provider_outage_past_stale_cascades_test(runtime))
        }),
        ("provider_outage_wall_clock_refuses_boot_test", |runtime| {
            Box::pin(run_provider_outage_wall_clock_refuses_boot_test(runtime))
        }),
        ("wall_clock_backward_jump_no_panic_test", |runtime| {
            Box::pin(run_wall_clock_backward_jump_no_panic_test(runtime))
        }),
        ("stalled_safe_head_startup_refuses_boot_test", |runtime| {
            Box::pin(run_stalled_safe_head_startup_refuses_boot_test(runtime))
        }),
        (
            "provider_outage_pre_danger_sequencer_continues_test",
            |runtime| {
                Box::pin(run_provider_outage_pre_danger_sequencer_continues_test(
                    runtime,
                ))
            },
        ),
        (
            "provider_outage_danger_zone_sequencer_self_exits_test",
            |runtime| {
                Box::pin(run_provider_outage_danger_zone_sequencer_self_exits_test(
                    runtime,
                ))
            },
        ),
        ("provider_outage_short_hiccup_no_recovery_test", |runtime| {
            Box::pin(run_provider_outage_short_hiccup_no_recovery_test(runtime))
        }),
        (
            "both_down_danger_zone_sequencer_first_refuses_boot_test",
            |runtime| {
                Box::pin(run_both_down_danger_zone_sequencer_first_refuses_boot_test(
                    runtime,
                ))
            },
        ),
        (
            "both_down_danger_zone_proxy_first_restart_cycle_recovers_test",
            |runtime| {
                Box::pin(run_both_down_danger_zone_proxy_first_restart_cycle_recovers_test(runtime))
            },
        ),
        (
            "sequencer_outage_danger_zone_coupled_restart_cycle_recovers_test",
            |runtime| {
                Box::pin(
                    run_sequencer_outage_danger_zone_coupled_restart_cycle_recovers_test(runtime),
                )
            },
        ),
        (
            "provider_outage_danger_zone_mid_run_exit_then_restart_cycle_recovers_test",
            |runtime| {
                Box::pin(
                    run_provider_outage_danger_zone_mid_run_exit_then_restart_cycle_recovers_test(
                        runtime,
                    ),
                )
            },
        ),
        (
            "first_boot_l1_unreachable_never_synced_refuses_boot_test",
            |runtime| {
                Box::pin(run_first_boot_l1_unreachable_never_synced_refuses_boot_test(runtime))
            },
        ),
        ("delayed_inclusion_cascades_on_restart_test", |runtime| {
            Box::pin(run_delayed_inclusion_cascades_on_restart_test(runtime))
        }),
        ("aging_open_tip_tolerated_by_zombie_check_test", |runtime| {
            Box::pin(run_aging_open_tip_tolerated_by_zombie_check_test(runtime))
        }),
        ("stalled_safe_head_live_exit_test", |runtime| {
            Box::pin(run_stalled_safe_head_live_exit_test(runtime))
        }),
        (
            "ws_reconnect_at_invalidated_offset_skips_cleanly_test",
            |runtime| {
                Box::pin(run_ws_reconnect_at_invalidated_offset_skips_cleanly_test(
                    runtime,
                ))
            },
        ),
        (
            "ws_subscribe_from_future_offset_waits_silently_test",
            |runtime| {
                Box::pin(run_ws_subscribe_from_future_offset_waits_silently_test(
                    runtime,
                ))
            },
        ),
        (
            "recovery_drains_safe_but_undrained_direct_input_test",
            |runtime| {
                Box::pin(run_recovery_drains_safe_but_undrained_direct_input_test(
                    runtime,
                ))
            },
        ),
        (
            "recovery_batch_opens_empty_when_no_direct_inputs_pending_test",
            |runtime| {
                Box::pin(run_recovery_batch_opens_empty_when_no_direct_inputs_pending_test(runtime))
            },
        ),
        ("replay_matches_live_for_mixed_workload_test", |runtime| {
            Box::pin(run_replay_matches_live_for_mixed_workload_test(runtime))
        }),
        (
            "provider_outage_input_reader_retries_after_reconnect_test",
            |runtime| {
                Box::pin(run_provider_outage_input_reader_retries_after_reconnect_test(runtime))
            },
        ),
        (
            "first_boot_no_cache_l1_unreachable_refuses_boot_test",
            |runtime| {
                Box::pin(run_first_boot_no_cache_l1_unreachable_refuses_boot_test(
                    runtime,
                ))
            },
        ),
        (
            "chain_id_mismatch_via_live_rpc_refuses_boot_test",
            |runtime| {
                Box::pin(run_chain_id_mismatch_via_live_rpc_refuses_boot_test(
                    runtime,
                ))
            },
        ),
        (
            "nonce_zero_recovery_invalidates_then_accepts_at_nonce_zero_test",
            |runtime| {
                Box::pin(
                    run_nonce_zero_recovery_invalidates_then_accepts_at_nonce_zero_test(runtime),
                )
            },
        ),
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

// Restart after a committed tx and verify replay stays consistent.
//
// This is intentionally not an "in-flight request during shutdown" test:
// `WalletL2Client::transfer()` awaits the HTTP ack, so by the time restart
// happens the user-op is already durable. What this locks down is the
// committed-tx replay path across restart.
async fn run_restart_after_committed_tx_replays_cleanly_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
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

    // Step 4: Simulate ~4h of outage: advance both L1 and wall clock by
    // MAX_WAIT_BLOCKS * SECONDS_PER_BLOCK = 1200 * 12 = 14400s. On respawn,
    // l1_safe_head will be >1200 blocks past the frames' safe_block.
    runtime
        .advance_wall_and_mine(blocks_as_duration(MAX_WAIT_BLOCKS))
        .await?;

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

// ── §11.1.1 — Sequencer outage, pre-danger zone ────────────────────────────
//
// Sequencer stops with an open batch (deposit + transfer); L1 advances 500
// blocks (well below the danger threshold of ~1125). On restart:
//   - Startup recovery runs but finds no danger zone → no flush.
//   - No batches are stale → no cascade invalidation.
//   - The deposit and transfer persist across the restart.
//   - New txs succeed against the unchanged state.
//
// This is the positive control for the recovery procedure: it must NOT fire
// (or over-fire) when L1 hasn't drifted enough to cause trouble.

async fn run_sequencer_outage_pre_danger_no_recovery_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Pick an advance that's safely below the 1125-block danger threshold
    // (MAX_WAIT_BLOCKS 1200 - default margin 75 = 1125).
    const PRE_DANGER: Duration = blocks_as_duration(PRE_DANGER_BLOCKS);

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
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    // Step 1: Fund Alice and record a transfer.
    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before,
        &alice_l1,
        deposit_amount,
    )
    .await?;
    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;

    let expected_alice_balance = deposit_amount - transfer_amount - gas;
    let expected_bob_balance = transfer_amount;

    // Step 2: Stop the sequencer. Leave Anvil running.
    drop(ws);
    runtime.stop().await?;

    // Step 3: Advance L1 + wall-clock a pre-danger amount (500 blocks ≈ 100min
    // < 1125 block danger threshold).
    runtime.advance_wall_and_mine(PRE_DANGER).await?;

    // Step 4: Restart. No recovery should fire.
    runtime.respawn().await?;

    // Step 5: Replay via WS from offset 0. Both the deposit and transfer must
    // still be present (no invalidation).
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    replay_after.apply(ws_after.expect_user_op_from(alice_address).await?)?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        expected_alice_balance,
        "pre-danger restart must preserve Alice's balance",
    );
    assert_eq!(
        replay_after.current_user_balance(bob_address),
        expected_bob_balance,
        "pre-danger restart must preserve Bob's balance",
    );
    assert_eq!(
        replay_after.current_user_nonce(alice_address),
        1,
        "Alice's nonce must NOT be reset",
    );

    // Step 6: No further messages queued. Confirm nothing else comes through.
    // (A follow-up "new work succeeds" step is omitted here because the
    // harness's `wallet_l2` initializes its local nonce counter at 0, and
    // this scenario explicitly does NOT reset the on-chain nonce — the
    // post-restart nonce is 1. Adding a "submit at nonce 1" check would
    // require harness plumbing beyond the scope of this regression test.)
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    Ok(())
}

// ── §11.1.2 — Sequencer outage, danger zone (not yet stale) ────────────────
//
// Sequencer stops; L1 advances into the danger zone (past 1125 blocks) but
// strictly below the staleness threshold (1200). On restart:
//   - `check_danger_zone` returns Some(_) — flush runs (no-op: nothing was
//     submitted and no w_nonce is pending).
//   - `detect_and_recover` finds nothing stale — no cascade.
//   - Pre-outage state is preserved (same positive invariant as §11.1.1).
//
// This exercises the flush-runs-but-cascade-doesn't path specifically.

async fn run_sequencer_outage_danger_zone_no_cascade_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Pick advance in the danger zone: > danger_threshold (1125) but < MAX_WAIT (1200).
    // Decoupled from wall clock on purpose: this test exercises the
    // block-based danger check in isolation. A coupled advance (wall+L1)
    // is more realistic but triggers the aged-Tip → close → submitter-
    // detects-danger → flush-and-restart cycle, which is a different
    // scenario (tracked separately — coupling this test would need the
    // harness to handle the restart cycle). §11.2.x cells use the proxy
    // to exercise the danger-zone + flush path with realistic timing.
    // Uses module-level `DANGER_ZONE_BLOCKS` (see top-of-file zone constants).

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
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before,
        &alice_l1,
        deposit_amount,
    )
    .await?;
    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;

    let expected_alice_balance = deposit_amount - transfer_amount - gas;
    let expected_bob_balance = transfer_amount;

    drop(ws);
    runtime.stop().await?;

    // L1 advances into the danger zone but strictly below the staleness
    // threshold. The danger-zone path should fire (flush is a no-op here
    // because no batch was ever submitted to L1), and the recovery procedure
    // should find no stale batches.
    runtime.mine_l1_blocks(DANGER_ZONE_BLOCKS).await?;

    runtime.respawn().await?;

    // Same positive invariant as §11.1.1: pre-outage state preserved, nonces
    // not reset, feed replay produces identical history.
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    replay_after.apply(ws_after.expect_user_op_from(alice_address).await?)?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        expected_alice_balance,
        "danger-zone restart must preserve Alice's balance \
         (flush runs but no cascade)",
    );
    assert_eq!(
        replay_after.current_user_balance(bob_address),
        expected_bob_balance,
    );
    assert_eq!(
        replay_after.current_user_nonce(alice_address),
        1,
        "nonce must not be reset when no cascade happens",
    );

    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    Ok(())
}

// ── §11.2.3 — Provider outage, past-stale (recovery through proxy) ──────────
//
// Scenario: the sequencer is routed through a `TcpProxy`, simulating a
// gateway in front of the real L1 node. While the sequencer is stopped,
// a temporary outage happens (proxy disconnected), L1 advances past the
// staleness threshold, and the outage ends (proxy reconnected). The next
// sequencer restart connects via the proxy, sees the advanced safe head,
// and cascade-invalidates the stale open batch.
//
// What this locks down that the sequencer-outage tests don't:
//   - The proxy is actually wired into the RPC path. Subsequent RPC calls
//     from the sequencer (safe-head sync, batch submission) route through
//     it. If `set_l1_endpoint_override` ever regressed (e.g., respawn
//     ignored the override), this test would fail.
//   - Recovery over a non-direct connection works end-to-end.
//
// Note on wall-clock fallback: in principle this scenario would also test
// the fallback refusing to boot when L1 is unreachable AND real time has
// elapsed past the danger threshold. In practice, `anvil_mine(N)` takes
// milliseconds of real wall-clock time, so the fallback correctly reports
// "not yet in danger by wall-clock" and lets the sequencer boot with stale
// data. Exercising the wall-clock-refuses-to-boot path requires either
// direct `synced_at_ms` DB manipulation or a time-skew tool — deferred.

async fn run_provider_outage_past_stale_cascades_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Advance comfortably past staleness so the test is robust to small
    // scheduling drifts.
    const PAST_STALE: Duration = blocks_as_duration(PAST_STALE_BLOCKS);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Step 1: Normal setup — deposit + transfer (the transfer will be lost).
    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay_before = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(100_000_u64);

    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before,
        &alice_l1,
        deposit_amount,
    )
    .await?;
    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;

    // Step 2: Stop the sequencer and insert a proxy into the L1 path.
    drop(ws);
    runtime.stop().await?;

    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));

    // Step 3: Simulate a gateway outage that spans the staleness window.
    //   - Disconnect the proxy (gateway is down).
    //   - Mine 1250 blocks directly on Anvil (bypasses the proxy).
    //   - Reconnect the proxy (gateway is back).
    // During the outage the sequencer is stopped; when it comes back up,
    // it will see the advanced safe head through the proxy.
    proxy.disconnect();
    runtime.advance_wall_and_mine(PAST_STALE).await?;
    proxy.reconnect();

    // Step 4: Respawn. The sequencer dials the proxy, the proxy forwards
    // to Anvil, `sync_to_current_safe_head` returns 1250+ blocks past the
    // open batch's first frame. `check_open_batch_staleness` fires, cascade
    // invalidates, recovery batch opens.
    runtime.respawn().await?;

    // Step 5: Verify via WS replay.
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        deposit_amount,
        "transfer must be invalidated after past-stale outage routed through proxy",
    );
    assert_eq!(
        replay_after.current_user_balance(bob_address),
        U256::ZERO,
        "Bob's receiving balance must be rolled back",
    );
    assert_eq!(replay_after.current_user_nonce(alice_address), 0);

    // Step 6: Tear down the proxy cleanly.
    proxy.shutdown().await?;

    Ok(())
}

// ── §7.8.1 — Wall-clock fallback refuses to boot past danger threshold ─────
//
// Scenario: L1 is unreachable AND wall-clock time has elapsed past the
// danger threshold since the last successful L1 sync. The sequencer must
// refuse to boot — proceeding would mean issuing soft confirmations against
// stale L1 state, potentially missing that batches are already doomed.
//
// This test only became possible after the `find_first_batch_in_danger`
// unification. Prior to that fix, an open batch was invisible to
// `check_danger_zone`, so the wall-clock fallback could "miss" an open
// batch aging into danger while L1 was unreachable and boot anyway.
//
// The wall-clock illusion is created without OS tooling: `rewind_synced_at_ms`
// rewrites `l1_safe_head.synced_at_ms` to an older timestamp, equivalent
// to advancing the wall clock from the sequencer's perspective. We mine
// an equivalent number of blocks on Anvil to keep the block-time coupling
// documented in `docs/threat-model/README.md`.

async fn run_provider_outage_wall_clock_refuses_boot_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Pick an elapsed time comfortably past the danger threshold. Defaults:
    // seconds_per_block=12, danger_threshold=MAX_WAIT_BLOCKS(1200)-margin(75)=1125.
    // We need elapsed_secs / 12 > 1125 → elapsed_secs > 13500. Use 5h.
    const OUTAGE: Duration = Duration::from_secs(5 * 60 * 60);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Step 1: Normal setup — deposit + transfer (transfer will be lost).
    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay_before = ReplayWalletApp::devnet();

    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before,
        &alice_l1,
        U256::from(600_000_u64),
    )
    .await?;
    alice_l2
        .transfer(bob_address, U256::from(100_000_u64))
        .await?;
    replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;

    // Step 2: Stop the sequencer, insert proxy, disconnect it, advance both
    // the wall clock and L1 by the outage duration — block-time coupled so
    // the sequencer sees a consistent view (5h ≈ 1500 blocks at 12s/block).
    drop(ws);
    runtime.stop().await?;

    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();
    runtime.advance_wall_and_mine(OUTAGE).await?;

    // Step 3: Attempt respawn with proxy disconnected. The sequencer:
    //   - dials the proxy → sync_to_current_safe_head fails (L1 unreachable).
    //   - falls back to wall-clock estimation.
    //   - computes missed_blocks = 18000s / 12 = 1500 > danger_threshold 1125.
    //   - `find_first_batch_in_danger(adjusted_threshold=0)` flags the open
    //     batch (first_frame_safe_block << current_safe_block - 0).
    //   - returns StartupDangerZoneEstimate → process exits with failure.
    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "respawn must fail: wall-clock says past-danger AND open batch is in danger",
    );

    // Step 4: Reconnect the proxy and respawn normally. Sync now succeeds,
    // the stale open batch is cascade-invalidated, recovery batch opens.
    proxy.reconnect();
    runtime.respawn().await?;

    // Step 5: Verify the invalidation: only the re-drained deposit appears.
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        U256::from(600_000_u64),
        "transfer must be invalidated after wall-clock-triggered recovery",
    );
    assert_eq!(replay_after.current_user_balance(bob_address), U256::ZERO,);
    assert_eq!(replay_after.current_user_nonce(alice_address), 0);

    proxy.shutdown().await?;
    Ok(())
}

// §7.8.3: `SystemTime::now()` backward jump → `saturating_sub` handles
// cleanly, no panic.
//
// Scenario: normal setup creates DB state at real time T. Stop, disconnect
// proxy, backward-jump the clock via faketime, respawn with L1 unreachable.
// The wall-clock fallback runs:
//
//     elapsed = now(T-1h).saturating_sub(last_sync_at_ms(≈T)) = 0
//
// No danger → boot proceeds. After reconnect, normal operation resumes.
// If `saturating_sub` ever regresses to a plain subtraction (underflow
// panic on u64), this test panics at respawn.
async fn run_wall_clock_backward_jump_no_panic_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut ws = runtime.ws(0).await?;
    let mut replay_before = ReplayWalletApp::devnet();

    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before,
        &alice_l1,
        U256::from(100_000_u64),
    )
    .await?;
    drop(ws);

    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();
    runtime.set_faketime_offset(Some("-1h".to_string()))?;

    // Respawn must NOT panic. With L1 unreachable, the wall-clock fallback
    // is the only path that sees `now - last_sync_ms` — if the subtraction
    // ever became non-saturating, this call would panic via u64 underflow.
    runtime.respawn().await?;

    // Clean up: reconnect and let the sequencer catch up normally.
    proxy.reconnect();
    // Clear the offset for subsequent respawns (not used here, but keeps the
    // teardown deterministic if future cleanup code respawns).
    runtime.set_faketime_offset(None)?;

    proxy.shutdown().await?;
    Ok(())
}

// §7.8.5 — Provider reachable, safe head frozen, startup refuses to boot.
//
// Scenario:
//   1. Create an open Tip and age it into the danger window while L1 is
//      still reachable.
//   2. Stop the sequencer without mining any more L1 blocks, so the next
//      startup sees the same safe head again.
//   3. Jump only the sequencer's wall clock forward by >1 block interval.
//      Startup sync succeeds, but because the safe head did not advance, the
//      reader preserves the old safe-progress timestamp.
//   4. `stalled_safe_head_danger_estimate` treats that as a reachable-but-
//      frozen safe head and refuses boot.
//   5. Mine one more L1 block and respawn again; safe-head progress resumes,
//      the timestamp refreshes, and the sequencer stays up.
async fn run_stalled_safe_head_startup_refuses_boot_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    const STALLED_SAFE_HEAD_OFFSET: &str = "+30s";
    const SAFE_HEAD_SYNC_WINDOW: Duration = Duration::from_secs(8);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            U256::from(600_000_u64),
        )
        .await?;
        alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    runtime.mine_l1_blocks(DANGER_ZONE_BLOCKS).await?;

    let early_exit = runtime.observe_for(SAFE_HEAD_SYNC_WINDOW).await?;
    assert!(
        early_exit.is_none(),
        "aging open Tip alone must not crash while the safe head is still progressing: \
         got unexpected exit {early_exit:?}",
    );

    runtime.stop().await?;
    runtime.set_faketime_offset(Some(STALLED_SAFE_HEAD_OFFSET.to_string()))?;

    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "startup must refuse when L1 is reachable but the safe head stayed frozen long enough to estimate danger",
    );

    let counts = runtime.count_batches()?;
    assert_eq!(
        counts.invalidated, 0,
        "startup refusal on a reachable-but-stalled safe head must not cascade batches: {counts:?}",
    );

    runtime.mine_l1_blocks(1).await?;
    runtime.respawn().await?;

    let stable_after_progress = runtime.observe_for(SAFE_HEAD_SYNC_WINDOW).await?;
    assert!(
        stable_after_progress.is_none(),
        "once safe-head progress resumes, the sequencer should boot and remain stable; got {stable_after_progress:?}",
    );

    Ok(())
}

// §11.2.1: provider outage in the pre-danger zone while the sequencer stays
// running.
//
// Load-under-outage check: the sequencer must continue to accept user ops,
// persist them, broadcast on WS, and CLOSE BATCHES BY SIZE while its L1
// connection is down. Proves the inclusion lane is independent of L1
// reachability — as long as the wall-clock fallback keeps the pre-danger
// verdict, the sequencer keeps doing useful work.
//
// Scenario:
//   1. Spawn + apply a large deposit so Alice can fund many transfers.
//   2. Route the sequencer through a proxy (stop → set override → respawn).
//   3. Disconnect the proxy, advance L1 by a pre-danger amount (500 blocks).
//   4. Submit enough transfers (~150 × ~100 B each ≈ 15 KB) to exceed the
//      default ~12 KB batch-size target, guaranteeing at least one size-
//      triggered batch close during the outage.
//   5. Assert `count_batches().sealed` strictly increased during the outage.
//   6. Reconnect the proxy; confirm one more transfer goes through and the
//      schema invariants hold post-test.
async fn run_provider_outage_pre_danger_sequencer_continues_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Pre-danger budget: see module-level `PRE_DANGER_BLOCKS` (500 blocks =
    // 100min at 12s/block, well below the danger threshold).
    // Enough transfers to exceed the default ~12 KB batch size target. Each
    // transfer user_op is ≈ 100 B (SSZ-encoded Transfer + signature + nonce),
    // so 150 ops ≈ 15 KB — one batch close is guaranteed; two or more is
    // typical.
    const TRANSFERS_DURING_OUTAGE: usize = 150;

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Step 1: Deposit big — Alice needs to cover 150+ transfers and their fees.
    // Default fee per user-op ≈ 3873 units (log-fee 1060); reserve margin.
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let deposit_amount = U256::from(10_000_000_u64);
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
            .await?;
    }

    // Step 2: Insert the proxy and route the sequencer through it via
    // stop → set override → respawn. The initial spawn (direct to Anvil) is
    // treated as setup only; from here on, all sequencer → L1 traffic flows
    // through the proxy.
    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    runtime.respawn().await?;

    // Step 3: Connect a fresh WS (catches up the deposit from offset 0) and
    // a fresh L2 wallet. Consume the deposit replay so subsequent
    // `expect_user_op_from` calls line up.
    let mut ws = runtime.ws(0).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    ws.expect_direct_input_from(runtime.erc20_portal_address())
        .await?;

    // Baseline: one transfer while the proxy is still connected, confirming
    // end-to-end plumbing works through the proxy.
    alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    let batches_before = runtime.count_batches()?;

    // Step 4: Cut the L1 connection, advance Anvil by 500 blocks (pre-danger).
    // The sequencer is still running; its wall-clock fallback sees real time
    // not yet past the threshold, so it keeps retrying rather than shutting
    // down.
    proxy.disconnect();
    runtime.mine_l1_blocks(PRE_DANGER_BLOCKS).await?;

    // Step 5: Submit many transfers during the outage. Each should be
    // accepted (POST /tx succeeds), broadcast on WS, and eventually packed
    // into a new batch. Size-triggered close fires when the cumulative user-op
    // bytes exceed the default target.
    for _ in 0..TRANSFERS_DURING_OUTAGE {
        alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    // Step 6: Batch closure during outage — the whole point of this test.
    let batches_mid = runtime.count_batches()?;
    assert!(
        batches_mid.sealed > batches_before.sealed,
        "sequencer must continue closing batches during L1 outage: \
         before={before:?}, after={after:?}",
        before = batches_before,
        after = batches_mid,
    );

    // Step 7: Restore L1 connectivity. The batch submitter's next tick
    // reaches L1 again and starts draining the pending batches.
    proxy.reconnect();

    // Final check: one more transfer goes through after reconnect, proving
    // the sequencer didn't just survive — it's fully operational.
    alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    // Sanity: total sealed batches grew from baseline to final, and nothing
    // got invalidated (pre-danger → no recovery triggered).
    let batches_final = runtime.count_batches()?;
    assert!(
        batches_final.sealed > batches_before.sealed,
        "final sealed count {final:?} must exceed baseline {before:?}",
        final = batches_final,
        before = batches_before,
    );
    assert_eq!(
        batches_final.invalidated, 0,
        "pre-danger outage must not invalidate any batches, got {:?}",
        batches_final,
    );

    proxy.shutdown().await?;
    Ok(())
}

// §11.2.2: provider outage aging into the danger zone while the sequencer is
// running — sequencer detects via its live wall-clock fallback and self-exits
// with `DangerZone`. Also verifies the startup wall-clock fallback refuses
// subsequent boots while L1 is still unreachable.
//
// The full "reconnect → recover → no cascade" cycle needs the harness to
// handle an orchestrator-style restart loop (the first post-reconnect boot
// may still trip the danger check and exit, requiring another boot after
// enough blocks age out). That's tracked as §11.1.5 / §11.2.2-follow-up and
// deliberately out of scope here.
//
// Uses dynamic faketime (FAKETIME_TIMESTAMP_FILE re-read on every time call)
// to jump the sequencer's clock past the danger threshold mid-run without
// respawning — the scenario we'd otherwise need 3h45min of real wall-clock
// time to reproduce.
async fn run_provider_outage_danger_zone_sequencer_self_exits_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Defaults: MAX_WAIT_BLOCKS=1200, margin=75, danger_threshold=1125
    // blocks at 12s/block = 13500s = 3h45min. Use 3h55min: past danger,
    // under MAX_WAIT (so no cascade fires later).
    const INTO_DANGER: Duration = Duration::from_secs(3 * 60 * 60 + 55 * 60);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Step 1: Baseline — deposit + transfer so there's observable state.
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            U256::from(500_000_u64),
        )
        .await?;
    }

    // Step 2: Switch routing to the proxy (stop → set override → respawn).
    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    runtime.respawn().await?;

    let mut ws = runtime.ws(0).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    ws.expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    // Step 3: Disconnect the proxy and advance both clocks into the danger
    // zone. The running sequencer's batch-submitter tick will try L1, hit
    // the proxy's disconnect, fall into the wall-clock fallback, and see
    // elapsed > danger_threshold → exit with DangerZone.
    proxy.disconnect();
    runtime.advance_wall_and_mine(INTO_DANGER).await?;

    // Step 4: Wait for the sequencer to detect and self-exit. Dynamic
    // faketime means the shift hits the submitter's next tick immediately —
    // no real-time wait needed.
    let exit_status = runtime.wait_for_exit(Duration::from_secs(30)).await?;
    assert!(
        !exit_status.success(),
        "sequencer must self-exit with non-zero status on DangerZone, got {exit_status:?}",
    );

    // Step 5: Try to respawn while proxy is still disconnected. Startup
    // runs the same wall-clock fallback via `run_preemptive_recovery` and
    // should refuse to boot (`StartupDangerZoneEstimate`).
    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "respawn must fail while proxy disconnected and wall-clock past danger",
    );

    // No cascade happened yet — batches under MAX_WAIT are not invalidated
    // by startup recovery, only preemptively shut-down-and-flushed.
    let counts = runtime.count_batches()?;
    assert_eq!(
        counts.invalidated, 0,
        "danger-zone (not past-stale) must not invalidate batches: {counts:?}",
    );

    proxy.shutdown().await?;
    Ok(())
}

// §11.4.1 — Short-duration provider hiccup, heals within pre-danger.
//
// The most-common production fault: an RPC gateway flakes briefly, retries
// succeed. No recovery should fire.
//
// What this tests that §11.2.1 doesn't: §11.2.1 disconnects for a
// 500-block L1 advance + 150 transfers worth of real time, exercising the
// inclusion lane under load. §11.4.1 instead exercises the "pure retry
// loop" path: **no** L1 advance, **no** faketime advance, just a few seconds
// of real-time wall-clock downtime across at least one
// `idle_poll_interval_ms` (default 5 s) so the submitter definitely attempts
// and fails a tick, then the reconnect path lets the next tick succeed.
//
// Scenario:
//   1. Route through proxy; establish a baseline transfer.
//   2. Disconnect, submit one more transfer (inclusion lane must still
//      accept), sleep >5 s so the submitter's tick hits the disconnect.
//   3. Reconnect, submit another transfer.
//   4. Assert no batches were invalidated and POST /tx still works.
async fn run_provider_outage_short_hiccup_no_recovery_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Long enough to straddle the default 5 s submitter idle_poll_interval so
    // at least one retry actually fails against the disconnected proxy.
    const HICCUP_DURATION: Duration = Duration::from_secs(6);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let deposit_amount = U256::from(2_000_000_u64);
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
            .await?;
    }

    // Route through the proxy (stop → override → respawn).
    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    runtime.respawn().await?;

    let mut ws = runtime.ws(0).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    ws.expect_direct_input_from(runtime.erc20_portal_address())
        .await?;

    // Baseline transfer via the proxy, proving the proxy path works.
    alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    let batches_before = runtime.count_batches()?;

    // Disconnect: the submitter's next tick (within 5 s) fails against the
    // disconnected proxy, runs wall_clock_danger_estimate with ~zero
    // elapsed — far below danger threshold — and just retries.
    proxy.disconnect();

    // Inclusion lane is independent of L1; POST /tx still accepts.
    alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    // Wait at least one full submitter idle_poll_interval (default 5 s) so the
    // failed-retry path is definitely exercised under the disconnect.
    tokio::time::sleep(HICCUP_DURATION).await;

    proxy.reconnect();

    // Reconnect: another transfer goes through normally — proves the
    // sequencer didn't just sit there, its next tick genuinely recovered.
    alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    let batches_after = runtime.count_batches()?;
    assert_eq!(
        batches_after.invalidated, 0,
        "a short pre-danger hiccup must not invalidate any batch: {batches_after:?}",
    );
    assert!(
        batches_after.sealed >= batches_before.sealed,
        "sealed-batch count must be monotonic across a hiccup: \
         before={batches_before:?}, after={batches_after:?}",
    );

    proxy.shutdown().await?;
    Ok(())
}

// §11.3.2 — Both down, sequencer returns first into the danger zone, refuses
// to boot.
//
// Companion to §11.2.3 (past-stale cascades through proxy): this is the
// *danger-zone* window of the same setup. Sequencer is stopped AND the proxy
// is disconnected; wall-clock and L1 advance into the danger zone but stay
// below `MAX_WAIT_BLOCKS`; the sequencer comes back first while L1 is still
// unreachable. Startup's wall-clock fallback must see "past danger" and
// refuse the boot — advancing the safe head off stale data would risk
// issuing soft confirmations against a state that may already be doomed.
//
// No cascade is expected yet (we haven't crossed MAX_WAIT_BLOCKS). The test
// stops at the refuse-to-boot assertion — the full reconnect+recovery cycle
// is covered by §11.3.3 below.
async fn run_both_down_danger_zone_sequencer_first_refuses_boot_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Safely inside the danger zone: past 1125-block threshold, below 1200.
    // 3h55min at 12 s/block = 1175 blocks — same slot the existing
    // §11.2.2 test uses.
    const INTO_DANGER: Duration = Duration::from_secs(3 * 60 * 60 + 55 * 60);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Baseline: deposit + transfer (both will survive — no cascade expected
    // in the danger-zone window).
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay_before = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay_before,
            &alice_l1,
            U256::from(600_000_u64),
        )
        .await?;
        alice_l2
            .transfer(bob_address, U256::from(100_000_u64))
            .await?;
        replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    // Both down: stop sequencer, insert proxy, disconnect proxy.
    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();

    // Coupled advance into the danger zone. Anvil mines behind the proxy
    // (direct connection via `mine_l1_blocks`), and faketime shifts the
    // sequencer's wall clock cumulatively.
    runtime.advance_wall_and_mine(INTO_DANGER).await?;

    // Respawn while proxy is still disconnected: sync fails → wall-clock
    // fallback computes past-danger → refuses to boot.
    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "sequencer must refuse to boot while L1 unreachable and wall-clock past danger",
    );

    // No cascade should have run yet — we haven't crossed MAX_WAIT_BLOCKS.
    let counts = runtime.count_batches()?;
    assert_eq!(
        counts.invalidated, 0,
        "refuse-to-boot must not invalidate any batch: {counts:?}",
    );

    proxy.shutdown().await?;
    Ok(())
}

// §11.3.3 — Both down, proxy returns first, then sequencer — restart cycle
// converges.
//
// Complement to §11.3.2 (sequencer first): here L1 comes back before the
// sequencer does. Once the sequencer restarts, startup recovery's wall-clock
// fallback sees L1 is now reachable and proceeds. The first boot cycle
// closes the aged Tip, the submitter detects a closed batch in danger, and
// the process exits. The orchestrator (simulated by `respawn_until_stable`)
// retries after a small additional L1 advance — the closed batch ages past
// `MAX_WAIT_BLOCKS`, startup recovery cascades, a fresh recovery batch opens,
// and the sequencer is healthy.
//
// The key invariant this tests that the existing §11.x tests don't: the full
// *restart-loop* works. Earlier tests stopped at "first respawn exits"
// because the harness lacked an orchestrator-restart primitive; now we have
// `respawn_until_stable`, so we can drive the loop to convergence.
async fn run_both_down_danger_zone_proxy_first_restart_cycle_recovers_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    const INTO_DANGER: Duration = Duration::from_secs(3 * 60 * 60 + 55 * 60);
    // Each restart attempt advances ~20 min (100 blocks) of additional L1
    // time, simulating the real orchestrator-restart cadence. One extra
    // tick past the first failed attempt is enough to push an aged Tip's
    // closed-batch form past MAX_WAIT_BLOCKS (1175 + 100 > 1200).
    const ADVANCE_PER_RETRY: Duration = blocks_as_duration(RESPAWN_RETRY_ADVANCE_BLOCKS);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Baseline: deposit + transfer — the transfer will be invalidated when
    // cascade finally fires.
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay_before = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay_before,
            &alice_l1,
            U256::from(600_000_u64),
        )
        .await?;
        alice_l2
            .transfer(bob_address, U256::from(100_000_u64))
            .await?;
        replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    // Both down: stop sequencer, insert proxy, disconnect.
    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();

    // Coupled advance into the danger zone.
    runtime.advance_wall_and_mine(INTO_DANGER).await?;

    // L1 recovers first — proxy back online while sequencer is still stopped.
    proxy.reconnect();

    // Simulated orchestrator loop. Each failed attempt advances L1 (and
    // wall-clock) by ~20 min; the aged Tip eventually ages past
    // `MAX_WAIT_BLOCKS` and cascade fires on a subsequent respawn.
    let outcomes = runtime
        .respawn_until_stable(RespawnPolicy {
            max_attempts: 5,
            stabilization: Duration::from_secs(8),
            advance_per_retry: Some(ADVANCE_PER_RETRY),
        })
        .await?;
    assert!(
        matches!(outcomes.last(), Some(RespawnAttemptOutcome::Stable)),
        "restart cycle must converge to Stable, got: {outcomes:?}",
    );

    // The cascade fired somewhere in the loop — the transfer was invalidated.
    let counts = runtime.count_batches()?;
    assert!(
        counts.invalidated >= 1,
        "expected at least one invalidation after restart-cycle cascade: {counts:?}",
    );

    // Verify via WS replay: only the re-drained deposit appears, the transfer
    // is gone.
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        U256::from(600_000_u64),
        "Alice must get her full deposit back after cascade",
    );
    assert_eq!(
        replay_after.current_user_balance(bob_address),
        U256::ZERO,
        "Bob's balance must roll back",
    );
    assert_eq!(replay_after.current_user_nonce(alice_address), 0);

    proxy.shutdown().await?;
    Ok(())
}

// §11.1.5 — Sequencer outage, coupled wall+L1 advance into the danger zone,
// orchestrator restart cycle converges.
//
// The realistic counterpart to the decoupled §11.1.2
// (`sequencer_outage_danger_zone_no_cascade_test`), which advances L1 without
// touching the wall clock to keep the aged-Tip-auto-close path out of scope.
// In a real outage both advance together, which means: on respawn, the aged
// Tip's `max_open_time` is exceeded, the inclusion lane closes it into a
// now-nonced closed batch, and the submitter's first tick detects the closed
// batch is in the danger zone (`age > danger_threshold`) and exits with
// `BatchSubmitterError::DangerZone`.
//
// That's a flush-and-restart signal, not a cascade. Under orchestration, the
// next boot's preemptive recovery runs `check_danger_zone` (closed-only),
// flushes the mempool (no-op here — nothing was ever submitted), re-syncs,
// then runs `run_startup_recovery` with the `MAX_WAIT_BLOCKS` threshold. The
// latter only cascades once the closed batch has aged past 1200 blocks —
// which happens once enough additional L1 blocks accumulate across
// orchestrator retries.
//
// Proves the sequencer-outage danger-zone path (not just the provider-outage
// analogue §11.2.2) follows the same flush/shutdown → respawn → cascade
// lifecycle to a healthy state.
async fn run_sequencer_outage_danger_zone_coupled_restart_cycle_recovers_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    const INTO_DANGER: Duration = Duration::from_secs(3 * 60 * 60 + 55 * 60);
    const ADVANCE_PER_RETRY: Duration = blocks_as_duration(RESPAWN_RETRY_ADVANCE_BLOCKS);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay_before = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay_before,
            &alice_l1,
            U256::from(600_000_u64),
        )
        .await?;
        alice_l2
            .transfer(bob_address, U256::from(100_000_u64))
            .await?;
        replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    // Sequencer outage: stop, do NOT insert a proxy. Coupled L1+wall advance
    // into the danger zone.
    runtime.stop().await?;
    runtime.advance_wall_and_mine(INTO_DANGER).await?;

    let outcomes = runtime
        .respawn_until_stable(RespawnPolicy {
            max_attempts: 5,
            stabilization: Duration::from_secs(8),
            advance_per_retry: Some(ADVANCE_PER_RETRY),
        })
        .await?;
    assert!(
        matches!(outcomes.last(), Some(RespawnAttemptOutcome::Stable)),
        "restart cycle must converge to Stable, got: {outcomes:?}",
    );

    // At least one orchestrator cycle expected before convergence — the
    // first respawn succeeds but the submitter tick exits with DangerZone.
    assert!(
        outcomes.len() >= 2,
        "danger-zone restart cycle must involve at least one failed attempt \
         before converging (else we're not exercising the flush/shutdown path): {outcomes:?}",
    );

    let counts = runtime.count_batches()?;
    assert!(
        counts.invalidated >= 1,
        "expected cascade-invalidation after restart-cycle: {counts:?}",
    );

    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        U256::from(600_000_u64),
        "cascade must roll Alice back to the full deposit",
    );
    assert_eq!(replay_after.current_user_balance(bob_address), U256::ZERO);
    assert_eq!(replay_after.current_user_nonce(alice_address), 0);

    Ok(())
}

// §11.2.2 follow-up — Provider outage into the danger zone while the
// sequencer is running, mid-run DangerZone exit, then reconnect + restart
// cycle converges.
//
// The existing §11.2.2 test stops at "refuse to reboot while proxy still
// disconnected". This completes that story: after the sequencer self-exits
// mid-run via its live wall-clock fallback and the proxy reconnects, the
// orchestrator restart cycle eventually converges — same
// `respawn_until_stable` pattern as §11.1.5 / §11.3.3.
//
// Ordering detail: the wall-clock advance only advances the sequencer's
// clock; the proxy has been disconnecting Anvil traffic, so Anvil's block
// count advanced via `mine_l1_blocks` (which bypasses the proxy). When the
// proxy reconnects, the sequencer sees both the shifted wall clock and the
// fresh safe head via the same RPC connection.
async fn run_provider_outage_danger_zone_mid_run_exit_then_restart_cycle_recovers_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    const INTO_DANGER: Duration = Duration::from_secs(3 * 60 * 60 + 55 * 60);
    const ADVANCE_PER_RETRY: Duration = blocks_as_duration(RESPAWN_RETRY_ADVANCE_BLOCKS);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Baseline — deposit + transfer while running directly against Anvil,
    // then route through the proxy for the outage.
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut replay_before = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay_before,
            &alice_l1,
            U256::from(600_000_u64),
        )
        .await?;
    }

    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    runtime.respawn().await?;

    let mut ws = runtime.ws(0).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    ws.expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    alice_l2
        .transfer(bob_address, U256::from(100_000_u64))
        .await?;
    replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;
    drop(ws);

    // Mid-run outage: proxy goes down, coupled wall+L1 advance into danger.
    // The running sequencer's submitter tick hits the disconnect, runs
    // wall_clock_danger_estimate, sees past-danger, and exits with
    // `DangerZone`.
    proxy.disconnect();
    runtime.advance_wall_and_mine(INTO_DANGER).await?;

    let exit_status = runtime.wait_for_exit(Duration::from_secs(30)).await?;
    assert!(
        !exit_status.success(),
        "sequencer must self-exit on mid-run DangerZone, got {exit_status:?}",
    );

    // L1 comes back. Run the orchestrator cycle: the aged closed batch
    // eventually ages past `MAX_WAIT_BLOCKS` and startup recovery cascades.
    proxy.reconnect();

    let outcomes = runtime
        .respawn_until_stable(RespawnPolicy {
            max_attempts: 5,
            stabilization: Duration::from_secs(8),
            advance_per_retry: Some(ADVANCE_PER_RETRY),
        })
        .await?;
    assert!(
        matches!(outcomes.last(), Some(RespawnAttemptOutcome::Stable)),
        "restart cycle must converge to Stable, got: {outcomes:?}",
    );

    let counts = runtime.count_batches()?;
    assert!(
        counts.invalidated >= 1,
        "expected cascade after mid-run exit + restart cycle: {counts:?}",
    );

    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        U256::from(600_000_u64),
    );
    assert_eq!(replay_after.current_user_balance(bob_address), U256::ZERO);
    assert_eq!(replay_after.current_user_nonce(alice_address), 0);

    proxy.shutdown().await?;
    Ok(())
}

// §7.8.2 — First-boot-with-L1-down refuses to boot (wall-clock fallback
// treats "never synced" as danger).
//
// `wall_clock_danger_estimate` has a distinguished branch for `last_sync_ms
// == 0`: it refuses to proceed because the sequencer has no baseline to
// measure drift against, so issuing soft confirmations against whatever
// stale safe head we last saw is unsafe. There's a unit test covering that
// branch in isolation; this e2e confirms the full `run()` boot path
// respects it end-to-end.
//
// How we reach the condition: the harness's `spawn()` does a successful
// first boot (needs L1 reachable to deploy contracts and bootstrap the
// chain-id/InputBox cache). We stop, rewrite `l1_safe_head.synced_at_ms`
// to 0 directly, then respawn with the proxy disconnected. The bootstrap
// cache is still populated — so the sequencer gets past the
// contract-discovery phase — but the wall-clock fallback sees the zeroed
// timestamp and returns `StartupDangerZoneEstimate`.
//
// Scope note: a "truly" first-ever boot would fail even earlier (no
// bootstrap cache, can't discover contracts). That's a separate test; this
// one targets only the wall-clock-fallback branch.
async fn run_first_boot_l1_unreachable_never_synced_refuses_boot_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Baseline boot so the bootstrap cache lands on disk.
    {
        let _ws = runtime.ws(0).await?;
    }

    runtime.stop().await?;

    // Simulate "never synced L1" by zeroing the timestamp. The block number
    // stays whatever it already is — the wall-clock fallback keys off
    // `synced_at_ms == 0`, not the block count.
    runtime.reset_l1_safe_head_synced_at_ms()?;

    // Route the sequencer through a disconnected proxy so L1 is unreachable
    // from the sequencer's perspective.
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();

    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "never-synced + L1-unreachable must refuse to boot, got {respawn_result:?}",
    );

    // Confirm the refusal is reversible: reconnect the proxy and the
    // sequencer boots normally (the wall-clock fallback path is gated on
    // L1 unreachability, not on any persistent flag).
    proxy.reconnect();
    runtime.respawn().await?;

    proxy.shutdown().await?;
    Ok(())
}

// §11.1.4 — Past-stale closed+submitted batch (delayed-inclusion cascade).
//
// Scenario: a batch closes and the submitter's L1 tx is never mined (the
// gateway dropped it, mempool evicted it, whatever). Blocks accumulate. On
// the sequencer's next startup recovery, the batch's first frame is >
// `MAX_WAIT_BLOCKS` behind current_safe_block, so the scheduler skips it
// in `populate_safe_accepted_batches` and `find_first_batch_in_danger`
// flags it — cascade fires.
//
// This is the structural sibling of §11.1.3 (open-batch variant) for
// closed+submitted batches. The `find_first_batch_in_danger` path has two
// flavors: "open batch got old" (§11.1.3) and "closed batch submission
// got lost" (this one). Both need to cascade correctly; §11.1.3 had e2e
// coverage, the closed-submitted variant had none.
//
// Setup shape: we use Anvil's `setAutomine(false)` + `dropAllPendingTxs`
// (new T2 harness primitives) to hold the sequencer's batch-submission tx
// out of the chain, then drop it entirely — cleaner than the mempool-hold
// approach, because `anvil_mine(N)` with a pending tx would include it in
// the first mined block, not the Nth. Dropping simulates gateway packet
// loss directly and advances 1250 genuinely empty blocks.
async fn run_delayed_inclusion_cascades_on_restart_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Past-stale: 1250 blocks > MAX_WAIT_BLOCKS (1200).
    const PAST_STALE: Duration = blocks_as_duration(PAST_STALE_BLOCKS);
    // Enough transfers to trigger at least one size-based batch close.
    // Matches §11.2.1's sizing (≈100 B/op × 150 ops ≈ 15 KB > 12 KB target).
    const TRANSFERS_TO_FORCE_BATCH_CLOSE: usize = 150;
    // After the last transfer, wait for the submitter's next tick so it
    // picks up the closed batch and sends the L1 tx to the (now-held)
    // mempool. Default `idle_poll_interval` is 5 s.
    const WAIT_FOR_SUBMITTER_TICK: Duration = Duration::from_secs(7);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Fund Alice generously — 151 transfers + fees is well under 10 M.
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let deposit_amount = U256::from(10_000_000_u64);
    let mut replay_before = ReplayWalletApp::devnet();

    let mut ws = runtime.ws(0).await?;
    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before,
        &alice_l1,
        deposit_amount,
    )
    .await?;

    // Capture the sealed-batch baseline BEFORE we disable auto-mining so
    // we can assert that at least one new batch sealed during the
    // mempool-held phase.
    let batches_before_close = runtime.count_batches()?;

    // Hold the mempool. From here, txs go to Anvil but don't mine until
    // we either re-enable auto-mining or call `anvil_mine`.
    runtime.set_automine(false).await?;

    // Submit enough transfers to trigger at least one size-triggered
    // batch close. Each POST /tx is processed by the sequencer
    // synchronously; the inclusion lane seals a batch when cumulative
    // user-op bytes exceed the target.
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    for _ in 0..TRANSFERS_TO_FORCE_BATCH_CLOSE {
        alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
        replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    // Give the submitter tick time to fire and put the batch-submission
    // tx into the (held) mempool.
    tokio::time::sleep(WAIT_FOR_SUBMITTER_TICK).await;

    let batches_after_close = runtime.count_batches()?;
    assert!(
        batches_after_close.sealed > batches_before_close.sealed,
        "expected at least one new sealed batch: before={batches_before_close:?} after={batches_after_close:?}",
    );

    // Shut the sequencer down, then drop the mempool so the submitted
    // batch tx never lands. The sequencer's DB still shows a sealed
    // batch; L1 has no corresponding event.
    drop(ws);
    runtime.stop().await?;
    runtime.drop_all_pending_txs().await?;

    // Advance past MAX_WAIT_BLOCKS. With auto-mining still off but the
    // mempool empty, these are genuinely empty blocks — nothing to
    // include. `advance_wall_and_mine` also shifts the sequencer's
    // faketime offset so the wall-clock fallback stays in sync with L1.
    runtime.advance_wall_and_mine(PAST_STALE).await?;

    // Re-enable auto-mining before respawn: startup recovery's flush step
    // submits a no-op at the stuck wallet-nonce slot and needs it mined
    // to progress. With auto-mining off, the flusher would hang.
    runtime.set_automine(true).await?;

    runtime.respawn().await?;

    // Verify cascade fired.
    let counts = runtime.count_batches()?;
    assert!(
        counts.invalidated >= 1,
        "expected cascade-invalidation of the delayed-inclusion batch: {counts:?}",
    );

    // Replay from offset 0: the deposit must be re-drained (it's still a
    // safe L1 input), and the sealed batch's transfers must be gone.
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        deposit_amount,
        "Alice must have her full deposit back (all transfers invalidated)",
    );
    assert_eq!(
        replay_after.current_user_balance(bob_address),
        U256::ZERO,
        "Bob's receiving balance must roll back",
    );
    assert_eq!(replay_after.current_user_nonce(alice_address), 0);

    Ok(())
}

// §7.3.5 — Aging Tip while sequencer is UP and L1 is reachable.
//
// Negative control for the danger-check split (see TEST_PLAN §7.3.5 and
// `check_danger_zone_does_not_flag_open_batch_zombie`). The submitter's
// `check_danger_zone` runs every tick and is intentionally **closed-only**:
// its response (shutdown → flush → restart) only makes sense for batches
// that have a pending L1 submission and could zombie on confirm. An open
// Tip has no submission and no zombie risk — flagging it would trigger a
// pointless restart loop.
//
// This test exercises the invariant end-to-end. With L1 reachable and the
// wall clock held (`max_batch_open` not yet elapsed), we advance L1 into
// the danger zone and verify the sequencer keeps running. Only once we
// shift the wall clock past `max_batch_open` — forcing the Tip to close
// naturally — does the submitter's tick rightly fire `DangerZone`.
//
// Staging (decoupled L1/wall-clock advance):
//   1. Baseline: deposit + transfer → Tip at first_frame_safe_block X.
//   2. `mine_l1_blocks(1150)` — current_safe_block jumps 1150 past X
//      (into the danger window, below MAX_WAIT). Wall clock unchanged,
//      so inclusion lane's time-based close doesn't fire and Tip stays
//      open.
//   3. `observe_for(8 s)` — real wall-clock wait that gives the input
//      reader (~2 s poll) time to sync the new safe head and the
//      submitter (~5 s poll) time to tick at least once. Assert the
//      child is still alive: the only way it would exit here is if the
//      zombie check wrongly flagged the open Tip.
//   4. `set_faketime_offset("+2h5m")` — jump the wall clock past
//      `DEFAULT_MAX_BATCH_OPEN` (2 h). Inclusion lane's next iteration
//      closes the Tip. Closed batch's age in L1 blocks is already 1150
//      > `danger_threshold` (1125), so the submitter's next tick exits
//      with `DangerZone`.
//   5. `wait_for_exit` + assert exit status is non-zero and
//      `counts.invalidated == 0` (we never crossed MAX_WAIT_BLOCKS, so
//      no cascade).
//
// If someone accidentally unifies `check_danger_zone` to include open
// batches, step 3's `observe_for` captures a `Some(exit)` and this test
// fails with a clear message. That's the bug class the schema refactor
// was designed to prevent.
async fn run_aging_open_tip_tolerated_by_zombie_check_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Comfortably past `DANGER_THRESHOLD_BLOCKS`, below `MAX_WAIT_BLOCKS`. No
    // cascade expected. Uses module-level `DANGER_ZONE_BLOCKS`.
    // Must exceed `DEFAULT_MAX_BATCH_OPEN` (2 h = 7200 s). 5 min of headroom.
    // Use the `+Ns` format that `advance_wall_and_mine` writes — libfaketime
    // parses it reliably; combined-unit forms like `+2h5m` are unreliable.
    const WALL_CLOCK_PAST_MAX_BATCH_OPEN: &str = "+7500s";
    // Spans at least one submitter `idle_poll_interval` (default 5 s) plus
    // input-reader lag (~2 s) with a safety margin, so we can reliably
    // observe "did not exit" rather than racing the first tick.
    const TOLERATE_WINDOW: Duration = Duration::from_secs(8);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Baseline: one transfer into the open Tip. The Tip's first frame is
    // anchored at the current safe_block; we'll advance L1 past this
    // without closing.
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            U256::from(600_000_u64),
        )
        .await?;
        alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    // L1 jumps into the danger window; wall clock stays put (Tip stays
    // open). `mine_l1_blocks` doesn't touch faketime, so this is a
    // genuinely decoupled advance.
    runtime.mine_l1_blocks(DANGER_ZONE_BLOCKS).await?;

    // Negative control: the submitter's zombie check must NOT fire on
    // the aging open Tip. If it did, `observe_for` returns `Some(exit)`
    // and we fail with a clear message.
    let early_exit = runtime.observe_for(TOLERATE_WINDOW).await?;
    assert!(
        early_exit.is_none(),
        "sequencer must tolerate an aging open Tip while L1 is reachable — \
         zombie check is closed-only; got unexpected exit {early_exit:?}",
    );

    // Trigger the natural close: jump the wall clock past
    // `max_batch_open`. The inclusion lane closes on its next iteration
    // (~10 ms), producing a closed batch already in danger. The
    // submitter's next tick (within ~5 s) sees it and exits.
    runtime.set_faketime_offset(Some(WALL_CLOCK_PAST_MAX_BATCH_OPEN.to_string()))?;

    let exit = runtime.wait_for_exit(Duration::from_secs(15)).await?;
    assert!(
        !exit.success(),
        "sequencer must exit non-zero on submitter `DangerZone` after Tip closes, got {exit:?}",
    );

    // Below MAX_WAIT_BLOCKS: no cascade. The batch is flush-eligible but
    // not invalidated. If anyone changes `run_startup_recovery` to
    // cascade at `danger_threshold` instead of `MAX_WAIT_BLOCKS`, this
    // assertion fails and signals the regression.
    let counts = runtime.count_batches()?;
    assert_eq!(
        counts.invalidated, 0,
        "danger-zone self-exit must not invalidate batches: {counts:?}",
    );

    Ok(())
}

// §7.8.6 — Provider reachable, safe head frozen, live submitter self-exits.
//
// This is the runtime twin of §7.8.5. We first reproduce the existing
// "aging open Tip under reachable L1" negative control: the reader catches up
// to a danger-window safe head and the sequencer stays alive because the
// closed-batch zombie check intentionally ignores the open Tip. We then freeze
// safe-head progress (no more L1 blocks) and jump only the sequencer's wall
// clock forward. The live submitter should notice the missing safe-progress
// timestamp advance and exit with `DangerZone` before the provider itself
// fails.
async fn run_stalled_safe_head_live_exit_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    const STALLED_SAFE_HEAD_OFFSET: &str = "+30s";
    const SAFE_HEAD_SYNC_WINDOW: Duration = Duration::from_secs(8);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            U256::from(600_000_u64),
        )
        .await?;
        alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    runtime.mine_l1_blocks(DANGER_ZONE_BLOCKS).await?;

    let early_exit = runtime.observe_for(SAFE_HEAD_SYNC_WINDOW).await?;
    assert!(
        early_exit.is_none(),
        "the closed-only zombie check must keep tolerating an aging open Tip before the safe head stalls; got unexpected exit {early_exit:?}",
    );

    runtime.set_faketime_offset(Some(STALLED_SAFE_HEAD_OFFSET.to_string()))?;

    let exit = runtime.wait_for_exit(Duration::from_secs(15)).await?;
    assert!(
        !exit.success(),
        "reachable-but-stalled safe head must force a non-zero self-exit before the provider fails, got {exit:?}",
    );

    let counts = runtime.count_batches()?;
    assert_eq!(
        counts.invalidated, 0,
        "live stalled-safe-head shutdown must not cascade batches on its own: {counts:?}",
    );

    Ok(())
}

// §4.4.2 — Reconnect at a previously-observed offset that got invalidated
// after the WS connection dropped.
//
// A WS connection cannot span invalidation: the sequencer necessarily exits
// (DangerZone or stop) before `detect_and_recover` runs, and the socket dies
// with the process. The meaningful invariant is the **reconnect** behavior —
// a client that reconnects at `from_offset=N`, where `N` was an offset it
// previously received and whose row is *now invalidated*, must see the
// cursor skip cleanly past `N` and deliver only post-recovery events.
//
// §4.4.1 covers the adjacent case (`from_offset=0`), which trivially walks
// `valid_sequenced_l2_txs` from the start. This case is distinct because
// the query `WHERE offset > N` is pointed at an offset that no longer
// exists in the valid view.
async fn run_ws_reconnect_at_invalidated_offset_skips_cleanly_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Past-stale: matches `recovery_after_stale_batches_test` sizing.
    const PAST_STALE: Duration = blocks_as_duration(MAX_WAIT_BLOCKS);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay_before = ReplayWalletApp::devnet();

    // Build up offsets 0 (deposit) and 1 (transfer) and capture the
    // transfer's offset so we can later reconnect at it.
    let mut ws = runtime.ws(0).await?;
    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_before,
        &alice_l1,
        U256::from(600_000_u64),
    )
    .await?;
    alice_l2
        .transfer(bob_address, U256::from(100_000_u64))
        .await?;
    let transfer_msg = ws.expect_user_op_from(alice_address).await?;
    let last_seen_offset = transfer_msg.offset();
    replay_before.apply(transfer_msg)?;

    // Kill the WS socket and the sequencer (same way a real reconnect arc
    // works — process dies, client dials back in).
    drop(ws);
    runtime.stop().await?;

    runtime.advance_wall_and_mine(PAST_STALE).await?;
    runtime.respawn().await?;

    // Reconnect at the last offset the client observed — now invalidated.
    // The query `WHERE offset > last_seen_offset` against
    // `valid_sequenced_l2_txs` must skip cleanly past the invalidated
    // rows and deliver only the post-recovery events (the re-drained
    // deposit).
    let mut ws_after = runtime.ws(last_seen_offset).await?;
    let redrained = ws_after
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    // The re-drained deposit's offset is strictly greater than the
    // last-seen offset — if the cursor ever delivered an invalidated row
    // or the same offset again, that'd be the regression.
    assert!(
        redrained.offset() > last_seen_offset,
        "re-drained event must have a strictly-greater offset: \
         last_seen={last_seen_offset}, redrained={}",
        redrained.offset(),
    );
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    // Sanity check: also reconnecting at 0 produces the same single event
    // (§4.4.1's property), to rule out any one-off weirdness in the
    // non-zero reconnect path.
    drop(ws_after);
    let mut ws_from_zero = runtime.ws(0).await?;
    let redrained_from_zero = ws_from_zero
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    assert_eq!(
        redrained.offset(),
        redrained_from_zero.offset(),
        "reconnect-at-invalidated and reconnect-at-zero must deliver the \
         same next valid event",
    );
    ws_from_zero
        .expect_no_message_for(NO_WS_MESSAGE_WAIT)
        .await?;

    Ok(())
}

// §4.1.3 — `from_offset=future` waits silently without erroring.
//
// A subscribe at a far-future offset is a valid subscription that should
// behave the same way `from_offset=0` does on an empty feed: sit idle on
// the live broadcast channel until an event with a greater offset arrives,
// no error, no close.
//
// The behavior is deliberately consistent with `from_offset=0` on an empty
// head — otherwise we'd be making the wait-for-something-new path differ
// based on whether history exists. Test pins this as part of the WS
// subscription contract.
async fn run_ws_subscribe_from_future_offset_waits_silently_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Comfortably beyond any offset this test will produce. `sequenced_l2_txs`
    // is rowid-based; rowid_u64 ≤ a few by the end of the short workload.
    const FUTURE_OFFSET: u64 = 1_000_000;
    // Enough real time to observe "waits silently" without being slow.
    const WAIT_WINDOW: Duration = Duration::from_secs(2);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    // Seed some actual events so we're not testing "empty head, future
    // offset" (trivial case). We want "non-trivial head, offset beyond it".
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            U256::from(500_000_u64),
        )
        .await?;
        alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    }

    // Subscribe far beyond the current head. The subscribe itself must
    // succeed (no 4xx / WS close code), and the resulting stream must be
    // quiet until something with a greater offset arrives.
    let mut ws_future = runtime.ws(FUTURE_OFFSET).await?;
    ws_future.expect_no_message_for(WAIT_WINDOW).await?;

    // Generate more activity. These events are still at offsets far below
    // `FUTURE_OFFSET`, so they must not be delivered — the subscription
    // keeps waiting.
    alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
    alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
    ws_future.expect_no_message_for(WAIT_WINDOW).await?;

    Ok(())
}

// §7.4.2 — Safe direct input that was NOT yet drained before the cascade
// must be drained into the recovery batch's first frame.
//
// Distinct from §7.4.1 (`recovery_after_stale_batches_test`), where the
// direct input was drained into an invalidated batch and gets *re*-drained
// on recovery. Here we exercise the simpler case: the input hit the
// sequencer's view post-stop, so it was never referenced by any frame;
// recovery must include it in the fresh batch's leading range.
//
// Setup:
//   1. Spawn + stop immediately. Initial Tip is empty and anchored at an
//      early safe_block.
//   2. Deposit on L1 directly (sequencer is stopped, so the event isn't
//      consumed yet).
//   3. Advance L1 past MAX_WAIT_BLOCKS to age the empty initial Tip past
//      stale.
//   4. Respawn. Startup recovery syncs the new safe head, sees the
//      deposit in `safe_inputs`, cascades the aged initial Tip, and opens
//      a recovery batch with `leading_range = [next_undrained, end)` —
//      including the undrained deposit.
//   5. WS replay at offset 0 must deliver the deposit event (drained
//      exactly once, into the recovery batch's first frame).
async fn run_recovery_drains_safe_but_undrained_direct_input_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    const PAST_STALE: Duration = blocks_as_duration(PAST_STALE_BLOCKS);

    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;

    // Stop the sequencer before any user-level activity. The initial Tip
    // is empty and anchored at whatever safe_block the lane saw on first
    // boot.
    runtime.stop().await?;

    // Deposit happens entirely on L1 while the sequencer is offline —
    // WalletL1Client dials Anvil directly, not through the sequencer.
    let deposit_amount = U256::from(600_000_u64);
    alice_l1.mint_supported_token(deposit_amount).await?;
    alice_l1.deposit_supported_token(deposit_amount).await?;

    // Advance L1 past MAX_WAIT_BLOCKS + safe-depth so the aged empty
    // initial Tip gets cascaded and the deposit event is safe.
    runtime.advance_wall_and_mine(PAST_STALE).await?;

    runtime.respawn().await?;

    // WS from offset 0. Recovery batch's first frame must contain the
    // deposit (never drained before), and nothing else.
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    let deposit_msg = ws_after
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    replay_after.apply(deposit_msg)?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_eq!(
        replay_after.current_user_balance(alice_address),
        deposit_amount,
        "the deposit, never previously drained, must land in the recovery \
         batch's first frame",
    );

    // Cascade fired on the empty initial Tip.
    let counts = runtime.count_batches()?;
    assert!(
        counts.invalidated >= 1,
        "expected the empty initial Tip to be cascaded: {counts:?}",
    );

    Ok(())
}

// §7.4.3 — Recovery batch opens empty when no direct inputs are pending.
//
// Negative control for §7.4.2: same overall shape but with no L1 deposit
// before respawn. The recovery batch's `leading_range` is `[0, 0)` and the
// batch's first frame is empty. WS replay delivers nothing.
async fn run_recovery_batch_opens_empty_when_no_direct_inputs_pending_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    const PAST_STALE: Duration = blocks_as_duration(PAST_STALE_BLOCKS);

    runtime.stop().await?;

    // No deposits, no user ops. Just age the initial Tip past stale.
    runtime.advance_wall_and_mine(PAST_STALE).await?;

    runtime.respawn().await?;

    // WS from offset 0 must deliver nothing — the recovery batch is empty.
    let mut ws_after = runtime.ws(0).await?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    // Cascade still fired (empty initial Tip past MAX_WAIT).
    let counts = runtime.count_batches()?;
    assert!(
        counts.invalidated >= 1,
        "expected the empty initial Tip to be cascaded even without direct \
         inputs: {counts:?}",
    );

    Ok(())
}

// §10.1.1 — Replay determinism: for any workload accepted live, catch-up
// replay must produce an identical per-user state.
//
// This is the `Application` trait's fundamental contract (see
// `AGENTS.md` §Application-Trait-Contract). Without it, restart
// replay and WS catch-up aren't equivalent to live execution — the
// whole soft-confirmation model collapses.
//
// `restart_and_replay_test` covers a single-user two-op workload; this
// test uses a deliberately diverse multi-user, multi-op mix (three
// senders, deposits interleaved with transfers and withdrawals) and
// asserts a *direct* equality between the live replay (assembled from
// WS events observed during execution) and the post-restart replay
// (assembled from WS catch-up at offset 0). Any per-user balance or
// nonce divergence would signal a non-deterministic application or a
// catch-up bug.
async fn run_replay_matches_live_for_mixed_workload_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let charlie = TestSigner::from_default(3)?;
    let alice_address = alice.address();
    let bob_address = bob.address();
    let charlie_address = charlie.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let charlie_l1 = runtime.wallet_l1(charlie.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut bob_l2 = runtime.wallet_l2(bob)?;
    let mut charlie_l2 = runtime.wallet_l2(charlie)?;

    let mut ws = runtime.ws(0).await?;
    let mut replay_live = ReplayWalletApp::devnet();

    // Diverse workload — exercises deposit-interleaving and every op
    // combination supported by the wallet app.
    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_live,
        &alice_l1,
        U256::from(1_000_000_u64),
    )
    .await?;
    alice_l2
        .transfer(bob_address, U256::from(400_000_u64))
        .await?;
    replay_live.apply(ws.expect_user_op_from(alice_address).await?)?;

    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay_live,
        &charlie_l1,
        U256::from(500_000_u64),
    )
    .await?;
    bob_l2
        .transfer(charlie_address, U256::from(150_000_u64))
        .await?;
    replay_live.apply(ws.expect_user_op_from(bob_address).await?)?;

    charlie_l2.withdraw(U256::from(100_000_u64)).await?;
    replay_live.apply(ws.expect_user_op_from(charlie_address).await?)?;

    alice_l2
        .transfer(charlie_address, U256::from(50_000_u64))
        .await?;
    replay_live.apply(ws.expect_user_op_from(alice_address).await?)?;

    bob_l2.withdraw(U256::from(50_000_u64)).await?;
    replay_live.apply(ws.expect_user_op_from(bob_address).await?)?;

    let expected_input_count = replay_live.executed_input_count();

    // Restart + catch-up replay. Each WS catch-up event feeds the fresh
    // replay identically to how the live stream fed the original; if the
    // application is deterministic, the two replays must be bit-identical
    // across every per-user view the replay exposes.
    drop(ws);
    runtime.restart().await?;
    let mut ws_after = runtime.ws(0).await?;
    let mut replay_post = ReplayWalletApp::devnet();

    // Two deposits + five user ops = seven events.
    for _ in 0..expected_input_count {
        replay_post.apply(ws_after.next_message().await?)?;
    }
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    for addr in [alice_address, bob_address, charlie_address] {
        assert_eq!(
            replay_post.current_user_balance(addr),
            replay_live.current_user_balance(addr),
            "balance divergence for {addr:?}: live vs. replay must match",
        );
        assert_eq!(
            replay_post.current_user_nonce(addr),
            replay_live.current_user_nonce(addr),
            "nonce divergence for {addr:?}: live vs. replay must match",
        );
    }
    assert_eq!(
        replay_post.executed_input_count(),
        replay_live.executed_input_count(),
    );

    Ok(())
}

// §5.4.1 / §5.4.2 — Transient provider outage: the L1 input reader must
// retry on provider errors (connection refused, timeout) without
// crashing, and pick up the backlog on reconnect.
//
// Distinct from §11.4.1 (short hiccup under load), which tests the batch
// submitter's retry path via POST activity. Here the interesting
// component is the **input reader**: its only job is polling L1 for new
// events, so the only observable signal that its retry loop works is
// whether a deposit made *during the disconnect* (and thus invisible
// until the proxy comes back) lands on the WS feed after reconnect.
//
// Scenario:
//   1. Route the sequencer through the proxy.
//   2. Disconnect proxy. Alice deposits on L1 (via `WalletL1Client`,
//      which dials Anvil directly — bypassing the proxy).
//   3. Advance a few L1 blocks to push the deposit past safe depth. The
//      sequencer's reader keeps failing to fetch (connection refused
//      from the disconnected proxy) and retrying.
//   4. Reconnect proxy. The reader's next poll succeeds; backlog is
//      pulled in; the WS subscriber (still connected) receives the
//      deposit event.
//   5. Assert the sequencer didn't crash (no respawn needed, still same
//      child) and the deposit landed.
async fn run_provider_outage_input_reader_retries_after_reconnect_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Well below any stale threshold — we just need safe-depth headroom.
    const SAFE_DEPTH_HEADROOM_BLOCKS: u64 = 20;

    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();

    // Route through the proxy (stop → override → respawn).
    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    runtime.respawn().await?;

    let alice_l1 = runtime.wallet_l1(alice).await?;
    let mut ws = runtime.ws(0).await?;
    let mut replay = ReplayWalletApp::devnet();

    // Baseline deposit with the proxy connected — proves the WS + reader
    // path works end-to-end before we break it.
    apply_safe_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        U256::from(300_000_u64),
    )
    .await?;

    // Proxy down. The sequencer's reader polls on a ~2 s cadence; each
    // poll will fail with a connection-refused-style provider error until
    // we reconnect.
    proxy.disconnect();

    // Deposit while the proxy is down. The L1 wallet bypasses the proxy,
    // so Anvil sees the deposit but the sequencer can't.
    let late_amount = U256::from(400_000_u64);
    alice_l1.mint_supported_token(late_amount).await?;
    alice_l1.deposit_supported_token(late_amount).await?;
    runtime.mine_l1_blocks(SAFE_DEPTH_HEADROOM_BLOCKS).await?;

    // During the disconnect, the reader should keep retrying rather than
    // crashing. Assert the sequencer stays up for a few real seconds
    // (long enough for multiple reader polls to fail + retry).
    let early_exit = runtime.observe_for(Duration::from_secs(5)).await?;
    assert!(
        early_exit.is_none(),
        "input reader must retry provider errors, not crash the process: \
         got unexpected exit {early_exit:?}",
    );

    // Reconnect. The reader's next poll succeeds, picks up the backlog,
    // WS subscriber receives the event.
    proxy.reconnect();

    let late_deposit_msg = ws
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    replay.apply(late_deposit_msg)?;

    assert_eq!(
        replay.current_user_balance(alice_address),
        U256::from(700_000_u64),
        "both deposits should be reflected after reader catches up",
    );

    proxy.shutdown().await?;
    Ok(())
}

// §8.1.2 — First-ever boot with empty bootstrap cache + L1 unreachable
// returns a fixed `RunError::Io("L1 unreachable and no bootstrap cache")`.
//
// Distinct from §7.8.2 (already covered): that test exercises the
// wall-clock fallback inside `run_preemptive_recovery`, which only fires
// AFTER bootstrap discovery has succeeded once (so the cache is
// populated). §8.1.2 targets the EARLIER failure — the
// `InputReader::new` discovery step where the sequencer asks L1 for the
// InputBox address + chain id. With nothing cached, that call has no
// fallback and the boot fails before recovery logic runs.
//
// The harness simulates "no cache" by `clear_l1_bootstrap_cache()` after
// a normal boot has populated it (truly first-ever boot would also lack
// a chain-id cache, but the failure mode is identical: the bootstrap
// step has nothing to fall back to).
async fn run_first_boot_no_cache_l1_unreachable_refuses_boot_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Baseline boot to ensure the schema is fully migrated. We then
    // clear the cache to mimic a first-ever boot.
    {
        let _ws = runtime.ws(0).await?;
    }
    runtime.stop().await?;
    runtime.clear_l1_bootstrap_cache()?;

    // Route through a disconnected proxy so InputReader::new fails with
    // a provider error.
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();

    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "first boot with no cache + L1 unreachable must refuse boot, got {respawn_result:?}",
    );

    // Verify reversibility: reconnect proxy, respawn, this time the
    // bootstrap step succeeds and populates the cache.
    proxy.reconnect();
    runtime.respawn().await?;

    proxy.shutdown().await?;
    Ok(())
}

// §8.2.1 / §8.3.1 — Chain-id mismatch via the live RPC path.
//
// Companion to `chain_id_mismatch_from_cache_returns_typed_error` in
// `sequencer/tests/chain_id_validation.rs`. The cache-path test runs
// in-process against a stub; this test runs the full sequencer binary
// against real Anvil with a deliberately mismatched `--chain-id`,
// proving the RPC-comparison path returns
// `RunError::ChainIdMismatch` *before* writing the wrong-chain
// bootstrap cache.
//
// The pre-write ordering matters: a regression that swapped the
// cache-write and the chain-id check would leave a bad cache row on
// disk, poisoning future startups. Asserting `respawn_result.is_err()`
// alone catches the bad-error case; we additionally verify a
// post-correction respawn succeeds, which only happens if the cache
// wasn't poisoned (bootstrap reads the L1 chain-id again, sees it
// matches, writes the correct cache).
async fn run_chain_id_mismatch_via_live_rpc_refuses_boot_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Anvil runs at `DEVNET_CHAIN_ID = 31337`. Pick something obviously
    // different that's still valid (chain_id > 0).
    const WRONG_CHAIN_ID: u64 = 99_999;

    // Initial boot completes normally (no override). This populates the
    // cache with the correct chain id.
    {
        let _ws = runtime.ws(0).await?;
    }
    runtime.stop().await?;

    // Clear the cache so the live RPC path runs (otherwise the cache
    // path would catch the mismatch first).
    runtime.clear_l1_bootstrap_cache()?;

    // Configure a mismatched chain id and respawn. The bootstrap-time
    // RPC check returns the actual chain id (31337), compares it with
    // the configured `--chain-id` (99999), and returns ChainIdMismatch.
    runtime.set_chain_id_override(Some(WRONG_CHAIN_ID));
    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "chain-id mismatch via live RPC must refuse boot, got {respawn_result:?}",
    );

    // Reset to the correct chain id. Respawn must succeed — proves the
    // failed attempt didn't poison the cache or other DB state.
    runtime.set_chain_id_override(None);
    runtime.respawn().await?;

    Ok(())
}

// §7.5.1 / §7.5.2 — Nonce-0 first batch recovery edge.
//
// Two coupled invariants:
//   - §7.5.1: If the FIRST-EVER batch (nonce 0) goes stale before any
//     batch reaches `Gold` (i.e., before any batch is L1-accepted),
//     recovery cascades it and opens a fresh recovery batch that itself
//     has nonce 0 (parent NULL — there's no valid ancestor to point
//     at). No genesis sentinel exists in the implementation; the
//     parent-pointer schema must handle "all batches invalidated"
//     natively.
//   - §7.5.2: After §7.5.1, the recovery batch (with nonce 0 reused)
//     submits to L1, gets accepted by `populate_safe_accepted_batches`,
//     and lands in `safe_accepted_batches` — proving the scheduler-
//     simulation cursor handles a reused nonce after cascade correctly.
//
// The structural invariants in §7.5.1 are validated by
// `assert_schema_invariants` (post-test hook in `tests/e2e/src/main.rs`):
// it checks that NULL-parent batches have nonce 0 and that valid-path
// nonces form a contiguous `0..N`. So this test asserts those
// observable consequences plus the explicit `safe_accepted_batches`
// post-condition for §7.5.2.
//
// Setup uses T2 (auto-mining off + drop) so the first batch's L1
// submission is dropped before reaching the chain — guaranteeing it
// never reaches `Gold` before being cascaded.
async fn run_nonce_zero_recovery_invalidates_then_accepts_at_nonce_zero_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Past stale to ensure the cascade fires.
    const PAST_STALE: Duration = blocks_as_duration(PAST_STALE_BLOCKS);
    // Force a size-triggered batch close. Same sizing as §11.1.4.
    const TRANSFERS_TO_FORCE_CLOSE: usize = 150;
    // Submitter idle_poll_interval = 5 s; allow one tick for the batch
    // to enter the (held) mempool.
    const WAIT_FOR_SUBMITTER_TICK: Duration = Duration::from_secs(7);

    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;

    // Fund Alice and queue many transfers into the open batch (which is
    // the FIRST EVER batch — nonce 0). Using auto-mining-off across the
    // submitter's tick so the batch's L1 tx hits the mempool but never
    // mines, then dropping it.
    let mut replay_before = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
        apply_safe_supported_deposit(
            runtime,
            &mut ws,
            &mut replay_before,
            &alice_l1,
            U256::from(10_000_000_u64),
        )
        .await?;

        runtime.set_automine(false).await?;

        for _ in 0..TRANSFERS_TO_FORCE_CLOSE {
            alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
            replay_before.apply(ws.expect_user_op_from(alice_address).await?)?;
        }

        // Let the submitter tick fire and put the (nonce-0) batch's L1
        // tx into the held mempool.
        tokio::time::sleep(WAIT_FOR_SUBMITTER_TICK).await;
    }

    runtime.stop().await?;
    runtime.drop_all_pending_txs().await?;

    runtime.advance_wall_and_mine(PAST_STALE).await?;
    runtime.set_automine(true).await?;

    runtime.respawn().await?;

    // §7.5.1 assertions: the only existing batch (the original nonce-0
    // one) was cascaded, and a recovery batch was opened. The recovery
    // batch's invariants (NULL parent → nonce 0) are checked structurally
    // by the post-test `assert_schema_invariants` hook.
    let counts = runtime.count_batches()?;
    assert!(
        counts.invalidated >= 1,
        "expected the original nonce-0 batch to be invalidated: {counts:?}",
    );
    assert!(
        counts.total > counts.invalidated,
        "recovery batch must exist alongside the invalidated original: {counts:?}",
    );

    // Replay shows the deposit re-drained, transfers gone (rolled back).
    // Recreate WS + wallet against the post-respawn HTTP endpoint
    // (`runtime.endpoint()` rebinds to a fresh port on every respawn).
    let mut ws_after = runtime.ws(0).await?;
    let mut alice_l2_fresh = runtime.wallet_l2(alice)?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    assert_eq!(
        replay_after.current_user_balance(alice_address),
        U256::from(10_000_000_u64),
        "Alice must have her full deposit back after nonce-0 cascade",
    );
    assert_eq!(replay_after.current_user_balance(bob_address), U256::ZERO,);
    assert_eq!(replay_after.current_user_nonce(alice_address), 0);

    // §7.5.2 — drive enough work into the recovery batch that the
    // submitter closes it by size and submits to L1. With auto-mining
    // back on, the submission lands and the input reader picks it up
    // into `safe_inputs`; `populate_safe_accepted_batches` accepts it
    // at the expected nonce (0, reused).
    for _ in 0..TRANSFERS_TO_FORCE_CLOSE {
        alice_l2_fresh
            .transfer(bob_address, U256::from(1_u64))
            .await?;
        replay_after.apply(ws_after.expect_user_op_from(alice_address).await?)?;
    }

    // Wait for the submitter to fire a tick + submit the batch. Anvil's
    // instamine puts the submission at 1 confirmation; the submitter's
    // `wait_for_confirmations` needs `confirmation_depth + 1 = 3`. We
    // explicitly mine the remaining 2 blocks below to unblock it without
    // having to wait the full 72s timeout.
    tokio::time::sleep(Duration::from_secs(7)).await;
    runtime.mine_l1_blocks(2).await?;

    // After confirmations land, the submitter's tick loop continues:
    // next iteration runs `refresh_recovery_metadata` →
    // `populate_safe_accepted_batches_inner`, which appends the batch
    // to `safe_accepted_batches` at its expected nonce (0, reused).
    tokio::time::sleep(Duration::from_secs(10)).await;

    let (accepted_count, min_accepted_nonce) = runtime.count_safe_accepted_batches()?;
    assert!(
        accepted_count >= 1,
        "expected at least one batch to land in safe_accepted_batches \
         post-recovery (proves §7.5.2 reused-nonce-0 was accepted): \
         count={accepted_count}",
    );
    assert_eq!(
        min_accepted_nonce,
        Some(0),
        "the first L1-accepted batch must have nonce 0 (reused after \
         cascade) — got {min_accepted_nonce:?}",
    );

    Ok(())
}

fn eip712_domain(runtime: &ManagedSequencer) -> alloy_sol_types::Eip712Domain {
    sequencer_core::build_input_domain(runtime.domain_chain_id(), runtime.verifying_contract())
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
