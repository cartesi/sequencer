// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! A cold consumer restores an HTTP archive and follows its claim through replay,
//! live writes, and a process-boundary recovery. Its reference starts at genesis.

use std::time::Duration;

use alloy_primitives::U256;
use rollups_harness::{ManagedSequencer, ReplayWalletApp, TestSigner, WsClient};
use sequencer_core::api::WsTxMessage;
use sequencer_rust_client::{
    HistoryClaim, HistoryPolicyError, SequencerClient, SnapshotResponse, SubscribeError,
};

use crate::ScenarioResult;
use crate::test_cases::advance_live_frame_until_covers;

pub(crate) async fn run(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    tokio::time::timeout(Duration::from_secs(120), run_scenario(runtime))
        .await
        .map_err(|_| "cold replica scenario exceeded its 120-second deadline")?
}

async fn run_scenario(runtime: &mut ManagedSequencer) -> ScenarioResult<()> {
    let client = SequencerClient::new(runtime.endpoint())?;
    let genesis = client.latest_snapshot().await?;
    assert_eq!(genesis.claim.next_input.get(), 0);
    let mut reference_ws = WsClient::connect(&client, genesis.claim).await?;
    drop(genesis);
    let mut reference = ReplayWalletApp::devnet();
    let mut history = Vec::new();

    let alice = TestSigner::from_default(1)?;
    let alice_address = alice.address();
    let bob_address = TestSigner::from_default(2)?.address();
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    let deposit = U256::from(100_000_000_u64);
    let deposit_block = alice_l1.mint_and_deposit_supported_token(deposit).await?;
    advance_live_frame_until_covers(runtime, deposit_block).await?;
    record(&mut reference_ws, &mut reference, &mut history).await?;

    // Size closure supplies a nonempty immutable snapshot while leaving a suffix
    // in the next batch. Only the first batch is submitted before the outage.
    for _ in 0..150 {
        alice_l2.transfer(alice_address, U256::from(1)).await?;
        record(&mut reference_ws, &mut reference, &mut history).await?;
    }
    wait_for_accepted_snapshot(runtime).await?;
    let snapshot = client.latest_snapshot().await?;
    let original_claim = snapshot.claim;
    let snapshot_count = original_claim.next_input.get();
    assert!(
        snapshot_count > 0,
        "the consumer must restore nonempty state"
    );
    assert!(snapshot_count <= reference.executed_input_count());

    // Hold the response without consuming its body. This write is acknowledged
    // after snapshot selection and before archive restoration or subscription.
    alice_l2.transfer(bob_address, U256::from(1_000)).await?;
    record(&mut reference_ws, &mut reference, &mut history).await?;
    let (mut replica, downloaded_claim) = restore(snapshot).await?;
    assert_eq!(downloaded_claim, original_claim);
    let snapshot_reference = replay_prefix(&history, snapshot_count)?;
    assert_same_state(&replica, &snapshot_reference)?;
    assert!(replica.executed_input_count() < reference.executed_input_count());

    let backlog_deposit = alice_l1
        .mint_and_deposit_supported_token(U256::from(70_000))
        .await?;
    advance_live_frame_until_covers(runtime, backlog_deposit).await?;
    record(&mut reference_ws, &mut reference, &mut history).await?;
    alice_l2.transfer(bob_address, U256::from(2_000)).await?;
    record(&mut reference_ws, &mut reference, &mut history).await?;

    let mut replica_ws = WsClient::connect(&client, downloaded_claim).await?;
    let backlog_head = reference.executed_input_count();
    let catch_up_target = backlog_head + 2;
    let (first_replayed, replay_started) = tokio::sync::oneshot::channel();
    let (writes_committed, resume_replay) = tokio::sync::oneshot::channel();
    let producer = async {
        replay_started.await?;
        for amount in [3_000_u64, 4_000] {
            alice_l2.transfer(bob_address, U256::from(amount)).await?;
            record(&mut reference_ws, &mut reference, &mut history).await?;
        }
        writes_committed
            .send(())
            .map_err(|_| "consumer dropped the commit barrier")?;
        ScenarioResult::Ok(())
    };
    let consumer = async {
        replica.apply(replica_ws.next_message().await?)?;
        assert!(replica.executed_input_count() < backlog_head);
        first_replayed
            .send(())
            .map_err(|_| "producer dropped the replay barrier")?;
        resume_replay.await?;
        consume_until(&mut replica_ws, &mut replica, catch_up_target).await
    };
    futures::try_join!(producer, consumer)?;
    assert_same_state(&replica, &reference)?;
    replica_ws
        .expect_no_message_for(Duration::from_millis(100))
        .await?;

    // Already at the tip: these inputs must arrive through live continuation.
    let clock_before_live = replica.last_executed_safe_block();
    let live_deposit = alice_l1
        .mint_and_deposit_supported_token(U256::from(80_000))
        .await?;
    advance_live_frame_until_covers(runtime, live_deposit).await?;
    record(&mut reference_ws, &mut reference, &mut history).await?;
    alice_l2.transfer(bob_address, U256::from(5_000)).await?;
    record(&mut reference_ws, &mut reference, &mut history).await?;
    consume_until(
        &mut replica_ws,
        &mut replica,
        reference.executed_input_count(),
    )
    .await?;
    assert_same_state(&replica, &reference)?;
    assert!(replica.last_executed_safe_block() > clock_before_live);
    assert_eq!(
        replica.current_user_balance(bob_address),
        U256::from(15_000)
    );

    let stale_claim = HistoryClaim {
        next_input: sequencer_rust_client::ExecutedInputCount::new(replica.executed_input_count()),
        ..downloaded_claim
    };
    drop(replica_ws);
    drop(reference_ws);
    runtime.stop().await?;
    runtime
        .advance_wall_and_mine(Duration::from_secs(
            (sequencer_core::MAX_WAIT_BLOCKS + 50) * 12,
        ))
        .await?;
    runtime.respawn().await?;

    // Respawn chooses a fresh listener. The claim comes exclusively from the
    // downloaded snapshot and consumed inputs, never the harness's DB helpers.
    let client = SequencerClient::new(runtime.endpoint())?;
    assert!(matches!(
        client.subscribe(stale_claim).await,
        Err(SubscribeError::History(
            HistoryPolicyError::StaleGeneration { .. }
        ))
    ));
    let (mut recovered, fresh_claim) = restore(client.latest_snapshot().await?).await?;
    assert_eq!(fresh_claim.version.era_id, original_claim.version.era_id);
    assert_eq!(
        fresh_claim.version.recovery_generation.get(),
        original_claim.version.recovery_generation.get() + 1,
    );
    assert_eq!(fresh_claim.next_input.get(), snapshot_count);

    // Reconstruct the expected replacement branch independently: the accepted
    // prefix survives, optimistic user ops disappear, and L1 directs replay.
    let mut recovered_reference = replay_prefix(&history, snapshot_count)?;
    assert_same_state(&recovered, &recovered_reference)?;
    for message in &history[snapshot_count as usize..] {
        if let WsTxMessage::DirectInput { .. } = message {
            let mut direct = message.clone();
            if let WsTxMessage::DirectInput { offset, .. } = &mut direct {
                *offset = recovered_reference.executed_input_count();
            }
            recovered_reference.apply(direct)?;
        }
    }
    let mut recovered_ws = WsClient::connect(&client, fresh_claim).await?;
    consume_until(
        &mut recovered_ws,
        &mut recovered,
        recovered_reference.executed_input_count(),
    )
    .await?;
    assert_same_state(&recovered, &recovered_reference)?;
    assert_eq!(recovered.current_user_balance(bob_address), U256::ZERO);
    assert!(recovered.executed_input_count() < replica.executed_input_count());

    let mut alice_l2 = runtime.wallet_l2(TestSigner::from_default(1)?)?;
    alice_l2.set_next_nonce(recovered_reference.current_user_nonce(alice_address));
    alice_l2.transfer(bob_address, U256::from(6_000)).await?;
    let resumed = recovered_ws.expect_user_op_from(alice_address).await?;
    recovered.apply(resumed.clone())?;
    recovered_reference.apply(resumed)?;
    assert_same_state(&recovered, &recovered_reference)?;
    assert_eq!(
        recovered.current_user_balance(bob_address),
        U256::from(6_000)
    );
    recovered_ws
        .expect_no_message_for(Duration::from_millis(100))
        .await?;
    Ok(())
}

