// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use app_core::application::{
    Method, Transfer, WALLET_SCHEDULER_LIMITS, WalletApp, WalletConfig, Withdrawal,
};
use c_app_engine::{Application, EngineApp, sys};
use rollups_types::Erc20Deposit;
use sequencer_core::application::{
    Admission, AppError, AppOutput, CanonicalState, Executed, PolicyRefusal, SkipReason,
    check_live_user_op, execute,
};
use sequencer_core::fee::fee_to_linear;
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
use sequencer_core::user_op::UserOp;
use ssz::Encode;
use std::os::unix::ffi::OsStringExt;
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

fn deposit(config: WalletConfig, recipient: Address, amount: u64, index: u64) -> L2Tx {
    L2Tx::Direct(Direct {
        input_index: index,
        sender: config.erc20_portal_address,
        block: 7 + index,
        timestamp: 1_700_000_000 + index,
        payload: Erc20Deposit {
            token: config.supported_erc20_token,
            sender: recipient,
            value: U256::from(amount),
            exec_layer_data: Vec::new(),
        }
        .encode(),
    })
}

fn user_op(nonce: u32, max_fee: u16, data: Vec<u8>) -> UserOp {
    UserOp {
        nonce,
        max_fee,
        data: data.into(),
    }
}

/// Run one transaction through the shared executor on both sides and demand the same fate.
fn execute_both(bridge: &mut EngineApp, native: &mut WalletApp, tx: &L2Tx) -> Executed {
    let actual = execute(bridge, tx).unwrap();
    assert_eq!(actual, execute(native, tx).unwrap());
    assert_eq!(bridge.executed_input_count(), native.executed_input_count());
    actual
}

#[test]
fn abi_version_and_handle_free_getters_match_the_header_and_native() {
    assert_eq!(
        c_wallet_engine::application_engine_abi_version(),
        sys::APPLICATION_ENGINE_ABI_VERSION
    );
    assert_eq!(
        EngineApp::max_method_payload_bytes(),
        WalletApp::max_method_payload_bytes()
    );
    assert_eq!(EngineApp::scheduler_limits(), WalletApp::scheduler_limits());
    assert_eq!(EngineApp::scheduler_limits(), WALLET_SCHEDULER_LIMITS);
}

#[test]
fn abi_mixed_history_matches_native_outputs_count_and_dump() {
    let (dir, mut bridge, mut native, config) = fixture();
    assert_eq!(bridge.executed_input_count(), ExecutedInputCount::new(0));
    let sender = Address::repeat_byte(0x11);
    let recipient = Address::repeat_byte(0x22);
    for direct in [
        deposit(config, sender, 1000, 0),
        L2Tx::Direct(Direct {
            input_index: 1,
            sender: config.erc20_portal_address,
            block: 4,
            timestamp: 3,
            payload: vec![0xff],
        }),
        L2Tx::Direct(Direct {
            input_index: 2,
            sender,
            block: 9,
            timestamp: 5,
            payload: vec![],
        }),
    ] {
        execute_both(&mut bridge, &mut native, &direct);
    }
    assert_eq!(bridge.executed_input_count().get(), 3);

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
        let op = user_op(nonce, 10, data);
        assert_eq!(bridge.user_nonce(sender).unwrap(), nonce);
        assert_eq!(
            bridge.user_nonce(sender).unwrap(),
            native.user_nonce(sender).unwrap()
        );
        assert_eq!(
            bridge.admit_user_op(sender, &op, 10).unwrap(),
            Admission::Admit
        );
        assert_eq!(check_live_user_op(&bridge, sender, &op, 10).unwrap(), None);
        assert_eq!(check_live_user_op(&native, sender, &op, 10).unwrap(), None);

        let tx = L2Tx::UserOp(UserOpTx::new(sender, &op, 10, block));
        let Executed::Done { outputs, .. } = execute_both(&mut bridge, &mut native, &tx) else {
            panic!("an op at its sender's next nonce was skipped")
        };
        match nonce {
            0 => assert!(matches!(outputs.as_slice(), [AppOutput::Notice(_)])),
            1 => assert!(
                matches!(outputs.as_slice(), [AppOutput::Voucher { value, .. }] if *value == U256::ZERO)
            ),
            2 => assert!(outputs.is_empty(), "malformed method is an executed no-op"),
            _ => unreachable!(),
        }
    }
    assert_eq!(bridge.executed_input_count().get(), 6);
    assert_eq!(bridge.user_nonce(sender).unwrap(), 3);
    assert_eq!(bridge.user_nonce(recipient).unwrap(), 0);
    let checkpoint = dir.path().join("checkpoint");
    assert_eq!(
        state_bytes(&mut bridge, &checkpoint),
        native.canonical_snapshot_bytes().unwrap()
    );
    let restored = EngineApp::from_dump(&checkpoint).unwrap();
    assert_eq!(
        restored.executed_input_count(),
        bridge.executed_input_count()
    );
    assert_eq!(restored.user_nonce(sender).unwrap(), 3);
}

