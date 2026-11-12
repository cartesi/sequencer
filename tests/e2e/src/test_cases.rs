// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use crate::{Host, Scenario, ScenarioFn, ScenarioResult};
use alloy_primitives::{Address, U256};
use rollups_harness::{
    DEVNET_SECONDS_PER_BLOCK, EXIT_TERMINAL, LandingHealth, ManagedSequencer,
    ManagedSequencerConfig, ReplayWalletApp, RunStart, TcpProxy, TestSigner, UnacceptedBatch,
    WalletL1Client, WalletL2Client, WsClient, batch_submitter_signer, encode_batch,
    is_terminal_abort, sign_user_op_hex,
};
use sequencer_core::api::{TxRequest, WsTxMessage};
use sequencer_core::batch::Batch;
use sequencer_core::fee::fee_to_linear;
use sequencer_core::user_op::UserOp;
use sequencer_rust_client::SequencerClient;

pub(crate) const NO_WS_MESSAGE_WAIT: Duration = Duration::from_secs(1);

/// Default log_recommended_fee exponent (0 + 296 + 20 + 419 + 621 = 1356).
const DEFAULT_FRAME_FEE: u16 = 1356;

/// Max fee used for raw TxRequest construction. Must be >= DEFAULT_FRAME_FEE.
const DEFAULT_MAX_FEE: u16 = 2500;

/// Self-transfers that force the inclusion lane to seal a batch by size, so a
/// finalized "gold" snapshot promotes for a watchdog compare — independent of
/// the batch-open timer.
pub(crate) const TRANSFERS_TO_FORCE_BATCH_CLOSE: usize = 150;

/// Poll budget for [`mine_until_finalized_advances`]: mine + recheck the
/// finalized snapshot up to this many times before timing out. With
/// [`PROMOTION_POLL_INTERVAL`] this bounds the wait at ~40s, generous against
/// CI scheduling jitter while never sleeping longer than the condition needs.
const PROMOTION_POLL_ATTEMPTS: usize = 40;

/// Per-attempt pause in [`mine_until_finalized_advances`], giving the submitter
/// tick and the promoter room to run between mines.
const PROMOTION_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Poll budget for waiting until a landed batch reaches the reader's
/// accepted frontier (`batches.accepted_input`).
const ACCEPTANCE_POLL_ATTEMPTS: usize = 40;

/// Per-attempt pause while waiting for batch submission and safe-head ingestion.
const ACCEPTANCE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Product-visible live frame-clock interval. E2E advancement must derive from
/// the protocol constant rather than incidental Anvil transaction counts.
const FRAME_CLOCK_INTERVAL_SAFE_BLOCKS: u64 =
    sequencer_core::protocol::FRAME_CLOCK_INTERVAL_SAFE_BLOCKS;
const FRAME_CLOCK_POLL_ATTEMPTS: usize = 80;
const FRAME_CLOCK_POLL_INTERVAL: Duration = Duration::from_millis(250);

// ── The window ───────────────────────────────────────────────────────────
//
// Every outage drill counts blocks from the wallet's staleness window, which
// its guest image and its sequencer binary both read, so a change to the
// window flows through here.

const MAX_WAIT_BLOCKS: u64 = app_core::application::WALLET_SCHEDULER_LIMITS.max_wait_blocks;

/// Blocks a drill leaves between the safe head and the void boundary when its
/// oldest batch must still land: the lane's seal, the poster's tick, the
/// inclusion and Anvil's safe-head lag fit in a few.
const LANDING_ROOM_BLOCKS: u64 = 50;

/// The lane's default batch-open deadline (two hours), in harness blocks.
const BATCH_OPEN_DEADLINE_BLOCKS: u64 = 2 * 60 * 60 / DEVNET_SECONDS_PER_BLOCK;

/// A short outage: well inside the window, and under the lane's batch-open
/// deadline, so the open batch stays open.
const SHORT_OUTAGE_BLOCKS: u64 = if MAX_WAIT_BLOCKS / 3 < BATCH_OPEN_DEADLINE_BLOCKS - 1 {
    MAX_WAIT_BLOCKS / 3
} else {
    BATCH_OPEN_DEADLINE_BLOCKS - 1
};

const _: () = assert!(LANDING_ROOM_BLOCKS < MAX_WAIT_BLOCKS);

/// Poll budget for a `/healthz` reading; the landing watch publishes every 2 s.
const LANDING_POLL_ATTEMPTS: usize = 40;
const LANDING_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long a running sequencer may take to stop on a terminal fact: the
/// reader's poll plus the landing watch's, with slack.
const TERMINAL_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Longer than the reader's and the landing watch's 2 s polls: a process alive
/// after it has seen what they read.
const STABILITY_WINDOW: Duration = Duration::from_secs(5);

/// `run`'s retry class: restart with backoff.
const EXIT_RESTART_TRANSIENT: i32 = 20;

/// Convert a block count to wall-clock duration at the harness block time.
const fn blocks_as_duration(blocks: u64) -> Duration {
    Duration::from_secs(blocks * DEVNET_SECONDS_PER_BLOCK)
}

struct ExpectedWalletState {
    address: Address,
    balance: U256,
    nonce: u32,
}

fn rust(name: &'static str, run: ScenarioFn) -> Scenario {
    Scenario {
        name,
        host: Host::Rust,
        run,
        configure: no_overrides,
    }
}

fn c_host(name: &'static str, run: ScenarioFn) -> Scenario {
    Scenario {
        name,
        host: Host::C,
        run,
        configure: no_overrides,
    }
}

fn no_overrides(_: &mut ManagedSequencerConfig) {}

/// Wall-clock-neutral scenarios run without libfaketime.
fn real_clock(config: &mut ManagedSequencerConfig) {
    config.faketime = false;
}

/// Recommended fee 1456, under the wallet client's max fee (2500), so
/// transfers still admit.
fn fixed_fee_oracle(config: &mut ManagedSequencerConfig) {
    config.fee_oracle_fixed_log_gas_price = Some(FIXED_LOG_GAS_PRICE);
}

const FIXED_LOG_GAS_PRICE: i16 = 100;

