// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use app_core::application::{Method, Transfer, WalletApp, WalletConfig, Withdrawal};
use c_app_engine::{Application, EngineApp};
use c_wallet_engine as _;
use sequencer_core::application::{
    AppError, AppOutput, CanonicalState, ExecutionOutcome, execute_direct_input,
    execute_valid_user_op, validate_and_execute_user_op,
};
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
use sequencer_core::user_op::UserOp;
use ssz::Encode;
use std::path::Path;

fn fixture() -> (tempfile::TempDir, EngineApp, WalletApp, WalletConfig) {
    let dir = tempfile::tempdir().unwrap();
    let config = WalletConfig::default();
    let genesis = dir.path().join("genesis");
    c_wallet_engine::write_genesis(&genesis, config).unwrap();
    let bridge = EngineApp::from_dump(&genesis).unwrap();
    let native = WalletApp::from_dump(&genesis).unwrap();
    (dir, bridge, native, config)
}

fn state_bytes(app: &mut EngineApp, path: &Path) -> Vec<u8> {
    app.create_dump(path).unwrap();
    std::fs::read(EngineApp::state_file_in_dump(path)).unwrap()
}

fn deposit(config: WalletConfig, recipient: Address, amount: u64, block: u64) -> DirectInput {
    let mut payload = Vec::new();
    payload.extend_from_slice(config.supported_erc20_token.as_slice());
    payload.extend_from_slice(recipient.as_slice());
    payload.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
    DirectInput {
        sender: config.erc20_portal_address,
        block_number: block,
        payload,
    }
}

#[test]
fn abi_mixed_history_matches_native_outputs_progress_and_dump() {
    let (dir, mut bridge, mut native, config) = fixture();
    let sender = Address::repeat_byte(0x11);
    let recipient = Address::repeat_byte(0x22);
    for direct in [
        deposit(config, sender, 1000, 7),
        DirectInput {
            sender: config.erc20_portal_address,
            block_number: 4,
            payload: vec![0xff],
        },
        DirectInput {
            sender,
            block_number: 9,
            payload: vec![],
        },
    ] {
        assert_eq!(
            execute_direct_input(&mut bridge, &direct).unwrap(),
            execute_direct_input(&mut native, &direct).unwrap()
        );
        assert_eq!(bridge.progress(), native.progress());
    }
    assert_eq!(bridge.progress().executed_input_count().get(), 3);
    assert_eq!(bridge.progress().last_executed_safe_block(), 9);

    for (nonce, data, block) in [
        (
            0,
            Method::Transfer(Transfer {
                amount: U256::from(100),
                to: recipient,
            })
            .as_ssz_bytes(),
            12,
        ),
        (
            1,
            Method::Withdrawal(Withdrawal {
                amount: U256::from(50),
            })
            .as_ssz_bytes(),
            10,
        ),
        (2, vec![0xff], 13),
    ] {
        let op = UserOp {
            nonce,
            max_fee: 10,
            data: data.into(),
        };
        let actual = validate_and_execute_user_op(&mut bridge, sender, &op, 10, block).unwrap();
        let expected = validate_and_execute_user_op(&mut native, sender, &op, 10, block).unwrap();
        assert_eq!(actual, expected);
        let ExecutionOutcome::Included(receipt) = actual else {
            panic!("funded operation was rejected")
        };
        match nonce {
            0 => assert!(matches!(receipt.outputs.as_slice(), [AppOutput::Notice(_)])),
            1 => assert!(
                matches!(receipt.outputs.as_slice(), [AppOutput::Voucher { value, .. }] if *value == U256::ZERO)
            ),
            2 => assert!(
                receipt.outputs.is_empty(),
                "malformed method is an executed no-op"
            ),
            _ => unreachable!(),
        }
        assert_eq!(bridge.progress(), native.progress());
    }
    assert_eq!(bridge.progress().executed_input_count().get(), 6);
    assert_eq!(bridge.progress().last_executed_safe_block(), 13);
    let checkpoint = dir.path().join("checkpoint");
    assert_eq!(
        state_bytes(&mut bridge, &checkpoint),
        native.canonical_snapshot_bytes().unwrap()
    );
    let restored = EngineApp::from_dump(&checkpoint).unwrap();
    assert_eq!(restored.progress(), bridge.progress());
}