#[test]
fn abi_wrong_nonce_is_skipped_leaving_state_and_count_unchanged() {
    let (dir, mut bridge, mut native, config) = fixture();
    let sender = Address::repeat_byte(0x11);
    execute_both(&mut bridge, &mut native, &deposit(config, sender, 20, 0));
    let before = state_bytes(&mut bridge, &dir.path().join("before"));

    let op = user_op(9, 10, vec![0xff]);
    let tx = L2Tx::UserOp(UserOpTx::new(sender, &op, 10, 99));
    assert_eq!(
        execute_both(&mut bridge, &mut native, &tx),
        Executed::Skipped(SkipReason::WrongNonce {
            expected: 0,
            got: 9
        })
    );
    assert_eq!(bridge.executed_input_count().get(), 1);
    assert_eq!(bridge.user_nonce(sender).unwrap(), 0);
    let after = state_bytes(&mut bridge, &dir.path().join("after"));
    assert_eq!(after, before);
    assert_eq!(after, native.canonical_snapshot_bytes().unwrap());
}

#[test]
fn abi_unfunded_op_is_refused_by_admission_yet_executes_and_counts() {
    let (dir, mut bridge, mut native, config) = fixture();
    let sequencer = config.sequencer_address;
    let partial = Address::repeat_byte(0x11);
    let unfunded = Address::repeat_byte(0x55);
    execute_both(&mut bridge, &mut native, &deposit(config, partial, 20, 0));
    let fee = 10_000;
    let required = fee_to_linear(fee);
    assert!(required > U256::from(20));

    for (offset, (who, available)) in [(partial, U256::from(20)), (unfunded, U256::ZERO)]
        .into_iter()
        .enumerate()
    {
        let op = user_op(0, fee, vec![0xff]);
        let refusal = Admission::Refuse(PolicyRefusal::Underfunded {
            required,
            available,
        });
        assert_eq!(bridge.admit_user_op(who, &op, fee).unwrap(), refusal);
        assert_eq!(native.admit_user_op(who, &op, fee).unwrap(), refusal);

        // Admission is the operator's policy only. A batch carrying the op still runs it,
        // charging what the sender has, and both the nonce and the count advance.
        let tx = L2Tx::UserOp(UserOpTx::new(who, &op, fee, 99));
        assert_eq!(
            execute_both(&mut bridge, &mut native, &tx),
            Executed::Done {
                offset: ExecutedInputCount::new(1 + offset as u64),
                outputs: vec![],
            }
        );
        assert_eq!(bridge.user_nonce(who).unwrap(), 1);
        assert_eq!(native.current_user_balance(who), U256::ZERO);
    }
    assert_eq!(bridge.executed_input_count().get(), 3);
    assert_eq!(native.current_user_balance(sequencer), U256::from(20));
    assert_eq!(
        state_bytes(&mut bridge, &dir.path().join("charged")),
        native.canonical_snapshot_bytes().unwrap()
    );
}

#[test]
fn abi_restored_instances_and_checkpoints_are_independent() {
    let (dir, mut first, _, config) = fixture();
    let source = dir.path().join("genesis");
    let mut second = EngineApp::from_dump(&source).unwrap();
    let original = std::fs::read(EngineApp::state_file_in_dump(&source)).unwrap();
    let input = deposit(config, Address::repeat_byte(0x11), 10, 0);
    let next = deposit(config, Address::repeat_byte(0x11), 10, 1);
    execute(&mut first, &input).unwrap();
    assert_eq!(second.executed_input_count().get(), 0);
    assert_eq!(
        std::fs::read(EngineApp::state_file_in_dump(&source)).unwrap(),
        original
    );
    let frozen = dir.path().join("frozen");
    let frozen_bytes = state_bytes(&mut first, &frozen);
    execute(&mut first, &next).unwrap();
    assert_eq!(
        std::fs::read(EngineApp::state_file_in_dump(&frozen)).unwrap(),
        frozen_bytes
    );
    std::fs::remove_dir_all(&source).unwrap();
    execute(&mut second, &input).unwrap();
    assert_eq!(
        state_bytes(&mut second, &dir.path().join("second")),
        frozen_bytes
    );
    drop(first);
    let loaded = EngineApp::from_dump(&frozen).unwrap();
    assert_eq!(loaded.executed_input_count().get(), 1);
}

