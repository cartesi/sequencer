// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Setup's base loader against real machines: `read_machine_export` must read
//! the template image as genesis, and a machine advanced and stored as exactly
//! what a native `CanonicalRunner` over the same inputs exports, without
//! changing the stored directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use app_core::application::{
    DEVNET_SEQUENCER_ADDRESS, Method, Transfer, WALLET_SCHEDULER_LIMITS, WalletApp, WalletConfig,
    Withdrawal,
};
use canonical_app::{STATE_INSPECT_QUERY, SchedulerConfig};
use rollups_types::Erc20Deposit;
use rollups_types::alloy_primitives::{Address, U256, address};
use sequencer::machine::read_machine_export;
use sequencer_core::application::{Application, CanonicalState};
use sequencer_core::batch::{Batch, Frame, WireUserOp};
use sequencer_core::history::L2TxCount;
use sequencer_core::scheduler::{CanonicalRunner, L1Input, SchedulerExport, TxFate};
use testsi::{InputBuilder, TestResult};

use crate::{
    TEST_CHAIN_ID, TEST_DAPP_ADDRESS, address_from_signing_key, devnet_machine, machine_image_path,
    signed_user_op, signing_key,
};

const BOB: Address = address!("0x8888888888888888888888888888888888888888");

/// The template image is the genesis base: six zeros and the genesis wallet,
/// and reading it leaves every file of the image as it was.
#[testsi::test_dapp(kind("export"))]
pub fn export_of_the_template_image_is_genesis() -> TestResult {
    let image = machine_image_path();
    let before = directory_listing(&image);
    let scratch = tempfile::tempdir()?;
    let state_file = scratch.path().join("state");

    let export = read_machine_export(&image, &state_file)?;

    assert_eq!(export, SchedulerExport::decode(&[0; 48])?);
    let genesis = WalletApp::new(WalletConfig::devnet());
    assert_eq!(
        std::fs::read(&state_file)?,
        genesis.canonical_snapshot_bytes()?
    );
    let projected = WalletApp::from_canonical_state(&state_file, L2TxCount::ZERO)?;
    assert_eq!(
        projected.canonical_snapshot_bytes()?,
        genesis.canonical_snapshot_bytes()?
    );
    assert_eq!(directory_listing(&image), before, "the image was written");
    Ok(())
}

/// A machine advanced through a mixed run and stored exports exactly what the
/// native runner holds after the same inputs, and the wallet accepts the
/// state at the exported count, and no other.
#[testsi::test_dapp(kind("export"))]
pub fn export_of_a_stored_machine_matches_the_native_runner() -> TestResult {
    let inputs = mixed_run();
    let mut runner = CanonicalRunner::genesis(
        WalletApp::new(WalletConfig::devnet()),
        SchedulerConfig::for_application::<WalletApp>(DEVNET_SEQUENCER_ADDRESS),
    );
    let mut machine = devnet_machine()?;
    let mut skipped = 0;
    for (index, input) in inputs.iter().enumerate() {
        let native = runner
            .process_input(input.l1_input(index))
            .expect("the native run never fails");
        skipped += native
            .txs
            .iter()
            .filter(|tx| matches!(tx.fate, TxFate::Skipped(_)))
            .count();
        let (outputs, _reports) = machine.advance_state(input.to_machine())?;
        assert_eq!(
            outputs.list().len(),
            native.outputs.len(),
            "input {index}: the machine and the native runner disagree"
        );
    }
    let expected = runner.export();
    // The run must cover what a base can hold: directs still queued, a drained
    // one, executed ops and a skipped one.
    assert_eq!(skipped, 1, "the run skips one op");
    assert_eq!(
        expected,
        SchedulerExport {
            l1_input_count: 6,
            l2_tx_count: 3,
            next_batch_nonce: 1,
            clock: 11,
            first_queued_input: 3,
            fridge_len: 2,
        }
    );

    let scratch = tempfile::tempdir()?;
    let stored = scratch.path().join("machine");
    machine.store(&stored)?;
    drop(machine);
    let before = directory_listing(&stored);
    let state_file = scratch.path().join("state");

    let export = read_machine_export(&stored, &state_file)?;

    assert_eq!(export, expected);
    let native_state = runner
        .inspect(STATE_INSPECT_QUERY)
        .expect("the wallet encodes its state");
    assert_eq!(std::fs::read(&state_file)?, native_state);
    let count = L2TxCount::new(export.l2_tx_count);
    let projected = WalletApp::from_canonical_state(&state_file, count)?;
    assert_eq!(projected.canonical_snapshot_bytes()?, native_state);
    assert_eq!(projected.l2_tx_count(), count);
    let off_by_one = L2TxCount::new(export.l2_tx_count + 1);
    assert!(WalletApp::from_canonical_state(&state_file, off_by_one).is_err());

    // A second read finds the same machine: the first one changed nothing.
    let again = scratch.path().join("state-again");
    assert_eq!(read_machine_export(&stored, &again)?, expected);
    assert_eq!(std::fs::read(&again)?, native_state);
    assert_eq!(
        directory_listing(&stored),
        before,
        "the machine was written"
    );
    Ok(())
}