#[test]
fn abi_protocol_rejections_leave_state_and_progress_unchanged() {
    let (dir, mut bridge, mut native, config) = fixture();
    let sender = Address::repeat_byte(0x11);
    let direct = deposit(config, sender, 20, 7);
    execute_direct_input(&mut bridge, &direct).unwrap();
    execute_direct_input(&mut native, &direct).unwrap();
    for (who, nonce, max_fee, current_fee) in [
        (sender, 9, 10, 10),
        (sender, 0, 0, 10),
        (Address::repeat_byte(0x55), 0, 10_000, 10_000),
    ] {
        let op = UserOp {
            nonce,
            max_fee,
            data: vec![0xff].into(),
        };
        let before = bridge.progress();
        let actual = validate_and_execute_user_op(&mut bridge, who, &op, current_fee, 99).unwrap();
        assert_eq!(
            actual,
            validate_and_execute_user_op(&mut native, who, &op, current_fee, 99).unwrap()
        );
        assert!(matches!(actual, ExecutionOutcome::Invalid(_)));
        assert_eq!(bridge.progress(), before);
    }
    assert_eq!(
        state_bytes(&mut bridge, &dir.path().join("rejected")),
        native.canonical_snapshot_bytes().unwrap()
    );
}

#[test]
fn abi_restored_instances_and_checkpoints_are_independent() {
    let (dir, mut first, _, config) = fixture();
    let source = dir.path().join("genesis");
    let mut second = EngineApp::from_dump(&source).unwrap();
    let original = std::fs::read(EngineApp::state_file_in_dump(&source)).unwrap();
    let input = deposit(config, Address::repeat_byte(0x11), 10, 7);
    execute_direct_input(&mut first, &input).unwrap();
    assert_eq!(second.progress().executed_input_count().get(), 0);
    assert_eq!(
        std::fs::read(EngineApp::state_file_in_dump(&source)).unwrap(),
        original
    );
    let frozen = dir.path().join("frozen");
    let frozen_bytes = state_bytes(&mut first, &frozen);
    execute_direct_input(&mut first, &input).unwrap();
    assert_eq!(
        std::fs::read(EngineApp::state_file_in_dump(&frozen)).unwrap(),
        frozen_bytes
    );
    EngineApp::delete_dump(&source).unwrap();
    execute_direct_input(&mut second, &input).unwrap();
    assert_eq!(
        state_bytes(&mut second, &dir.path().join("second")),
        frozen_bytes
    );
    drop(first);
    let loaded = EngineApp::from_dump(&frozen).unwrap();
    assert_eq!(loaded.progress().executed_input_count().get(), 1);
}

#[test]
fn abi_dump_errors_preserve_missing_and_corrupt_classification() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        matches!(EngineApp::from_dump(&dir.path().join("missing")), Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
    );
    let corrupt = dir.path().join("corrupt");
    std::fs::create_dir(&corrupt).unwrap();
    std::fs::write(EngineApp::state_file_in_dump(&corrupt), [0xff]).unwrap();
    assert!(
        matches!(EngineApp::from_dump(&corrupt), Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData)
    );
}

#[test]
fn abi_malformed_dump_paths_remain_terminal_like_native_reads() {
    let dir = tempfile::tempdir().unwrap();
    let file_prefix = dir.path().join("file-prefix");
    std::fs::write(&file_prefix, []).unwrap();
    let directory_state = dir.path().join("directory-state");
    std::fs::create_dir(&directory_state).unwrap();
    std::fs::create_dir(WalletApp::state_file_in_dump(&directory_state)).unwrap();
    for prefix in [file_prefix, directory_state] {
        assert!(
            matches!(WalletApp::from_dump(&prefix), Err(AppError::Io(error))
            if matches!(error.kind(), std::io::ErrorKind::NotADirectory | std::io::ErrorKind::IsADirectory))
        );
        assert!(
            matches!(EngineApp::from_dump(&prefix), Err(AppError::Io(error))
            if error.kind() == std::io::ErrorKind::InvalidData)
        );
    }
}

#[test]
fn abi_execution_failure_returns_app_error_without_resuming_the_instance() {
    let (_dir, mut bridge, _, _) = fixture();
    let op = ValidUserOp {
        sender: Address::repeat_byte(0x11),
        fee: 10,
        data: vec![],
    };
    // Calling the already-validated boundary on an unfunded op is an application invariant fault.
    assert!(matches!(
        execute_valid_user_op(&mut bridge, &op, 7),
        Err(AppError::Internal { .. })
    ));
    drop(bridge);
}