pub fn test_cases() -> Vec<Scenario> {
    vec![
        c_host(
            "c_host_cold_replica_snapshot_backlog_live_restart_test",
            |runtime| Box::pin(crate::cold_replica::run_c(runtime)),
        ),
        c_host("c_host_deposit_transfer_withdrawal_test", |runtime| {
            Box::pin(run_deposit_transfer_withdrawal_test(runtime))
        })
        .with(real_clock),
        c_host("c_host_restart_and_replay_test", |runtime| {
            Box::pin(run_restart_and_replay_test(runtime))
        }),
        rust(
            "cold_replica_snapshot_backlog_live_restart_test",
            |runtime| Box::pin(crate::cold_replica::run(runtime)),
        ),
        rust("deposit_transfer_withdrawal_test", |runtime| {
            Box::pin(run_deposit_transfer_withdrawal_test(runtime))
        })
        .with(real_clock),
        rust("direct_input_not_safe_yet_test", |runtime| {
            Box::pin(run_direct_input_not_safe_yet_test(runtime))
        }),
        rust("rejected_user_op_not_broadcast_test", |runtime| {
            Box::pin(run_rejected_user_op_not_broadcast_test(runtime))
        }),
        rust("reconnect_from_offset_test", |runtime| {
            Box::pin(run_reconnect_from_offset_test(runtime))
        }),
        rust("restart_and_replay_test", |runtime| {
            Box::pin(run_restart_and_replay_test(runtime))
        }),
        rust("unsupported_token_deposit_noop_test", |runtime| {
            Box::pin(run_unsupported_token_deposit_noop_test(runtime))
        }),
        rust("fee_below_minimum_rejected_test", |runtime| {
            Box::pin(run_fee_below_minimum_rejected_test(runtime))
        }),
        rust("fixed_fee_oracle_sets_frame_fee_test", |runtime| {
            Box::pin(run_fixed_fee_oracle_sets_frame_fee_test(runtime))
        })
        .with(fixed_fee_oracle),
        rust("forged_signature_rejected_test", |runtime| {
            Box::pin(run_forged_signature_rejected_test(runtime))
        }),
        rust("concurrent_user_ops_test", |runtime| {
            Box::pin(run_concurrent_user_ops_test(runtime))
        }),
        rust("multi_deposit_reconciliation_test", |runtime| {
            Box::pin(run_multi_deposit_reconciliation_test(runtime))
        }),
        rust(
            "restart_after_committed_tx_replays_cleanly_test",
            |runtime| Box::pin(run_restart_after_committed_tx_replays_cleanly_test(runtime)),
        ),
        // Batch landing and outages (docs/recovery/README.md, "Scenarios").
        rust(
            "restart_after_a_short_outage_keeps_history_test",
            |runtime| Box::pin(run_restart_after_a_short_outage_keeps_history_test(runtime)),
        ),
        rust(
            "restart_inside_the_window_keeps_history_then_lands_test",
            |runtime| {
                Box::pin(run_restart_inside_the_window_keeps_history_then_lands_test(
                    runtime,
                ))
            },
        ),
        rust(
            "dropped_submission_is_posted_again_after_restart_test",
            |runtime| {
                Box::pin(run_dropped_submission_is_posted_again_after_restart_test(
                    runtime,
                ))
            },
        ),
        rust(
            "batches_stop_landing_inside_the_window_then_land_test",
            |runtime| {
                Box::pin(run_batches_stop_landing_inside_the_window_then_land_test(
                    runtime,
                ))
            },
        ),
        rust(
            "void_history_while_running_stops_for_good_test",
            |runtime| Box::pin(run_void_history_while_running_stops_for_good_test(runtime)),
        ),
        rust(
            "blind_past_the_window_stops_at_the_first_sync_that_shows_it_test",
            |runtime| {
                Box::pin(
                    run_blind_past_the_window_stops_at_the_first_sync_that_shows_it_test(runtime),
                )
            },
        ),
        rust(
            "blind_start_boots_on_the_stored_view_then_stops_at_the_first_sync_test",
            |runtime| {
                Box::pin(
                    run_blind_start_boots_on_the_stored_view_then_stops_at_the_first_sync_test(
                        runtime,
                    ),
                )
            },
        ),
        rust(
            "stopped_past_the_window_refuses_every_start_test",
            |runtime| {
                Box::pin(run_stopped_past_the_window_refuses_every_start_test(
                    runtime,
                ))
            },
        ),
        rust(
            "stopped_past_the_window_with_nothing_confirmed_refuses_test",
            |runtime| {
                Box::pin(run_stopped_past_the_window_with_nothing_confirmed_refuses_test(runtime))
            },
        ),
        rust(
            "foreign_batch_at_our_nonce_stops_for_good_test",
            |runtime| Box::pin(run_foreign_batch_at_our_nonce_stops_for_good_test(runtime)),
        ),
        rust("first_run_of_an_era_with_l1_down_retries_test", |runtime| {
            Box::pin(run_first_run_of_an_era_with_l1_down_retries_test(runtime))
        }),
        rust("warm_restart_with_l1_down_test", |runtime| {
            Box::pin(run_warm_restart_with_l1_down_test(runtime))
        }),
        rust(
            "provider_outage_inside_the_window_sequencer_continues_test",
            |runtime| {
                Box::pin(run_provider_outage_inside_the_window_sequencer_continues_test(runtime))
            },
        ),
        rust("provider_outage_short_hiccup_test", |runtime| {
            Box::pin(run_provider_outage_short_hiccup_test(runtime))
        }),
        rust("ws_subscribe_ahead_of_head_is_refused_test", |runtime| {
            Box::pin(run_ws_subscribe_ahead_of_head_is_refused_test(runtime))
        }),
        rust("replay_matches_live_for_mixed_workload_test", |runtime| {
            Box::pin(run_replay_matches_live_for_mixed_workload_test(runtime))
        }),
        rust(
            "provider_outage_input_reader_retries_after_reconnect_test",
            |runtime| {
                Box::pin(run_provider_outage_input_reader_retries_after_reconnect_test(runtime))
            },
        ),
        rust(
            "first_boot_no_identity_l1_unreachable_refuses_boot_test",
            |runtime| {
                Box::pin(run_first_boot_no_identity_l1_unreachable_refuses_boot_test(
                    runtime,
                ))
            },
        ),
        rust(
            "chain_id_mismatch_via_live_rpc_refuses_boot_test",
            |runtime| {
                Box::pin(run_chain_id_mismatch_via_live_rpc_refuses_boot_test(
                    runtime,
                ))
            },
        ),
        rust("watchdog_genesis_compare_test", |runtime| {
            Box::pin(crate::watchdog_compare::run_watchdog_genesis_compare_test(
                runtime,
            ))
        })
        .with(real_clock),
        rust("watchdog_divergence_drill_test", |runtime| {
            Box::pin(run_watchdog_divergence_drill_test(runtime))
        })
        .with(real_clock),
        rust("setup_refuses_a_set_up_data_directory_test", |runtime| {
            Box::pin(
                crate::recovery_round_trip::run_setup_refuses_a_set_up_data_directory_test(runtime),
            )
        }),
        c_host(
            "c_host_setup_refuses_a_set_up_data_directory_test",
            |runtime| {
                Box::pin(
                    crate::recovery_round_trip::run_setup_refuses_a_set_up_data_directory_test(
                        runtime,
                    ),
                )
            },
        ),
        rust("recovery_from_genesis_round_trip_test", |runtime| {
            Box::pin(crate::recovery_round_trip::run_recovery_from_genesis_round_trip_test(runtime))
        })
        .with(real_clock),
        // The rebuild's execute pass, the baseline dump and the feed of a
        // rebuilt era, through the C engine.
        c_host("c_host_recovery_from_genesis_round_trip_test", |runtime| {
            Box::pin(crate::recovery_round_trip::run_recovery_from_genesis_round_trip_test(runtime))
        })
        .with(real_clock),
        rust("recovery_from_stored_machine_round_trip_test", |runtime| {
            Box::pin(
                crate::recovery_round_trip::run_recovery_from_stored_machine_round_trip_test(
                    runtime,
                ),
            )
        })
        .with(real_clock),
        // The base state taken out of a stored machine, opened by the C engine.
        c_host(
            "c_host_recovery_from_stored_machine_round_trip_test",
            |runtime| {
                Box::pin(
                    crate::recovery_round_trip::run_recovery_from_stored_machine_round_trip_test(
                        runtime,
                    ),
                )
            },
        )
        .with(real_clock),
    ]
}

async fn run_deposit_transfer_withdrawal_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    prepare_non_genesis_watchdog_state(runtime).await?;
    crate::watchdog_compare::run_watchdog_non_genesis_compare_test(runtime).await?;
    Ok(())
}

async fn run_watchdog_divergence_drill_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    prepare_non_genesis_watchdog_state(runtime).await?;
    crate::watchdog_compare::run_watchdog_divergence_drill_test(runtime).await?;
    Ok(())
}