#[test]
fn abi_from_canonical_state_matches_native_and_classifies_bad_files() {
    let (dir, _, mut native, config) = fixture();
    let sender = Address::repeat_byte(0x11);
    execute(&mut native, &deposit(config, sender, 500, 0)).unwrap();
    let op = user_op(
        0,
        10,
        Method::Withdrawal(Withdrawal {
            amount: U256::from(5),
        })
        .as_ssz_bytes(),
    );
    execute(
        &mut native,
        &L2Tx::UserOp(UserOpTx::new(sender, &op, 10, 8)),
    )
    .unwrap();
    let count = native.executed_input_count();
    let machine = dir.path().join("machine");
    native.create_dump(&machine).unwrap();
    let state_file = WalletApp::state_file_in_dump(&machine);

    let mut bridge = EngineApp::from_canonical_state(&state_file, count).unwrap();
    assert_eq!(bridge.executed_input_count(), count);
    assert_eq!(bridge.user_nonce(sender).unwrap(), 1);
    assert_eq!(
        state_bytes(&mut bridge, &dir.path().join("projected")),
        native.canonical_snapshot_bytes().unwrap()
    );
    // The source stays the caller's: the instance outlives it and evolves as native does.
    std::fs::remove_dir_all(&machine).unwrap();
    let next = deposit(config, sender, 1, 2);
    assert_eq!(
        execute(&mut bridge, &next).unwrap(),
        execute(&mut native, &next).unwrap()
    );
    assert_eq!(
        state_bytes(&mut bridge, &dir.path().join("continued")),
        native.canonical_snapshot_bytes().unwrap()
    );

    let rewritten = dir.path().join("rewritten");
    native.create_dump(&rewritten).unwrap();
    let state_file = WalletApp::state_file_in_dump(&rewritten);
    let wrong_count = ExecutedInputCount::new(native.executed_input_count().get() + 1);
    let garbage = dir.path().join("garbage");
    std::fs::write(&garbage, [0xff]).unwrap();
    for (file, count) in [(state_file.as_path(), wrong_count), (&garbage, count)] {
        assert!(matches!(
            EngineApp::from_canonical_state(file, count),
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));
    }
    assert!(matches!(
        EngineApp::from_canonical_state(&dir.path().join("missing"), count),
        Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));
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

/// An unpayable fee no longer fails an execution, so the fatal trigger is the engine's refusal
/// to run over undrained outputs. `EngineApp` always drains, so this drives the export directly;
/// the adapter's unit tests map INTERNAL_ERROR to `AppError::Internal`.
#[test]
fn abi_execution_over_queued_outputs_is_refused_as_internal_error() {
    let (dir, _, _, config) = fixture();
    let genesis =
        std::ffi::CString::new(dir.path().join("genesis").into_os_string().into_vec()).unwrap();
    let mut engine = std::ptr::null_mut();
    assert_eq!(
        unsafe { c_wallet_engine::application_engine_from_dump(genesis.as_ptr(), &mut engine) },
        sys::APPLICATION_ENGINE_STATUS_OK
    );
    let payload = Erc20Deposit {
        token: config.supported_erc20_token,
        sender: Address::repeat_byte(0x11),
        value: U256::from(10),
        exec_layer_data: Vec::new(),
    }
    .encode();
    let input = sys::ApplicationEngineDirectInput {
        sender: sys::ApplicationEngineEthereumAddress {
            bytes: config.erc20_portal_address.into_array(),
        },
        input_index: 0,
        block_number: 7,
        timestamp: 1_700_000_000,
        payload: sys::ApplicationEngineByteSpan {
            data: payload.as_ptr(),
            size: payload.len() as u64,
        },
    };
    let mut outputs = 0;
    let run = |engine, outputs: &mut u64| unsafe {
        c_wallet_engine::application_engine_execute_direct_input(engine, &input, outputs)
    };
    assert_eq!(run(engine, &mut outputs), sys::APPLICATION_ENGINE_STATUS_OK);
    assert_eq!(outputs, 1, "a supported deposit emits its notice");

    let mut untouched = u64::MAX;
    assert_eq!(
        run(engine, &mut untouched),
        sys::APPLICATION_ENGINE_STATUS_INTERNAL_ERROR
    );
    assert_eq!(untouched, u64::MAX, "the count is written only on OK");
    let message = unsafe {
        std::ffi::CStr::from_ptr(c_wallet_engine::application_engine_get_last_error_message())
    };
    assert!(message.to_str().unwrap().contains("still queued"));
    assert_eq!(
        unsafe { c_wallet_engine::application_engine_l2_tx_count(engine) },
        1,
        "the refused execution ran nothing"
    );
    unsafe { c_wallet_engine::application_engine_destroy(engine) };
}