/// A directory that holds no machine is a load error, and no state is written.
#[testsi::test_dapp(kind("export"))]
pub fn export_of_a_missing_machine_is_a_load_error() -> TestResult {
    let scratch = tempfile::tempdir()?;
    let state_file = scratch.path().join("state");
    let err = read_machine_export(scratch.path(), &state_file).expect_err("no machine there");
    assert!(
        matches!(err, sequencer::machine::MachineExportError::Load { .. }),
        "{err}"
    );
    assert!(!state_file.exists());
    Ok(())
}

/// Six inputs. The first batch drains Alice's deposit by its frame, runs her
/// transfer, skips its replay and runs her withdrawal; the over-cap deposit
/// never joins; a rejected batch changes nothing; two deposits above the
/// clock stay queued.
fn mixed_run() -> Vec<Step> {
    let token = WalletConfig::devnet().supported_erc20_token;
    let alice_key = signing_key(41);
    let alice = address_from_signing_key(&alice_key);
    let carol = address!("0xc0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0");
    let cap = usize::try_from(WALLET_SCHEDULER_LIMITS.max_direct_payload_bytes)
        .expect("the cap fits usize");
    let over_cap = vec![0_u8; cap + 1 - rollups_types::ERC20_DEPOSIT_PREFIX_BYTES];
    let method = |method: Method| ssz::Encode::as_ssz_bytes(&method);
    let transfer = method(Method::Transfer(Transfer {
        amount: U256::from(100_u64),
        to: BOB,
    }));
    let ops: Vec<WireUserOp> = vec![
        signed_user_op(&alice_key, 0, transfer.clone()),
        signed_user_op(&alice_key, 0, transfer),
        signed_user_op(
            &alice_key,
            1,
            method(Method::Withdrawal(Withdrawal {
                amount: U256::from(50_u64),
            })),
        ),
    ];
    let batch = Batch {
        nonce: 0,
        frames: vec![Frame {
            user_ops: ops,
            safe_block: 11,
            fee_price: 0,
        }],
    };
    vec![
        Step::deposit(10, 1_000, token, alice, 1_000, Vec::new()),
        Step::deposit(10, 1_000, token, carol, 7, over_cap),
        Step::submitter(12, 1_024, ssz::Encode::as_ssz_bytes(&batch)),
        Step::deposit(12, 1_024, token, carol, 77, Vec::new()),
        Step::submitter(13, 1_036, vec![0xff, 0xee, 0xdd]),
        Step::deposit(13, 1_036, token, alice, 5, Vec::new()),
    ]
}

/// One input, as both the machine and the native runner receive it.
struct Step {
    sender: Address,
    block: u64,
    timestamp: u64,
    payload: Vec<u8>,
}

impl Step {
    fn deposit(
        block: u64,
        timestamp: u64,
        token: Address,
        owner: Address,
        amount: u64,
        exec_layer_data: Vec<u8>,
    ) -> Self {
        let payload = Erc20Deposit {
            token,
            sender: owner,
            value: U256::from(amount),
            exec_layer_data,
        }
        .encode();
        Self {
            sender: WalletConfig::devnet().erc20_portal_address,
            block,
            timestamp,
            payload,
        }
    }

    fn submitter(block: u64, timestamp: u64, payload: Vec<u8>) -> Self {
        Self {
            sender: DEVNET_SEQUENCER_ADDRESS,
            block,
            timestamp,
            payload,
        }
    }

    fn l1_input(&self, index: usize) -> L1Input {
        L1Input {
            index: u64::try_from(index).expect("the index fits u64"),
            sender: self.sender,
            block: self.block,
            timestamp: self.timestamp,
            chain_id: TEST_CHAIN_ID,
            app_contract: TEST_DAPP_ADDRESS,
            payload: self.payload.clone(),
        }
    }

    fn to_machine(&self) -> InputBuilder {
        let block = usize::try_from(self.block).expect("the block fits usize");
        let timestamp = usize::try_from(self.timestamp).expect("the timestamp fits usize");
        InputBuilder::from_address(self.sender)
            .at_block(block)
            .with_block_timestamp(timestamp)
            .with_payload(&self.payload)
    }
}

/// Every file under `dir`, with its length and modification time: a store,
/// a shared load or any write moves one of them.
fn directory_listing(dir: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    let mut listing = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).expect("read the machine directory") {
            let entry = entry.expect("read a directory entry");
            let metadata = entry.metadata().expect("read an entry's metadata");
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                let modified = metadata.modified().expect("read a modification time");
                listing.insert(entry.path(), (metadata.len(), modified));
            }
        }
    }
    listing
}