async fn prepare_non_genesis_watchdog_state(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut bob_l2 = runtime.wallet_l2(bob)?;
    let mut replay = ReplayWalletApp::devnet();

    // Large deposit leaves headroom for the post-scenario batch-close pump
    // (~150 user ops) that drives a gold finalized snapshot for watchdog compare.
    let deposit_amount = U256::from(10_000_000_u64);
    let transfer_amount = U256::from(400_000_u64);
    let withdrawal_amount = U256::from(150_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
        .await?;

    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;

    bob_l2.withdraw(withdrawal_amount).await?;
    replay.apply(ws.expect_user_op_from(bob_address).await?)?;

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

    drive_finalized_gold_batch_for_watchdog(
        runtime,
        &mut ws,
        &mut replay,
        &mut alice_l2,
        alice_address,
    )
    .await?;
    Ok(())
}

/// Mine L1 forward until a finalized snapshot is promoted at an inclusion block
/// strictly above `floor` (the value observed before the batch that should
/// promote), polling the DB instead of sleeping a fixed submitter-tick
/// interval. Each attempt mines one live block and pauses for the same wall
/// time, so the submitter and promoter get to run without synthesizing a future
/// L1 clock; returns the new inclusion block, or times out loudly. Robust
/// against CI jitter — it waits exactly as long as promotion takes, no more.
pub(crate) async fn mine_until_finalized_advances(
    runtime: &ManagedSequencer,
    floor: u64,
) -> ScenarioResult<u64> {
    for _ in 0..PROMOTION_POLL_ATTEMPTS {
        runtime.mine_l1_block_at_wall_clock().await?;
        tokio::time::sleep(PROMOTION_POLL_INTERVAL).await;
        if let Some(inclusion_block) = runtime.finalized_inclusion_block().await?
            && inclusion_block > floor
        {
            return Ok(inclusion_block);
        }
    }
    Err(
        format!("timed out waiting for finalized snapshot to advance past inclusion_block {floor}")
            .into(),
    )
}

/// Advance Anvil only as far as the next semantic live-frame tick needs, then
/// wait for the input reader and inclusion lane to commit a frame covering
/// `required_safe_block`. The live safe-head query prevents stale SQLite
/// observations from turning an asynchronous reader poll into over-mining.
pub(crate) async fn advance_live_frame_until_covers(
    runtime: &ManagedSequencer,
    required_safe_block: u64,
) -> ScenarioResult<u64> {
    for _ in 0..FRAME_CLOCK_POLL_ATTEMPTS {
        let (frame_safe_block, _) = runtime.frame_clock_observation()?;
        if frame_safe_block >= required_safe_block {
            return Ok(frame_safe_block);
        }

        let next_tick = frame_safe_block
            .checked_add(FRAME_CLOCK_INTERVAL_SAFE_BLOCKS)
            .ok_or("frame-clock target overflow")?;
        let target_safe_head = next_tick.max(required_safe_block);
        let live_safe_head = runtime.l1_safe_block_number().await?;
        if live_safe_head < target_safe_head {
            runtime
                .mine_live_l1_blocks(target_safe_head - live_safe_head)
                .await?;
        }

        tokio::time::sleep(FRAME_CLOCK_POLL_INTERVAL).await;
    }

    let (frame_safe_block, persisted_safe_head) = runtime.frame_clock_observation()?;
    let live_safe_head = runtime.l1_safe_block_number().await?;
    Err(format!(
        "timed out advancing live frame to cover block {required_safe_block}: \
         frame={frame_safe_block} persisted_safe_head={persisted_safe_head} \
         live_safe_head={live_safe_head}"
    )
    .into())
}

/// Reach one exact Anvil safe head and wait for the input reader to persist it.
/// Used only by the frame-clock boundary test, where overshooting would erase
/// the below-threshold assertion.
async fn advance_persisted_safe_head_exactly(
    runtime: &ManagedSequencer,
    target_safe_head: u64,
) -> ScenarioResult<()> {
    for _ in 0..FRAME_CLOCK_POLL_ATTEMPTS {
        let live_safe_head = runtime.l1_safe_block_number().await?;
        if live_safe_head > target_safe_head {
            return Err(format!(
                "cannot pin safe head {target_safe_head}: live Anvil safe head is already {live_safe_head}"
            )
            .into());
        }
        if live_safe_head < target_safe_head {
            // Mine one at a time: Anvil's safe tag may lag the chain head, so
            // requested block count and safe-head delta are not interchangeable.
            runtime.mine_live_l1_blocks(1).await?;
            tokio::time::sleep(FRAME_CLOCK_POLL_INTERVAL).await;
            continue;
        }

        let (_, persisted_safe_head) = runtime.frame_clock_observation()?;
        if persisted_safe_head == target_safe_head {
            return Ok(());
        }
        if persisted_safe_head > target_safe_head {
            return Err(format!(
                "persisted safe head overshot {target_safe_head}: got {persisted_safe_head}"
            )
            .into());
        }
        tokio::time::sleep(FRAME_CLOCK_POLL_INTERVAL).await;
    }

    let (_, persisted_safe_head) = runtime.frame_clock_observation()?;
    Err(format!(
        "timed out waiting for persisted safe head {target_safe_head}: got {persisted_safe_head}"
    )
    .into())
}

/// Close the open batch, land it on L1, and wait for snapshot promotion so
/// `/finalized_state` reports a new `inclusion_block`.
async fn drive_finalized_gold_batch_for_watchdog(
    runtime: &ManagedSequencer,
    ws: &mut WsClient,
    replay: &mut ReplayWalletApp,
    alice_l2: &mut WalletL2Client,
    alice_address: Address,
) -> ScenarioResult<()> {
    let floor_inclusion_block = runtime.finalized_inclusion_block().await?.unwrap_or(0);
    let batches_before = runtime.count_batches()?;
    for _ in 0..TRANSFERS_TO_FORCE_BATCH_CLOSE {
        alice_l2.transfer(alice_address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    }
    let batches_after = runtime.count_batches()?;
    if batches_after.sealed <= batches_before.sealed {
        return Err(format!(
            "expected batch close before watchdog compare: before={batches_before:?} after={batches_after:?}"
        )
        .into());
    }

    mine_until_finalized_advances(runtime, floor_inclusion_block).await?;
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

    apply_reconciled_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        initial_funding_amount,
    )
    .await?;

    let (frame_anchor, persisted_safe_head) = runtime.frame_clock_observation()?;
    let live_safe_head = runtime.l1_safe_block_number().await?;
    assert_eq!(
        persisted_safe_head, frame_anchor,
        "funding reconciliation must leave a clean frame-clock anchor",
    );
    assert_eq!(
        live_safe_head, frame_anchor,
        "Anvil safe head must match the clean frame-clock anchor",
    );

    alice_l1
        .mint_supported_token(pending_deposit_amount)
        .await?;
    let deposit_block = alice_l1
        .deposit_supported_token(pending_deposit_amount)
        .await?;

    alice_l2.transfer(bob_address, transfer_amount).await?;
    let user_op = ws.expect_user_op_from(alice_address).await?;
    match &user_op {
        WsTxMessage::UserOp { safe_block, .. } => assert_eq!(
            *safe_block, frame_anchor,
            "below-threshold direct must not advance the user-op frame clock",
        ),
        other => unreachable!("expected user op after typed WS assertion, got {other:?}"),
    }
    replay.apply(user_op)?;

    let below_threshold = frame_anchor + FRAME_CLOCK_INTERVAL_SAFE_BLOCKS - 1;
    assert!(
        deposit_block <= below_threshold,
        "boundary setup consumed too many L1 blocks: deposit={deposit_block} \
         below_threshold={below_threshold}",
    );
    advance_persisted_safe_head_exactly(runtime, below_threshold).await?;
    assert_eq!(
        runtime.frame_clock_observation()?,
        (frame_anchor, below_threshold),
        "S+4 must leave the open frame at S",
    );
    ws.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    let at_threshold = frame_anchor + FRAME_CLOCK_INTERVAL_SAFE_BLOCKS;
    advance_persisted_safe_head_exactly(runtime, at_threshold).await?;
    let rotated_frame = advance_live_frame_until_covers(runtime, deposit_block).await?;
    assert_eq!(
        rotated_frame, at_threshold,
        "S+5 must create exactly one frame at the observed safe head",
    );
    let direct = ws
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    match &direct {
        WsTxMessage::DirectInput { block_number, .. } => assert_eq!(
            *block_number, deposit_block,
            "direct must retain its exact L1 inclusion block",
        ),
        other => unreachable!("expected direct after typed WS assertion, got {other:?}"),
    }
    replay.apply(direct)?;
    ws.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

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

    apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
        .await?;

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

    let deposit_message = apply_reconciled_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        deposit_amount,
    )
    .await?;
    // The resume claim names the next input, after the consumed deposit.
    let reconnect_claim =
        runtime.history_claim(deposit_message.offset().checked_add(1).unwrap())?;
    drop(ws);

    alice_l2.transfer(bob_address, transfer_amount).await?;
    bob_l2.withdraw(withdrawal_amount).await?;

    let mut resumed_ws = WsClient::connect(
        &SequencerClient::new(runtime.internal_endpoint())?,
        reconnect_claim,
    )
    .await?;
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

    apply_reconciled_supported_deposit(
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
    assert_eq!(
        replay_after_restart.canonical_snapshot_bytes()?,
        replay_before_restart.canonical_snapshot_bytes()?,
        "the replayed mirror's whole state must match the pre-restart live mirror",
    );
    assert_eq!(
        replay_after_restart.last_applied_block(),
        replay_before_restart.last_applied_block(),
        "the replayed feed must carry the same L1 blocks as the live feed",
    );

    // Reading the persisted feed alone does not prove the restarted engine
    // restored its balances and nonce. Require a fresh execution at nonce 1.
    let mut resumed_alice = runtime.wallet_l2(alice)?;
    assert_eq!(resumed_alice.served_next_nonce().await?, 1);
    resumed_alice.set_next_nonce(1);
    resumed_alice.transfer(bob_address, U256::from(1)).await?;
    let resumed = ws_after_restart.expect_user_op_from(alice_address).await?;
    replay_after_restart.apply(resumed.clone())?;
    replay_before_restart.apply(resumed)?;
    assert_eq!(
        replay_after_restart.canonical_snapshot_bytes()?,
        replay_before_restart.canonical_snapshot_bytes()?
    );
    assert_wallet_state(
        &replay_after_restart,
        ExpectedWalletState {
            address: alice_address,
            balance: expected_alice - U256::from(1) - gas,
            nonce: 2,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: expected_bob + U256::from(1),
            nonce: 1,
        },
        4,
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
    let deposit_block = alice_l1
        .deposit_token(unsupported_token, U256::from(123_000_u64))
        .await?;
    advance_live_frame_until_covers(runtime, deposit_block).await?;

    let direct = ws
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    match &direct {
        WsTxMessage::DirectInput { block_number, .. } => assert_eq!(
            *block_number, deposit_block,
            "unsupported-token direct must retain its L1 inclusion block",
        ),
        other => unreachable!("expected direct after typed WS assertion, got {other:?}"),
    }
    replay.apply(direct)?;

    assert_eq!(replay.current_user_balance(alice_address), U256::ZERO);
    assert_eq!(replay.current_user_nonce(alice_address), 0);
    assert_eq!(replay.l2_tx_count(), 1);
    Ok(())
}

/// Setup reads L1 before its first write, so a refusal there leaves the data
/// directory empty but for the lock file, and a retry may run in it.
pub(crate) fn assert_data_dir_holds_at_most_the_lock(
    runtime: &ManagedSequencer,
) -> ScenarioResult<()> {
    let entries = runtime.data_dir_entries()?;
    if entries.iter().any(|entry| entry != "sequencer.lock") {
        return Err(format!("a setup refused before writing left {entries:?}").into());
    }
    Ok(())
}

pub(crate) async fn apply_reconciled_supported_deposit(
    runtime: &ManagedSequencer,
    ws: &mut WsClient,
    replay: &mut ReplayWalletApp,
    wallet_l1: &WalletL1Client,
    amount: U256,
) -> ScenarioResult<WsTxMessage> {
    wallet_l1.mint_supported_token(amount).await?;
    let deposit_block = wallet_l1.deposit_supported_token(amount).await?;
    advance_live_frame_until_covers(runtime, deposit_block).await?;

    let message = ws
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    match &message {
        WsTxMessage::DirectInput { block_number, .. } => assert_eq!(
            *block_number, deposit_block,
            "reconciled direct must retain its exact L1 inclusion block",
        ),
        other => unreachable!("expected direct after typed WS assertion, got {other:?}"),
    }
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
    apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
        .await?;

    // Submit user-op with max_fee=0, which is below the default frame fee (1356).
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

/// Fixed fee-oracle exponent 100 → recommended frame fee 1456
/// (`100 + 296 + 20 + 419 + 621`), sampled by the first frame.
async fn run_fixed_fee_oracle_sets_frame_fee_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    const EXPECTED_FRAME_FEE: u16 = FIXED_LOG_GAS_PRICE as u16 + 296 + 20 + 419 + 621;

    assert_eq!(
        runtime.first_frame_fee()?,
        EXPECTED_FRAME_FEE,
        "the genesis frame must sample the setup-pinned fixed fee before any rotation"
    );

    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
        .await?;

    let transfer_amount = U256::from(100_u64);
    alice_l2.transfer(alice_address, transfer_amount).await?;
    let message = ws.expect_user_op_from(alice_address).await?;
    match &message {
        WsTxMessage::UserOp { .. } => {}
        other => return Err(format!("expected user op, got {other:?}").into()),
    }
    replay.apply(message)?;
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
        apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &l1, deposit_amount)
            .await?;
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

async fn run_multi_deposit_reconciliation_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
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

    // Default automining puts these deposits in separate blocks. This scenario
    // pins one reconciliation turn draining multiple accumulated directs; a
    // true same-block ordering scenario is tracked separately.
    let alice_deposit_block = alice_l1
        .mint_and_deposit_supported_token(alice_deposit)
        .await?;
    let bob_deposit_block = bob_l1.mint_and_deposit_supported_token(bob_deposit).await?;

    // Drive one or more semantic frame ticks until both deposits are covered.
    advance_live_frame_until_covers(runtime, alice_deposit_block.max(bob_deposit_block)).await?;

    // Expect two direct inputs (one per deposit).
    let portal = runtime.erc20_portal_address();
    replay.apply(ws.expect_direct_input_from(portal).await?)?;
    replay.apply(ws.expect_direct_input_from(portal).await?)?;

    assert_eq!(replay.current_user_balance(alice_address), alice_deposit);
    assert_eq!(replay.current_user_balance(bob_address), bob_deposit);
    assert_eq!(replay.l2_tx_count(), 2);
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

    apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
        .await?;

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

// ── Batch landing and outages ─────────────────────────────────────
//
// The drills of docs/recovery/README.md ("Scenarios"). Each derives its block
// counts from the oldest unaccepted batch's first frame `s` and the window:
// the history is void once the safe head `H` reaches `s + MAX_WAIT_BLOCKS - 1`
// (`H + 1 - s >= max_wait_blocks`). Outages advance L1 and the sequencer's
// wall clock together, so the safe head's age follows the outage.

/// The oldest batch L1 has not accepted.
fn oldest_unaccepted(runtime: &ManagedSequencer) -> ScenarioResult<UnacceptedBatch> {
    Ok(runtime
        .oldest_unaccepted_batch()?
        .ok_or("expected a batch L1 has not accepted")?)
}

/// The safe head at which a batch opened at `first_frame_safe_block` is void.
const fn void_safe_head(first_frame_safe_block: u64) -> u64 {
    first_frame_safe_block + MAX_WAIT_BLOCKS - 1
}

/// The safe head that leaves a batch opened at `first_frame_safe_block`
/// [`LANDING_ROOM_BLOCKS`] to land in.
const fn inside_window_safe_head(first_frame_safe_block: u64) -> u64 {
    void_safe_head(first_frame_safe_block) - LANDING_ROOM_BLOCKS
}

/// Advance L1 and the sequencer's wall clock together until Anvil's safe
/// head is `target`, in one `anvil_mine`.
async fn advance_safe_head_to(runtime: &mut ManagedSequencer, target: u64) -> ScenarioResult<()> {
    let live = runtime.l1_safe_block_number().await?;
    let blocks = target
        .checked_sub(live)
        .ok_or_else(|| format!("Anvil's safe head {live} is already past {target}"))?;
    runtime
        .advance_wall_and_mine(blocks_as_duration(blocks))
        .await?;
    let reached = runtime.l1_safe_block_number().await?;
    if reached != target {
        return Err(format!("mined to safe head {reached}, expected {target}").into());
    }
    Ok(())
}

/// Mine single blocks until Anvil's safe head covers `block`.
async fn mine_until_safe(runtime: &ManagedSequencer, block: u64) -> ScenarioResult<()> {
    for _ in 0..ACCEPTANCE_POLL_ATTEMPTS {
        if runtime.l1_safe_block_number().await? >= block {
            return Ok(());
        }
        runtime.mine_live_l1_blocks(1).await?;
    }
    Err(format!("Anvil's safe head never reached block {block}").into())
}

/// Poll `/healthz` until the running sequencer reports `landing_lag_blocks`
/// `expected` (`H + 1 - s`) beside the pinned window.
async fn wait_for_landing_lag(
    runtime: &ManagedSequencer,
    expected: u64,
) -> ScenarioResult<LandingHealth> {
    let mut health = runtime.landing_health().await?;
    for _ in 0..LANDING_POLL_ATTEMPTS {
        if health.landing_lag_blocks == Some(expected) {
            assert_eq!(health.max_wait_blocks, MAX_WAIT_BLOCKS);
            return Ok(health);
        }
        tokio::time::sleep(LANDING_POLL_INTERVAL).await;
        health = runtime.landing_health().await?;
    }
    Err(format!("expected landing_lag_blocks {expected}, /healthz reports {health:?}").into())
}

/// A running sequencer that sees a terminal fact aborts, and its log names
/// the fact. Every terminal runtime fault aborts, so the log tells them apart.
async fn expect_terminal_abort(
    runtime: &mut ManagedSequencer,
    facts: &[String],
) -> ScenarioResult<()> {
    let status = runtime.wait_for_exit(TERMINAL_STOP_TIMEOUT).await?;
    if !is_terminal_abort(&status) {
        return Err(format!("expected the terminal abort (SIGABRT), got {status:?}").into());
    }
    let log = runtime.read_log_contents()?;
    if !facts.iter().all(|fact| log.contains(fact.as_str())) {
        return Err(format!(
            "the aborted run must name {facts:?}; see {}",
            runtime.log_path().display()
        )
        .into());
    }
    Ok(())
}

/// The facts a void-history refusal names. `safe_head` is the safe block it
/// names, where the drill knows it exactly: a poster that lands a void batch
/// as the provider returns can move it by a block.
fn void_history_facts(first_frame_safe_block: u64, safe_head: Option<u64>) -> Vec<String> {
    let mut facts = vec![
        "the history is void".to_owned(),
        format!("opened at safe block {first_frame_safe_block}"),
        format!("max_wait_blocks {MAX_WAIT_BLOCKS}"),
    ];
    if let Some(safe_head) = safe_head {
        facts.push(format!("not accepted by safe block {safe_head}"));
    }
    facts
}

/// Start `run` and expect the terminal refusal: exit 30 before readiness and
/// its exit line naming `facts`. The terminal-fault black box stays as it
/// was: the database already holds a terminal fact, so a refusal copies none.
async fn expect_start_refused(
    runtime: &mut ManagedSequencer,
    facts: &[String],
) -> ScenarioResult<()> {
    let recorded = runtime.terminal_fault_causes()?;
    match runtime.try_respawn().await? {
        RunStart::Exited(status) if status.code() == Some(EXIT_TERMINAL) => {}
        other => {
            return Err(format!(
                "expected a start refused with exit {EXIT_TERMINAL}, got {other:?}"
            )
            .into());
        }
    }
    let log = runtime.read_log_contents()?;
    if !facts.iter().all(|fact| log.contains(fact.as_str())) {
        return Err(format!(
            "the refusal must name {facts:?}; see {}",
            runtime.log_path().display()
        )
        .into());
    }
    let causes = runtime.terminal_fault_causes()?;
    if causes != recorded {
        return Err(format!(
            "a start refused on {facts:?} must leave the black box as it was ({recorded:?}); \
             it holds {causes:?}"
        )
        .into());
    }
    Ok(())
}

/// Mine one block per second until L1 has accepted every batch through
/// `nonce`.
async fn mine_until_accepted_through(runtime: &ManagedSequencer, nonce: u64) -> ScenarioResult<()> {
    for _ in 0..ACCEPTANCE_POLL_ATTEMPTS {
        runtime.mine_l1_block_at_wall_clock().await?;
        tokio::time::sleep(ACCEPTANCE_POLL_INTERVAL).await;
        if runtime
            .batch_nonce_frontier()?
            .safe_accepted
            .is_some_and(|accepted| accepted >= nonce)
        {
            return Ok(());
        }
    }
    Err(format!("timed out waiting for L1 to accept batch nonce {nonce}").into())
}

/// The accepted batches are the nonces from zero, accepted in nonce order: a
/// batch lands under the nonce it was sealed with.
fn assert_accepted_in_order(runtime: &ManagedSequencer) -> ScenarioResult<()> {
    let accepted = runtime.accepted_batches()?;
    for (position, (nonce, _)) in (0_u64..).zip(&accepted) {
        assert_eq!(
            *nonce, position,
            "accepted nonces run from zero: {accepted:?}"
        );
    }
    for pair in accepted.windows(2) {
        assert!(
            pair[0].1 < pair[1].1,
            "batches are accepted in nonce order: {accepted:?}"
        );
    }
    Ok(())
}

/// Self-transfers until the lane seals at least one more batch by size.
async fn seal_a_batch_by_size(
    runtime: &ManagedSequencer,
    ws: &mut WsClient,
    replay: &mut ReplayWalletApp,
    wallet: &mut WalletL2Client,
    address: Address,
) -> ScenarioResult<()> {
    let sealed_before = runtime.count_batches()?.sealed;
    for _ in 0..TRANSFERS_TO_FORCE_BATCH_CLOSE {
        wallet.transfer(address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(address).await?)?;
    }
    let sealed = runtime.count_batches()?.sealed;
    if sealed <= sealed_before {
        return Err(
            format!("the transfers did not seal a batch: {sealed_before} -> {sealed}").into(),
        );
    }
    Ok(())
}

/// The feed from offset zero carries exactly what `expected` applied.
async fn assert_feed_matches(
    runtime: &ManagedSequencer,
    expected: &ReplayWalletApp,
) -> ScenarioResult<()> {
    let mut ws = runtime.ws(0).await?;
    let mut replay = ReplayWalletApp::devnet();
    for _ in 0..expected.l2_tx_count() {
        replay.apply(ws.next_message().await?)?;
    }
    ws.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;
    assert_eq!(
        replay.canonical_snapshot_bytes()?,
        expected.canonical_snapshot_bytes()?,
        "the feed serves the history as it was",
    );
    Ok(())
}

// (e) A short stop, well inside the window: the restart is nothing special.
// History, the served nonce and the open batch are unchanged; a direct that
// arrived while the sequencer was down is drained after it. Also the suite's
// process-level pin of the exit-code contract's clean class: SIGTERM on a
// healthy process exits 0.
async fn run_restart_after_a_short_outage_keeps_history_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay = ReplayWalletApp::devnet();

    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(100_000_u64);
    let late_amount = U256::from(50_000_u64);
    let gas = fee_to_linear(DEFAULT_FRAME_FEE);

    apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
        .await?;
    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    let open_batch = oldest_unaccepted(runtime)?;

    drop(ws);
    runtime.stop_expecting_clean_exit().await?;

    // A deposit while the sequencer is down, then a short outage.
    alice_l1.mint_supported_token(late_amount).await?;
    let late_block = alice_l1.deposit_supported_token(late_amount).await?;
    runtime
        .advance_wall_and_mine(blocks_as_duration(SHORT_OUTAGE_BLOCKS))
        .await?;
    runtime.respawn().await?;

    assert_eq!(
        oldest_unaccepted(runtime)?,
        open_batch,
        "the restart keeps the open batch: same nonce, same first frame",
    );
    let safe_head = runtime.l1_safe_block_number().await?;
    wait_for_landing_lag(runtime, safe_head + 1 - open_batch.first_frame_safe_block).await?;

    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    replay_after.apply(ws_after.expect_user_op_from(alice_address).await?)?;
    let late = ws_after
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    assert!(
        matches!(late, WsTxMessage::DirectInput { block_number, .. } if block_number == late_block),
        "the deposit made while the sequencer was down is drained after the restart: {late:?}",
    );
    replay_after.apply(late)?;
    ws_after.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    assert_wallet_state(
        &replay_after,
        ExpectedWalletState {
            address: alice_address,
            balance: deposit_amount - transfer_amount - gas + late_amount,
            nonce: 1,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: transfer_amount,
            nonce: 0,
        },
        3,
    );
    assert_eq!(
        runtime.wallet_l2(alice)?.served_next_nonce().await?,
        1,
        "GET /nonce keeps the confirmed transfer",
    );
    Ok(())
}

// (e) then (a): a stop that leaves the open batch just inside the window. The
// restart keeps history and the served nonce; the stop outlasted the lane's
// batch-open deadline, so the lane seals the batch at once, and it lands
// inside the window under its own nonce.
async fn run_restart_inside_the_window_keeps_history_then_lands_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay = ReplayWalletApp::devnet();

    apply_reconciled_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        U256::from(600_000_u64),
    )
    .await?;
    alice_l2
        .transfer(bob_address, U256::from(100_000_u64))
        .await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    let batch = oldest_unaccepted(runtime)?;

    drop(ws);
    runtime.stop().await?;
    advance_safe_head_to(
        runtime,
        inside_window_safe_head(batch.first_frame_safe_block),
    )
    .await?;
    runtime.respawn().await?;

    assert_feed_matches(runtime, &replay).await?;
    assert_eq!(runtime.wallet_l2(alice)?.served_next_nonce().await?, 1);

    mine_until_accepted_through(runtime, batch.nonce).await?;
    assert_accepted_in_order(runtime)?;
    assert_feed_matches(runtime, &replay).await?;
    Ok(())
}