async fn restore(snapshot: SnapshotResponse) -> ScenarioResult<(ReplayWalletApp, HistoryClaim)> {
    let claim = snapshot.claim;
    assert_eq!(
        snapshot.response.headers()["Content-Type"],
        "application/x-tar"
    );
    let archive = snapshot.response.bytes().await?;
    let directory = tempfile::tempdir()?;
    tar::Archive::new(archive.as_ref()).unpack(directory.path())?;
    assert!(directory.path().join("info.toml").is_file());
    let app = ReplayWalletApp::from_dump(&directory.path().join("state"))?;
    assert_eq!(app.executed_input_count(), claim.next_input.get());
    // Subsequent replay also checks that restoring does not retain a dependency
    // on the downloaded source directory.
    directory.close()?;
    Ok((app, claim))
}

async fn record(
    ws: &mut WsClient,
    reference: &mut ReplayWalletApp,
    history: &mut Vec<WsTxMessage>,
) -> ScenarioResult<()> {
    let message = ws.next_message().await?;
    reference.apply(message.clone())?;
    history.push(message);
    Ok(())
}

async fn consume_until(
    ws: &mut WsClient,
    app: &mut ReplayWalletApp,
    target: u64,
) -> ScenarioResult<()> {
    while app.executed_input_count() < target {
        app.apply(ws.next_message().await?)?;
    }
    assert_eq!(app.executed_input_count(), target);
    Ok(())
}

fn replay_prefix(history: &[WsTxMessage], count: u64) -> ScenarioResult<ReplayWalletApp> {
    let mut app = ReplayWalletApp::devnet();
    for message in &history[..usize::try_from(count)?] {
        app.apply(message.clone())?;
    }
    Ok(app)
}

fn assert_same_state(actual: &ReplayWalletApp, expected: &ReplayWalletApp) -> ScenarioResult<()> {
    assert_eq!(
        actual.executed_input_count(),
        expected.executed_input_count()
    );
    assert_eq!(
        actual.last_executed_safe_block(),
        expected.last_executed_safe_block()
    );
    assert_eq!(
        actual.canonical_snapshot_bytes()?,
        expected.canonical_snapshot_bytes()?,
        "all wallet state, including balances, nonces, config, count, and clock"
    );
    Ok(())
}

async fn wait_for_accepted_snapshot(runtime: &ManagedSequencer) -> ScenarioResult<()> {
    for _ in 0..40 {
        runtime.mine_live_l1_blocks(1).await?;
        if runtime
            .finalized_inclusion_block()
            .await?
            .is_some_and(|b| b > 0)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err("timed out waiting for the initial batch's accepted snapshot".into())
}
