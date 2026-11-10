// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Setup as an operator runs it: in an empty data directory only, from a
//! base (genesis or a stored canonical machine) through a stop block, then
//! `run`, then a wallet client of the old era adopting the new one at its own
//! count. The procedure is docs/recovery/cockroach.md ("What setup does").

use std::path::PathBuf;
use std::time::Duration;

use alloy_primitives::{Address, U256};
use rollups_harness::{
    ManagedSequencer, RebuiltHistoryRows, ReplayWalletApp, TestSigner, WalletL2Client, WsClient,
};
use sequencer_core::api::WsTxMessage;
use sequencer_rust_client::{
    AdoptEraError, AdoptError, ExecutedInputCount, HistoryClaim, HistoryPolicyError,
    SequencerClient, SubscribeError,
};

use crate::ScenarioResult;
use crate::test_cases::{
    NO_WS_MESSAGE_WAIT, TRANSFERS_TO_FORCE_BATCH_CLOSE, apply_reconciled_supported_deposit,
    mine_until_finalized_advances,
};
use crate::watchdog_compare::{
    DEVNET_MACHINE_IMAGE, Watchdog, head_block, machine_image, require_cartesi_machine,
};

/// Setup's terminal exit class: do not restart, page.
const EXIT_TERMINAL: i32 = 30;

/// Bound on single-block mines while waiting for Anvil's safe tag to cover a
/// block; its safe head trails the chain head by a few blocks.
const SAFE_HEAD_MINE_ATTEMPTS: usize = 64;

/// A genesis setup on a fresh chain publishes an empty era; the dry run
/// reports it without touching the data directory; and a second setup on the
/// set-up directory is refused with exit 30, leaving it as it was for `run`.
pub(crate) async fn run_setup_refuses_a_set_up_data_directory_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    let era = runtime.published_era()?;
    assert_eq!(era.base_count, 0, "a genesis base starts at count zero");
    assert_eq!(
        era.rebuilt_history_end, 0,
        "a fresh chain has nothing to rebuild"
    );
    assert_eq!(era.first_queued_input, 0, "nothing is queued");
    assert_eq!(
        era.next_batch_nonce, 0,
        "the batch tree starts at nonce zero"
    );
    assert_eq!(
        runtime.rebuilt_history_rows()?,
        RebuiltHistoryRows {
            total: 0,
            executed: 0
        }
    );
    runtime.stop().await?;
    let entries = runtime.data_dir_entries()?;

    let dry_run = runtime.setup_dry_run().await?;
    assert!(dry_run.success(), "the dry run must exit 0: {dry_run:?}");
    assert!(
        dry_run.stdout.contains("base: 0 L1 inputs, L2 tx count 0"),
        "the dry run reports its base: {}",
        dry_run.stdout
    );
    assert_eq!(
        runtime.data_dir_entries()?,
        entries,
        "the dry run never touches the data directory"
    );

    let refused = runtime.run_setup().await?;
    assert_eq!(
        refused.code(),
        Some(EXIT_TERMINAL),
        "setup refuses a set-up directory with the terminal class: {refused:?}"
    );
    assert_eq!(
        runtime.data_dir_entries()?,
        entries,
        "a refused setup leaves the directory as it was"
    );
    assert_eq!(
        runtime.published_era()?,
        era,
        "a refused setup changes no era field"
    );

    // `run` still starts on the untouched directory.
    runtime.respawn().await?;
    let _ws = runtime.ws(0).await?;
    Ok(())
}

/// The round trip from genesis: any base works, genesis included, though the
/// rebuild re-judges every batch the old instance landed.
pub(crate) async fn run_recovery_from_genesis_round_trip_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    run_round_trip(runtime, RecoveryBase::Genesis).await
}