// (a) at restart: the submission of a sealed batch is lost from the mempool
// while the sequencer is down. Restarted inside the window, the poster posts
// the same batch nonce again and L1 accepts it.
async fn run_dropped_submission_is_posted_again_after_restart_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay = ReplayWalletApp::devnet();

    let mut ws = runtime.ws(0).await?;
    apply_reconciled_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        U256::from(10_000_000_u64),
    )
    .await?;

    // Hold the submission in the mempool, then lose it.
    runtime.set_automine(false).await?;
    seal_a_batch_by_size(runtime, &mut ws, &mut replay, &mut alice_l2, alice_address).await?;
    runtime.wait_for_pending_batch_submission().await?;
    let batch = oldest_unaccepted(runtime)?;
    let served_nonce = alice_l2.served_next_nonce().await?;

    drop(ws);
    runtime.stop().await?;
    runtime.drop_all_pending_txs().await?;
    runtime.set_automine(true).await?;
    advance_safe_head_to(
        runtime,
        inside_window_safe_head(batch.first_frame_safe_block),
    )
    .await?;
    runtime.respawn().await?;

    mine_until_accepted_through(runtime, batch.nonce).await?;
    assert_accepted_in_order(runtime)?;
    assert_feed_matches(runtime, &replay).await?;
    assert_eq!(
        runtime.wallet_l2(alice)?.served_next_nonce().await?,
        served_nonce
    );
    Ok(())
}

// (a) live: batches stop landing while the sequencer runs (the submitter's
// transactions are refused), and the cause is fixed with the oldest batch
// just inside the window. The sequencer keeps serving and sealing, reports
// the lag, then posts the backlog in order once it can; a claim taken before
// the outage subscribes with no gap.
async fn run_batches_stop_landing_inside_the_window_then_land_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay = ReplayWalletApp::devnet();

    let mut ws = runtime.ws(0).await?;
    apply_reconciled_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        U256::from(20_000_000_u64),
    )
    .await?;
    let claim = runtime.history_claim(replay.l2_tx_count())?;

    runtime.hold_batch_submissions().await?;
    seal_a_batch_by_size(runtime, &mut ws, &mut replay, &mut alice_l2, alice_address).await?;
    let batch = oldest_unaccepted(runtime)?;
    advance_safe_head_to(
        runtime,
        inside_window_safe_head(batch.first_frame_safe_block),
    )
    .await?;

    // Inside the window: up, serving, sealing, reporting the lag.
    wait_for_landing_lag(runtime, MAX_WAIT_BLOCKS - LANDING_ROOM_BLOCKS).await?;
    seal_a_batch_by_size(runtime, &mut ws, &mut replay, &mut alice_l2, alice_address).await?;
    let frontier = runtime.batch_nonce_frontier()?;
    assert_eq!(
        frontier.safe_accepted, None,
        "nothing lands while submissions are held"
    );
    let sealed = frontier.sealed.ok_or("batches were sealed")?;
    let served_nonce = alice_l2.served_next_nonce().await?;
    assert_eq!(served_nonce, replay.current_user_nonce(alice_address));

    runtime.release_batch_submissions().await?;
    mine_until_accepted_through(runtime, sealed).await?;
    assert_accepted_in_order(runtime)?;
    assert_eq!(
        alice_l2.served_next_nonce().await?,
        served_nonce,
        "GET /nonce is unchanged"
    );

    let mut resumed = WsClient::connect(&runtime.sequencer_client()?, claim).await?;
    for offset in claim.next_input.get()..replay.l2_tx_count() {
        let message = resumed.next_message().await?;
        assert_eq!(
            message.offset(),
            offset,
            "the pre-outage claim resumes with no gap"
        );
    }
    drop(ws);
    Ok(())
}