/// The round trip from a stored canonical machine, taken at the first
/// batch's inclusion block by the watchdog's `replay` from the template image.
pub(crate) async fn run_recovery_from_stored_machine_round_trip_test(
    runtime: &mut ManagedSequencer,
) -> ScenarioResult<()> {
    require_cartesi_machine();
    run_round_trip(runtime, RecoveryBase::StoredMachine).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryBase {
    Genesis,
    StoredMachine,
}

/// A toy-wallet client of the feed: its state, and every row it applied,
/// kept by count as a backup would keep them.
struct WalletClient {
    app: ReplayWalletApp,
    rows: Vec<WsTxMessage>,
}

impl WalletClient {
    fn new() -> Self {
        Self {
            app: ReplayWalletApp::devnet(),
            rows: Vec::new(),
        }
    }

    fn apply(&mut self, message: WsTxMessage) -> ScenarioResult<()> {
        self.app.apply(message.clone())?;
        self.rows.push(message);
        Ok(())
    }

    fn count(&self) -> u64 {
        self.app.executed_input_count()
    }

    /// The client restored from its backup at `count`.
    fn restored_at(&self, count: u64) -> ScenarioResult<Self> {
        let mut restored = Self::new();
        for row in self
            .rows
            .iter()
            .take(usize::try_from(count).map_err(|_| "count overflows usize")?)
        {
            restored.apply(row.clone())?;
        }
        if restored.count() != count {
            return Err(format!(
                "the client kept {} rows, cannot restore count {count}",
                restored.count()
            )
            .into());
        }
        Ok(restored)
    }
}

/// What the old instance left on L1 that the scenario checks the new era
/// against.
struct OldEra {
    /// The accepted checkpoint after the first batch landed.
    first_accepted: (u64, u64),
    /// The accepted checkpoint after the second batch landed: the client's
    /// trusted count is its count.
    last_accepted_count: u64,
    /// The claim the client held in the old era.
    claim: HistoryClaim,
    /// The old era's head; the rows past the last accepted count were served
    /// but never submitted.
    head: u64,
}

async fn run_round_trip(runtime: &mut ManagedSequencer, base: RecoveryBase) -> ScenarioResult<()> {
    let alice = TestSigner::from_default(1)?;
    let bob = TestSigner::from_default(2)?;
    let alice_address = alice.address();
    let bob_address = bob.address();
    let alice_l1 = runtime.wallet_l1(alice.clone()).await?;
    let mut client = WalletClient::new();

    // ── The old era: two batches land, and the old instance serves more
    // rows in a batch it never submits.
    let old = {
        let mut ws = runtime.ws(0).await?;
        let mut alice_l2 = runtime.wallet_l2(alice.clone())?;
        let mut bob_l2 = runtime.wallet_l2(bob.clone())?;
        let deposit = apply_reconciled_supported_deposit(
            runtime,
            &mut ws,
            &mut client.app,
            &alice_l1,
            // Fees for the two batch-sealing pumps of self-transfers.
            U256::from(30_000_000_u64),
        )
        .await?;
        client.rows.push(deposit);
        alice_l2
            .transfer(bob_address, U256::from(400_000_u64))
            .await?;
        client.apply(ws.expect_user_op_from(alice_address).await?)?;
        let first_accepted =
            land_a_batch(runtime, &mut ws, &mut client, &mut alice_l2, alice_address).await?;

        bob_l2
            .transfer(alice_address, U256::from(1_000_u64))
            .await?;
        client.apply(ws.expect_user_op_from(bob_address).await?)?;
        let (_, last_accepted_count) =
            land_a_batch(runtime, &mut ws, &mut client, &mut alice_l2, alice_address).await?;
        OldEra {
            first_accepted,
            last_accepted_count,
            claim: runtime.history_claim(last_accepted_count)?,
            head: client.count(),
        }
    };

    // ── The old instance stops for good; a direct arrives while it is down
    // and becomes safe, so it is queued at the stop block.
    runtime.stop().await?;
    let queued_amount = U256::from(500_000_u64);
    alice_l1.mint_supported_token(queued_amount).await?;
    let queued_block = alice_l1.deposit_supported_token(queued_amount).await?;
    mine_until_safe(runtime, queued_block).await?;
    let input_blocks = runtime.input_blocks().await?;
    let queued_input = u64::try_from(input_blocks.len())?
        .checked_sub(1)
        .ok_or("the queued deposit is an input")?;

    // ── The base, and the client's trusted state at the last accepted count.
    let (base_dir, base_count, base_inputs) = match base {
        RecoveryBase::Genesis => (None, 0, 0),
        RecoveryBase::StoredMachine => {
            let (block, count) = old.first_accepted;
            let dir = stored_machine_at(runtime, block).await?;
            let inputs = input_blocks.iter().filter(|b| **b <= block).count();
            (Some(dir), count, inputs)
        }
    };
    let trusted = old.last_accepted_count;
    let mut client = client.restored_at(trusted)?;

    // ── The operator's playbook: a new data directory, the dry run, setup,
    // run.
    runtime.discard_data_dir();
    runtime.set_setup_base(base_dir);
    let dry_run = runtime.setup_dry_run().await?;
    assert!(dry_run.success(), "the dry run must exit 0: {dry_run:?}");
    for expected in [
        format!("base: {base_inputs} L1 inputs, L2 tx count {base_count}"),
        "the rebuild re-judges the old instance's batches past the base".to_string(),
    ] {
        assert!(
            dry_run.stdout.contains(&expected),
            "the dry run must report `{expected}`: {}",
            dry_run.stdout
        );
    }
    assert!(
        runtime.data_dir_entries()?.is_empty(),
        "the dry run never creates the data directory"
    );
    runtime.respawn().await?;

    // ── The published era.
    let era = runtime.published_era()?;
    assert_eq!(era.base_count, base_count, "the era starts at the base");
    assert_eq!(
        era.rebuilt_history_end, trusted,
        "the rebuilt history ends where the landed batches end: the old \
         instance's unsubmitted rows never landed, and the queued direct waits \
         for the root frame"
    );
    assert_eq!(
        era.first_queued_input, queued_input,
        "the direct that arrived while the old instance was down is queued"
    );
    assert!(
        era.stop_block >= queued_block,
        "the queued direct is safe at the stop block"
    );
    assert_eq!(
        era.next_batch_nonce, 2,
        "two batches landed before the stop block"
    );
    let rebuilt = runtime.rebuilt_history_rows()?;
    assert_eq!(
        rebuilt.executed,
        trusted - base_count,
        "every rebuilt row past the base executed"
    );
    assert_eq!(runtime.canonical_divergence()?, None);

    let sequencer = runtime.sequencer_client()?;
    let info = sequencer.history(None, None).await?;
    assert_eq!(info.history.available_from.get(), base_count);
    assert_eq!(info.baseline.rebuilt_history_end.get(), trusted);
    assert_eq!(info.baseline.l1_stop_block, era.stop_block);
    assert_eq!(info.baseline.next_batch_nonce, 2);
    let checkpoint = info
        .accepted_checkpoint
        .ok_or("the era's baseline is its first comparison checkpoint")?;
    assert_eq!(
        (
            checkpoint.inclusion_block,
            checkpoint.executed_input_count.get(),
            checkpoint.next_batch_nonce
        ),
        (era.stop_block, trusted, 2)
    );
    assert_ne!(
        info.history.version.era_id, old.claim.version.era_id,
        "setup starts a new era"
    );

    // ── The client's playbook.
    assert!(matches!(
        sequencer.subscribe(old.claim).await,
        Err(SubscribeError::History(
            HistoryPolicyError::EraChanged { .. }
        ))
    ));
    // The sealing pump overshoots the batch size target, so the old instance
    // served rows past its last landed batch; a client at its head cannot
    // adopt the new era there.
    assert!(
        old.head > trusted,
        "the old instance served rows past its last landed batch: head {}, last accepted {trusted}",
        old.head
    );
    assert!(matches!(
        sequencer
            .adopt_current_era(ExecutedInputCount::new(old.head))
            .await,
        Err(AdoptEraError::Adopt(AdoptError::PastRebuiltEnd { .. }))
    ));
    if base_count > 0 {
        assert!(matches!(
            sequencer
                .adopt_current_era(ExecutedInputCount::new(base_count - 1))
                .await,
            Err(AdoptEraError::Adopt(AdoptError::BelowBase { .. }))
        ));
    }
    check_rebuilt_rows(runtime, &client, base_count, trusted).await?;

    let balance_before = client.app.current_user_balance(alice_address);
    let mut ws = runtime.ws_adopting(trusted).await?;
    let drained = ws
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;
    assert_eq!(
        drained.offset(),
        trusted,
        "the root frame drains the queued direct first"
    );
    client.apply(drained)?;
    assert_eq!(
        client.app.current_user_balance(alice_address),
        balance_before + queued_amount
    );

    // Live rows follow; the client signs on from its own nonce.
    let mut alice_l2 = runtime.wallet_l2(alice)?;
    alice_l2.set_next_nonce(client.app.current_user_nonce(alice_address));
    let bob_before = client.app.current_user_balance(bob_address);
    alice_l2
        .transfer(bob_address, U256::from(7_000_u64))
        .await?;
    let live = ws.expect_user_op_from(alice_address).await?;
    assert_eq!(live.offset(), trusted + 1);
    client.apply(live)?;
    assert_eq!(
        client.app.current_user_balance(bob_address),
        bob_before + U256::from(7_000_u64)
    );
    ws.expect_no_message_for(NO_WS_MESSAGE_WAIT).await?;

    if base == RecoveryBase::StoredMachine {
        // The watchdog compares the era's baseline on its next tick.
        let watchdog = Watchdog::new(machine_image(DEVNET_MACHINE_IMAGE)?, 0)?;
        watchdog.expect(runtime, &["init"], 0).await?;
        watchdog.expect(runtime, &["tick"], 0).await?;
        let status = watchdog.status(runtime).await?;
        let agreed = head_block(&status).ok_or("status without a head")?;
        if agreed < era.stop_block || status["last_tick"]["outcome"] != "agreed" {
            return Err(format!(
                "expected the watchdog to agree with the baseline at block {}: {status}",
                era.stop_block
            )
            .into());
        }
    }
    Ok(())
}

/// Seal the open batch by size, land it, and wait for its acceptance.
/// Returns the accepted checkpoint's inclusion block and count.
async fn land_a_batch(
    runtime: &ManagedSequencer,
    ws: &mut WsClient,
    client: &mut WalletClient,
    alice_l2: &mut WalletL2Client,
    alice_address: Address,
) -> ScenarioResult<(u64, u64)> {
    let floor = runtime.finalized_inclusion_block().await?.unwrap_or(0);
    let sealed_before = runtime.count_batches()?.sealed;
    for _ in 0..TRANSFERS_TO_FORCE_BATCH_CLOSE {
        alice_l2.transfer(alice_address, U256::from(1_u64)).await?;
        client.apply(ws.expect_user_op_from(alice_address).await?)?;
    }
    if runtime.count_batches()?.sealed <= sealed_before {
        return Err("the transfers did not seal a batch".into());
    }
    let block = mine_until_finalized_advances(runtime, floor).await?;
    let checkpoint = runtime
        .sequencer_client()?
        .history(None, None)
        .await?
        .accepted_checkpoint
        .ok_or("a landed batch is an accepted checkpoint")?;
    if checkpoint.inclusion_block != block {
        return Err(format!(
            "/history reports checkpoint block {}, /finalized_state {block}",
            checkpoint.inclusion_block
        )
        .into());
    }
    Ok((block, checkpoint.executed_input_count.get()))
}

/// Mine single blocks until Anvil's safe head covers `block`.
async fn mine_until_safe(runtime: &ManagedSequencer, block: u64) -> ScenarioResult<()> {
    for _ in 0..SAFE_HEAD_MINE_ATTEMPTS {
        if runtime.l1_safe_block_number().await? >= block {
            return Ok(());
        }
        runtime.mine_live_l1_blocks(1).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("Anvil's safe head never reached block {block}").into())
}

/// A canonical machine stopped after every input through `block`, as an
/// operator sources one: replayed from the template image by the watchdog.
async fn stored_machine_at(runtime: &ManagedSequencer, block: u64) -> ScenarioResult<PathBuf> {
    let image = machine_image(DEVNET_MACHINE_IMAGE)?;
    let out = tempfile::tempdir()
        .map_err(|err| format!("stored machine dir: {err}"))?
        .keep()
        .join("base");
    let watchdog = Watchdog::new(image.clone(), 0)?;
    let stdout = watchdog
        .expect(
            runtime,
            &[
                "replay",
                "--from",
                &image.to_string_lossy(),
                "--from-block",
                "0",
                "--to-block",
                &block.to_string(),
                "--out",
                &out.to_string_lossy(),
            ],
            0,
        )
        .await?;
    let replay: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|err| format!("replay JSON: {err}: {stdout}"))?;
    if !replay["stop"].is_null() {
        return Err(format!("the canonical machine stopped while replaying: {replay}").into());
    }
    Ok(out)
}

/// The client's optional row check: the new era serves, from the base to the
/// client's count, exactly the rows the client kept from the old era.
async fn check_rebuilt_rows(
    runtime: &ManagedSequencer,
    client: &WalletClient,
    base_count: u64,
    trusted: u64,
) -> ScenarioResult<()> {
    let sequencer: SequencerClient = runtime.sequencer_client()?;
    let claim = sequencer
        .adopt_current_era(ExecutedInputCount::new(base_count))
        .await?;
    let mut ws = WsClient::connect(&sequencer, claim).await?;
    for count in base_count..trusted {
        let served = serde_json::to_value(ws.next_message().await?)?;
        let kept = serde_json::to_value(
            client
                .rows
                .get(usize::try_from(count)?)
                .ok_or("the client kept no row at this count")?,
        )?;
        if served != kept {
            return Err(
                format!("row {count} differs between eras: served {served}, kept {kept}").into(),
            );
        }
    }
    Ok(())
}