// (b) Not fixed: submissions stay refused until the oldest batch is void
// while the sequencer runs. One block short of the boundary it is up and
// reports a lag of one block under the window; at the boundary it stops for
// good, and every start refuses. The operator's playbook (a new data
// directory, setup, run) starts a new era, which a client of the old one
// sees as ERA_CHANGED. A transaction of the old instance still in the
// mempool is replaced by setup's flush, and the new era's first batch lands
// at the era's anchor nonce.
async fn run_void_history_while_running_stops_for_good_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();
    // Fees for the new era's batch-sealing transfers.
    let deposit_amount = U256::from(10_000_000_u64);

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay = ReplayWalletApp::devnet();
    let claim = {
        let mut ws = runtime.ws(0).await?;
        apply_reconciled_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            deposit_amount,
        )
        .await?;
        alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(alice_address).await?)?;
        runtime.history_claim(replay.l2_tx_count())?
    };

    runtime.hold_batch_submissions().await?;
    let batch = oldest_unaccepted(runtime)?;
    let void_at = void_safe_head(batch.first_frame_safe_block);
    advance_safe_head_to(runtime, void_at - 1).await?;
    wait_for_landing_lag(runtime, MAX_WAIT_BLOCKS - 1).await?;
    assert_eq!(runtime.observe_for(STABILITY_WINDOW).await?, None);

    advance_safe_head_to(runtime, void_at).await?;
    let facts = void_history_facts(batch.first_frame_safe_block, Some(void_at));
    expect_terminal_abort(runtime, &facts).await?;
    expect_start_refused(runtime, &facts).await?;
    expect_start_refused(runtime, &facts).await?;

    // The cause is fixed, but a transaction signed with the submitter's key
    // is still in the mempool when the operator follows the playbook.
    runtime.release_batch_submissions().await?;
    runtime.set_automine(false).await?;
    let stranded = runtime
        .broadcast_input_as(
            &batch_submitter_signer()?,
            encode_batch(&Batch {
                nonce: batch.nonce,
                frames: Vec::new(),
            }),
        )
        .await?;
    runtime.discard_data_dir();
    runtime.set_mine_l1_during_setup(true);
    runtime.respawn().await?;
    runtime.set_mine_l1_during_setup(false);
    runtime.set_automine(true).await?;
    assert!(
        !runtime.transaction_mined(stranded).await?,
        "setup's flush replaces the stranded transaction with a no-op",
    );

    assert!(matches!(
        runtime.sequencer_client()?.subscribe(claim).await,
        Err(sequencer_rust_client::SubscribeError::History(
            sequencer_rust_client::HistoryPolicyError::EraChanged { .. }
        ))
    ));

    // The new era serves the rebuilt deposit, and its first batch lands at
    // the anchor nonce.
    let anchor = runtime.published_era()?.next_batch_nonce;
    let mut ws = runtime.ws(0).await?;
    let mut replay = ReplayWalletApp::devnet();
    replay.apply(
        ws.expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    assert_eq!(replay.current_user_balance(alice_address), deposit_amount);
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    seal_a_batch_by_size(runtime, &mut ws, &mut replay, &mut alice_l2, alice_address).await?;
    mine_until_accepted_through(runtime, anchor).await?;
    assert_eq!(
        runtime.accepted_batches()?.first().map(|(nonce, _)| *nonce),
        Some(anchor),
        "the new era's first accepted batch takes the anchor nonce",
    );
    Ok(())
}

// (c) live: the provider goes down while the sequencer runs, and L1 passes
// the window behind it. Blind, the sequencer keeps running: its landing lag
// freezes below the window while the safe head's age grows. It stops at the
// first sync that shows the void history, and a respawn refuses.
async fn run_blind_past_the_window_stops_at_the_first_sync_that_shows_it_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_reconciled_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            U256::from(500_000_u64),
        )
        .await?;
    }

    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    runtime.respawn().await?;
    {
        let mut ws = runtime.ws(0).await?;
        let mut alice_l2 = runtime.wallet_l2(alice)?;
        ws.expect_direct_input_from(runtime.erc20_portal_address())
            .await?;
        alice_l2.transfer(bob_address, U256::from(1_u64)).await?;
        replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    }
    let batch = oldest_unaccepted(runtime)?;

    proxy.disconnect();
    advance_safe_head_to(runtime, void_safe_head(batch.first_frame_safe_block)).await?;
    assert_eq!(
        runtime.observe_for(STABILITY_WINDOW).await?,
        None,
        "no exit while blind",
    );
    assert_blind(runtime).await?;

    proxy.reconnect();
    let facts = void_history_facts(batch.first_frame_safe_block, None);
    expect_terminal_abort(runtime, &facts).await?;
    expect_start_refused(runtime, &facts).await?;
    proxy.shutdown().await?;
    Ok(())
}

// (c) at start: the sequencer is stopped and the provider is down while L1
// passes the window. The start boots on its stored view, where the history is
// not void yet, and serves; the first sync after the provider returns shows
// the void history and stops it; a respawn refuses.
async fn run_blind_start_boots_on_the_stored_view_then_stops_at_the_first_sync_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
    let mut replay = ReplayWalletApp::devnet();
    apply_reconciled_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        U256::from(600_000_u64),
    )
    .await?;
    alice_l2
        .transfer(bob_address, U256::from(100_000_u64))
        .await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    let batch = oldest_unaccepted(runtime)?;

    drop(ws);
    runtime.stop().await?;
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();
    advance_safe_head_to(runtime, void_safe_head(batch.first_frame_safe_block)).await?;

    runtime.respawn().await?;
    assert_eq!(
        runtime.observe_for(STABILITY_WINDOW).await?,
        None,
        "a blind start runs on its stored view",
    );
    assert_blind(runtime).await?;
    assert_feed_matches(runtime, &replay).await?;

    proxy.reconnect();
    let facts = void_history_facts(batch.first_frame_safe_block, None);
    expect_terminal_abort(runtime, &facts).await?;
    expect_start_refused(runtime, &facts).await?;
    proxy.shutdown().await?;
    Ok(())
}

/// What a blind sequencer reports: a landing lag frozen below the window, and
/// a safe head as old as the outage.
async fn assert_blind(runtime: &ManagedSequencer) -> ScenarioResult<()> {
    let health = runtime.landing_health().await?;
    let lag = health
        .landing_lag_blocks
        .ok_or("a blind sequencer still reports its lag")?;
    assert!(
        lag < MAX_WAIT_BLOCKS,
        "the lag freezes below the window: {health:?}"
    );
    let age = health
        .safe_head_age_seconds
        .ok_or("a blind sequencer reports its safe head's age")?;
    // The outage spans most of the window; live mining before it can stamp
    // the stored safe block a little ahead of the wall clock.
    assert!(
        age >= blocks_as_duration(MAX_WAIT_BLOCKS / 2).as_secs(),
        "the safe head's age covers the outage the lag cannot see: {health:?}",
    );
    assert_eq!(health.max_wait_blocks, MAX_WAIT_BLOCKS);
    Ok(())
}

// (d) Stopped past the window: the open batch aged while the sequencer was
// down. One block short of the boundary a start boots; at the boundary the
// first start finds the void history at its sync and refuses, the next
// refuses on the database alone, and neither changes the batches.
async fn run_stopped_past_the_window_refuses_every_start_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();

    let mut ws = runtime.ws(0).await?;
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let mut replay = ReplayWalletApp::devnet();
    apply_reconciled_supported_deposit(
        runtime,
        &mut ws,
        &mut replay,
        &alice_l1,
        U256::from(600_000_u64),
    )
    .await?;
    alice_l2
        .transfer(bob_address, U256::from(100_000_u64))
        .await?;
    replay.apply(ws.expect_user_op_from(alice_address).await?)?;
    let batch = oldest_unaccepted(runtime)?;
    let void_at = void_safe_head(batch.first_frame_safe_block);

    // One block short of the boundary a start boots. Submissions are held,
    // so the batch its batch-open deadline seals cannot land.
    drop(ws);
    runtime.stop().await?;
    runtime.hold_batch_submissions().await?;
    advance_safe_head_to(runtime, void_at - 1).await?;
    runtime.respawn().await?;
    wait_for_landing_lag(runtime, MAX_WAIT_BLOCKS - 1).await?;
    runtime.stop().await?;

    // At the boundary every start refuses.
    advance_safe_head_to(runtime, void_at).await?;
    let facts = void_history_facts(batch.first_frame_safe_block, Some(void_at));
    let batches = runtime.count_batches()?;
    let frontier = runtime.batch_nonce_frontier()?;
    expect_start_refused(runtime, &facts).await?;
    expect_start_refused(runtime, &facts).await?;
    assert_eq!(
        runtime.count_batches()?,
        batches,
        "a refused start changes no batch"
    );
    assert_eq!(runtime.batch_nonce_frontier()?, frontier);
    assert_eq!(oldest_unaccepted(runtime)?, batch);
    Ok(())
}

// (d) with nothing confirmed: the first run's batch holds no user op, and it
// ages all the same. The start refuses, and so does one with L1 unreachable:
// the void history is a fact of the database.
async fn run_stopped_past_the_window_with_nothing_confirmed_refuses_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let batch = oldest_unaccepted(runtime)?;
    runtime.stop().await?;
    let void_at = void_safe_head(batch.first_frame_safe_block);
    advance_safe_head_to(runtime, void_at).await?;

    let facts = void_history_facts(batch.first_frame_safe_block, Some(void_at));
    expect_start_refused(runtime, &facts).await?;

    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();
    expect_start_refused(runtime, &facts).await?;
    proxy.shutdown().await?;
    Ok(())
}

// (f) A foreign batch at the sequencer's nonce: another process signs a batch
// with the submitter's key and lands it at the open batch's nonce. The running
// sequencer records the divergence and stops for good; every start refuses.
async fn run_foreign_batch_at_our_nonce_stops_for_good_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let batch = oldest_unaccepted(runtime)?;
    // An empty batch is never stale, so the scheduler accepts it at the nonce.
    let foreign = encode_batch(&Batch {
        nonce: batch.nonce,
        frames: Vec::new(),
    });
    let block = runtime
        .add_input_as(&batch_submitter_signer()?, foreign)
        .await?;
    mine_until_safe(runtime, block).await?;

    let facts = [format!(
        "canonical divergence at batch nonce {}",
        batch.nonce
    )];
    expect_terminal_abort(runtime, &facts).await?;
    assert_eq!(runtime.canonical_divergence()?.as_deref(), Some("foreign"));
    expect_start_refused(runtime, &facts).await?;
    expect_start_refused(runtime, &facts).await?;
    Ok(())
}

// The first `run` of an era with L1 unreachable: it has no batch to start on,
// so it exits with the retry class and opens none; once L1 is back, the next
// start opens the era's first batch.
async fn run_first_run_of_an_era_with_l1_down_retries_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    runtime.stop().await?;
    runtime.discard_data_dir();
    let setup = runtime.run_setup().await?;
    assert!(setup.success(), "setup with L1 up must succeed: {setup:?}");

    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();
    match runtime.try_respawn().await? {
        RunStart::Exited(status) if status.code() == Some(EXIT_RESTART_TRANSIENT) => {}
        other => {
            return Err(format!("expected exit {EXIT_RESTART_TRANSIENT}, got {other:?}").into());
        }
    }
    assert_eq!(
        runtime.count_batches()?.total,
        0,
        "the refused start opens no batch"
    );

    proxy.reconnect();
    runtime.respawn().await?;
    assert_eq!(
        oldest_unaccepted(runtime)?.nonce,
        0,
        "the era's first batch"
    );
    proxy.shutdown().await?;
    Ok(())
}

// Warm restart while the RPC is unreachable: the startup sync fails on the
// provider, and `run` starts on its stored L1 view and serves. The test keeps
// the proxy disconnected through readiness, one full background-worker retry
// cadence, POST /tx, and WS broadcast. A boot path that makes provider
// reachability a start condition fails at `respawn`; a runtime that launches
// without a usable inclusion lane fails the transaction or WS assertion.
async fn run_warm_restart_with_l1_down_test(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();
    let deposit_amount = U256::from(600_000_u64);
    let transfer_amount = U256::from(100_000_u64);
    let fee = fee_to_linear(DEFAULT_FRAME_FEE);

    // Persist replayable wallet state, then stop cleanly.
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut ws = runtime.ws(0).await?;
    let mut replay = ReplayWalletApp::devnet();
    apply_reconciled_supported_deposit(runtime, &mut ws, &mut replay, &alice_l1, deposit_amount)
        .await?;
    drop(ws);
    runtime.stop().await?;

    // Route the next process through an already-disconnected gateway. The
    // proxy stays down until after the runtime serves the write and
    // its corresponding ordered-tx feed event.
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();
    runtime.respawn().await?;
    assert!(
        !proxy.is_connected(),
        "warm restart must reach readiness without reconnecting the L1 proxy",
    );

    // Cross the 2s input-reader/landing-watch cadence before submitting, so
    // this proves more than racing a request ahead of the first failed RPC
    // retry.
    let exit = runtime.observe_for(STABILITY_WINDOW).await?;
    assert!(
        exit.is_none(),
        "a warm runtime must remain live with L1 down; got {exit:?}",
    );

    let mut ws_after = runtime.ws(0).await?;
    let mut replay_after = ReplayWalletApp::devnet();
    replay_after.apply(
        ws_after
            .expect_direct_input_from(runtime.erc20_portal_address())
            .await?,
    )?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    alice_l2.transfer(bob_address, transfer_amount).await?;
    replay_after.apply(ws_after.expect_user_op_from(alice_address).await?)?;

    assert_wallet_state(
        &replay_after,
        ExpectedWalletState {
            address: alice_address,
            balance: deposit_amount - transfer_amount - fee,
            nonce: 1,
        },
        ExpectedWalletState {
            address: bob_address,
            balance: transfer_amount,
            nonce: 0,
        },
        2,
    );
    assert!(
        !proxy.is_connected(),
        "POST /tx and WS must succeed before L1 connectivity is restored",
    );

    proxy.reconnect();
    runtime.set_l1_endpoint_override(None);
    proxy.shutdown().await?;
    Ok(())
}

// Provider outage inside the window while the sequencer stays running.
//
// Load-under-outage check: the sequencer must continue to accept user ops,
// persist them, broadcast on WS, and CLOSE BATCHES BY SIZE while its L1
// connection is down. Proves the inclusion lane is independent of L1
// reachability: nothing stops intake on a stale L1 view.
//
// Scenario:
//   1. Spawn + apply a large deposit so Alice can fund many transfers.
//   2. Route the sequencer through a proxy (stop → set override → respawn).
//   3. Disconnect the proxy, advance L1 well inside the window.
//   4. Submit enough transfers (~150 × ~100 B each ≈ 15 KB) to exceed the
//      default ~12 KB batch-size target, guaranteeing at least one size-
//      triggered batch close during the outage.
//   5. Assert `count_batches().sealed` strictly increased during the outage.
//   6. Reconnect the proxy; confirm one more transfer goes through and the
//      schema invariants hold post-test.
async fn run_provider_outage_inside_the_window_sequencer_continues_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
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
    // Default fee per user-op ≈ 38276 units (log-fee 1356); reserve margin.
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let deposit_amount = U256::from(10_000_000_u64);
    let mut replay = ReplayWalletApp::devnet();
    {
        let mut ws = runtime.ws(0).await?;
        apply_reconciled_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            deposit_amount,
        )
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

    // Step 4: Cut the L1 connection and advance Anvil well inside the
    // window. The sequencer keeps running and its workers keep retrying.
    proxy.disconnect();
    runtime.mine_l1_blocks(SHORT_OUTAGE_BLOCKS).await?;

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

    // Sanity: total sealed batches grew from baseline to final.
    let batches_final = runtime.count_batches()?;
    assert!(
        batches_final.sealed > batches_before.sealed,
        "final sealed count {final:?} must exceed baseline {before:?}",
        final = batches_final,
        before = batches_before,
    );

    proxy.shutdown().await?;
    Ok(())
}

// Short-duration provider hiccup.
//
// The most-common production fault: an RPC gateway flakes briefly, retries
// succeed.
//
// What this tests that the longer-disconnect provider-outage test doesn't: the longer test disconnects for a
// 500-block L1 advance + 150 transfers worth of real time, exercising the
// inclusion lane under load. This test instead exercises the "pure retry
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
//   4. Assert POST /tx still works and the sealed count never falls.
async fn run_provider_outage_short_hiccup_test(
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
        apply_reconciled_supported_deposit(
            runtime,
            &mut ws,
            &mut replay,
            &alice_l1,
            deposit_amount,
        )
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
    // disconnected proxy and retries.
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
    assert!(
        batches_after.sealed >= batches_before.sealed,
        "sealed-batch count must be monotonic across a hiccup: \
         before={batches_before:?}, after={batches_after:?}",
    );

    proxy.shutdown().await?;
    Ok(())
}

async fn run_ws_subscribe_ahead_of_head_is_refused_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let claim = runtime.history_claim(u64::MAX)?;
    assert!(matches!(
        SequencerClient::new(runtime.internal_endpoint())?
            .subscribe(claim)
            .await,
        Err(sequencer_rust_client::SubscribeError::History(
            sequencer_rust_client::HistoryPolicyError::AheadOfHead { .. }
        ))
    ));
    Ok(())
}

// Replay determinism: for any workload accepted live, catch-up
// replay must produce an identical per-user state.
//
// This is the `Application` trait's fundamental contract (see
// `docs/protocol/application-contract.md`). Without it, restart
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
    apply_reconciled_supported_deposit(
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

    apply_reconciled_supported_deposit(
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

    let expected_input_count = replay_live.l2_tx_count();

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
    assert_eq!(replay_post.l2_tx_count(), replay_live.l2_tx_count(),);
    assert_eq!(
        replay_post.canonical_snapshot_bytes()?,
        replay_live.canonical_snapshot_bytes()?,
        "the replayed mirror's whole state must match the live mirror",
    );
    assert_eq!(
        replay_post.last_applied_block(),
        replay_live.last_applied_block(),
        "the replayed feed must carry the same L1 blocks as the live feed",
    );

    Ok(())
}

// Transient provider outage: the L1 input reader must
// retry on provider errors (connection refused, timeout) without
// crashing, and pick up the backlog on reconnect.
//
// Distinct from `provider_outage_short_hiccup_test`, which tests
// the batch submitter's retry path via POST activity. Here the interesting
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
    apply_reconciled_supported_deposit(
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

// First-ever boot with no deployment identity + L1 unreachable: setup refuses.
//
// Distinct from `run_first_run_of_an_era_with_l1_down_retries_test`, where
// setup succeeded and `run` meets the outage. This test targets the earlier
// failure: setup's L1 discovery, which first checks the chain id and then asks
// L1 for the InputBox address. With no deployment identity, those reads have
// no fallback and setup fails before it writes anything.
//
// The harness simulates "no deployment identity" by moving to a new data
// directory after a normal boot (a truly first-ever boot also lacks a pinned
// identity, and the failure mode is identical: the bootstrap step has nothing
// to fall back to).
async fn run_first_boot_no_identity_l1_unreachable_refuses_boot_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    {
        let _ws = runtime.ws(0).await?;
    }
    runtime.stop().await?;
    runtime.discard_data_dir();

    // Route through a disconnected proxy so setup's first L1 read (the chain
    // id) fails with a provider error.
    let proxy = TcpProxy::spawn(runtime.l1_endpoint()).await?;
    runtime.set_l1_endpoint_override(Some(proxy.endpoint()));
    proxy.disconnect();

    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "first boot with no deployment identity + L1 unreachable must refuse boot, got {respawn_result:?}",
    );
    // Setup reads L1 before its first write, so the refusal leaves the
    // directory empty (the lock file aside) and a retry may set it up.
    assert_data_dir_holds_at_most_the_lock(runtime)?;

    // Verify reversibility: reconnect proxy, respawn in the same directory,
    // this time the bootstrap step succeeds and pins the deployment identity.
    proxy.reconnect();
    runtime.respawn().await?;

    proxy.shutdown().await?;
    Ok(())
}

// Chain-id mismatch via the live RPC path.
//
// Companion to `chain_id_mismatch_from_deployment_identity_returns_typed_error`
// in `sequencer/tests/chain_id_validation.rs`. The identity-fallback test runs
// in-process against a pre-seeded DB; this test runs the full sequencer binary
// against real Anvil with a deliberately mismatched `--chain-id`, proving the
// RPC-comparison path refuses before pinning a wrong deployment identity.
//
// The pre-write ordering matters: a regression that swapped the
// identity-write and the chain-id check would leave a bad identity row on
// disk, poisoning future startups. Asserting `respawn_result.is_err()`
// alone catches the bad-error case; we additionally verify a
// post-correction respawn succeeds, which only happens if identity pinning
// wasn't poisoned (bootstrap checks the L1 chain id again, sees it matches,
// then writes the correct deployment identity).
async fn run_chain_id_mismatch_via_live_rpc_refuses_boot_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    // Anvil runs at `DEVNET_CHAIN_ID = 31337`. Pick something obviously
    // different that's still valid (chain_id > 0).
    const WRONG_CHAIN_ID: u64 = 99_999;

    // Initial boot completes normally (no override). This pins the correct
    // deployment identity.
    {
        let _ws = runtime.ws(0).await?;
    }
    runtime.stop().await?;

    // A new data directory, so setup's live RPC path runs (otherwise the
    // identity-fallback path would catch the mismatch first).
    runtime.discard_data_dir();

    // Configure a mismatched chain id and respawn. The bootstrap-time
    // RPC check returns the actual chain id (31337), compares it with
    // the configured `--chain-id` (99999), and returns ChainIdMismatch.
    runtime.set_chain_id_override(Some(WRONG_CHAIN_ID));
    let respawn_result = runtime.respawn().await;
    assert!(
        respawn_result.is_err(),
        "chain-id mismatch via live RPC must refuse boot, got {respawn_result:?}",
    );
    assert_data_dir_holds_at_most_the_lock(runtime)?;

    // Reset to the correct chain id. Respawn in the same directory must
    // succeed — proves the failed attempt wrote nothing.
    runtime.set_chain_id_override(None);
    runtime.respawn().await?;

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
    l2_tx_count: u64,
) {
    assert_eq!(replay.current_user_balance(first.address), first.balance);
    assert_eq!(replay.current_user_nonce(first.address), first.nonce);
    assert_eq!(replay.current_user_balance(second.address), second.balance);
    assert_eq!(replay.current_user_nonce(second.address), second.nonce);
    assert_eq!(replay.l2_tx_count(), l2_tx_count);
}
