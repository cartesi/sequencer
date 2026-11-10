// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use super::*;

// This fixture covers output borrowing, the handle-free getters, admission's reason mapping and
// the records execution hands across; wallet tests cover what execution does, and dumps.
// Every non-empty output borrows the same allocation, overwritten by the next drain.
#[derive(Default)]
struct OutputEngine {
    drained: usize,
    payload: [u8; 4],
    /// The last records the execution entry points received, read back field by field.
    direct: Option<Direct>,
    user_op: Option<UserOpTx>,
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_abi_version() -> u32 {
    sys::APPLICATION_ENGINE_ABI_VERSION
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_max_method_payload_bytes() -> u64 {
    0
}

#[unsafe(no_mangle)]
unsafe extern "C" fn application_engine_scheduler_limits(
    out_limits: *mut sys::ApplicationEngineSchedulerLimits,
) {
    // SAFETY: EngineApp supplies a writable record.
    unsafe {
        *out_limits = sys::ApplicationEngineSchedulerLimits {
            fridge_bound: 3,
            max_direct_payload_bytes: u64::MAX,
        }
    };
}

#[test]
fn empty_method_payload_bound_comes_from_the_engine() {
    assert_eq!(EngineApp::max_method_payload_bytes(), 0);
}

#[test]
fn scheduler_limits_come_from_the_engine_at_full_width() {
    assert_eq!(
        EngineApp::scheduler_limits(),
        SchedulerLimits {
            fridge_bound: 3,
            max_direct_payload_bytes: u64::MAX,
        }
    );
}

#[test]
fn an_engine_of_another_abi_version_is_refused() {
    abi_version_matches(sys::APPLICATION_ENGINE_ABI_VERSION).unwrap();
    for reported in [0, 1, sys::APPLICATION_ENGINE_ABI_VERSION + 1, u32::MAX] {
        assert!(matches!(
            abi_version_matches(reported),
            Err(AppError::Internal { reason }) if reason.contains(&format!("version {reported}"))
        ));
    }
    check_abi_version().unwrap();
}

#[unsafe(no_mangle)]
unsafe extern "C" fn application_engine_drain_output(
    engine: *mut sys::ApplicationEngine,
    out_output: *mut sys::ApplicationEngineOutput,
) -> i32 {
    // SAFETY: the test transfers a boxed OutputEngine to EngineApp; it remains exclusively
    // owned until destroy. EngineApp supplies a writable output record for every call.
    let engine = unsafe { &mut *engine.cast::<OutputEngine>() };
    engine.payload = match engine.drained {
        0 => *b"one!",
        1 => [0, 0xff, 0x80, 0x42],
        _ => *b"last",
    };
    let (kind, values) = match engine.drained {
        0 | 3 => (
            sys::APPLICATION_ENGINE_OUTPUT_NOTICE,
            sys::ApplicationEngineOutputValues {
                notice: abi_span(&engine.payload),
            },
        ),
        1 => (
            sys::APPLICATION_ENGINE_OUTPUT_VOUCHER,
            sys::ApplicationEngineOutputValues {
                voucher: sys::ApplicationEngineVoucher {
                    destination: sys::ApplicationEngineEthereumAddress { bytes: [0x23; 20] },
                    value: sys::ApplicationEngineUint256 {
                        bytes: [
                            129, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
                            20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
                        ],
                    },
                    payload: abi_span(&engine.payload[..3]),
                },
            },
        ),
        2 => (
            sys::APPLICATION_ENGINE_OUTPUT_NOTICE,
            sys::ApplicationEngineOutputValues {
                notice: sys::ApplicationEngineByteSpan {
                    data: std::ptr::null(),
                    size: 0,
                },
            },
        ),
        _ => return sys::APPLICATION_ENGINE_STATUS_INTERNAL_ERROR,
    };
    engine.drained += 1;
    unsafe { *out_output = sys::ApplicationEngineOutput { kind, values } };
    sys::APPLICATION_ENGINE_STATUS_OK
}

#[unsafe(no_mangle)]
unsafe extern "C" fn application_engine_execute_direct_input(
    engine: *mut sys::ApplicationEngine,
    input: *const sys::ApplicationEngineDirectInput,
    out_output_count: *mut u64,
) -> i32 {
    // SAFETY: as for drain; EngineApp passes a live record and a writable count.
    let engine = unsafe { &mut *engine.cast::<OutputEngine>() };
    let input = unsafe { &*input };
    engine.direct = Some(Direct {
        input_index: input.input_index,
        sender: Address::from(input.sender.bytes),
        block: input.block_number,
        timestamp: input.timestamp,
        payload: payload_from(input.payload),
    });
    unsafe { *out_output_count = 0 };
    sys::APPLICATION_ENGINE_STATUS_OK
}

#[unsafe(no_mangle)]
unsafe extern "C" fn application_engine_execute_user_op(
    engine: *mut sys::ApplicationEngine,
    user_op: *const sys::ApplicationEngineUserOpTx,
    out_output_count: *mut u64,
) -> i32 {
    // SAFETY: as for drain; EngineApp passes a live record and a writable count.
    let engine = unsafe { &mut *engine.cast::<OutputEngine>() };
    let user_op = unsafe { &*user_op };
    engine.user_op = Some(UserOpTx {
        sender: Address::from(user_op.sender.bytes),
        nonce: user_op.nonce,
        fee: user_op.fee,
        block: user_op.safe_block,
        data: payload_from(user_op.data),
    });
    unsafe { *out_output_count = 0 };
    sys::APPLICATION_ENGINE_STATUS_OK
}

/// The wallet ignores a direct's index, block and timestamp and a user op's nonce and safe block,
/// so only an engine that records them shows a dropped or swapped field.
#[test]
fn execution_hands_every_field_to_the_engine() {
    let mut app = EngineApp {
        engine: Box::into_raw(Box::<OutputEngine>::default()).cast(),
    };
    let direct = Direct {
        input_index: 3,
        sender: Address::repeat_byte(0x41),
        block: 7,
        timestamp: 1_700_000_011,
        payload: vec![1, 2, 3],
    };
    let op = UserOpTx {
        sender: Address::repeat_byte(0x42),
        nonce: 5,
        fee: 300,
        block: 9,
        data: vec![4, 5],
    };
    assert_eq!(app.apply_direct(&direct).unwrap(), AppOutputs::new());
    assert_eq!(app.apply_user_op(&op).unwrap(), AppOutputs::new());

    // SAFETY: the handle is the OutputEngine boxed above, alive until `app` drops.
    let engine = unsafe { &*app.engine.cast::<OutputEngine>() };
    assert_eq!(engine.direct.as_ref(), Some(&direct));
    assert_eq!(engine.user_op.as_ref(), Some(&op));
}

#[unsafe(no_mangle)]
unsafe extern "C" fn application_engine_destroy(engine: *mut sys::ApplicationEngine) {
    // SAFETY: EngineApp calls this once for the allocation transferred by the test.
    drop(unsafe { Box::from_raw(engine.cast::<OutputEngine>()) });
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_get_last_error_message() -> *const std::ffi::c_char {
    c"fixture refuses: quota spent".as_ptr()
}

#[test]
fn drain_preserves_output_order_and_copies_reused_payloads() {
    let mut app = EngineApp {
        engine: Box::into_raw(Box::<OutputEngine>::default()).cast(),
    };
    let outputs = app.drain_outputs(4).unwrap();
    drop(app);

    assert_eq!(
        outputs,
        vec![
            AppOutput::Notice(b"one!".to_vec()),
            AppOutput::Voucher {
                destination: Address::repeat_byte(0x23),
                // Independent of the fixture's big-endian bytes: limbs are least-significant first.
                value: U256::from_limbs([
                    0x191a1b1c1d1e1f20,
                    0x1112131415161718,
                    0x090a0b0c0d0e0f10,
                    0x8102030405060708,
                ]),
                payload: vec![0, 0xff, 0x80],
            },
            AppOutput::Notice(vec![]),
            AppOutput::Notice(b"last".to_vec()),
        ]
    );
}

#[test]
fn lifecycle_statuses_are_fatal_outside_dump_lifecycle_calls() {
    for status in [
        sys::APPLICATION_ENGINE_STATUS_NOT_FOUND,
        sys::APPLICATION_ENGINE_STATUS_INVALID_DUMP,
    ] {
        assert!(matches!(
            check(status, "execute_direct_input"),
            Err(AppError::Internal { .. })
        ));
        assert!(matches!(
            check_lifecycle(status, "from_dump"),
            Err(AppError::Io(_))
        ));
    }
    assert!(matches!(
        check(sys::APPLICATION_ENGINE_STATUS_IO_ERROR, "drain_output"),
        Err(AppError::Io(_))
    ));
    // An execution refused over queued outputs reports INTERNAL_ERROR, which is fatal.
    assert!(matches!(
        check(
            sys::APPLICATION_ENGINE_STATUS_INTERNAL_ERROR,
            "execute_user_op"
        ),
        Err(AppError::Internal { .. })
    ));
}

fn refusal(reason: sys::ApplicationEngineInvalidReason) -> sys::ApplicationEngineInvalid {
    let mut required = [0; 32];
    required[0] = 0x80;
    required[31] = 7;
    sys::ApplicationEngineInvalid {
        reason,
        values: sys::ApplicationEngineInvalidValues {
            fee_balance: sys::ApplicationEngineInsufficientFeeBalance {
                required: sys::ApplicationEngineUint256 { bytes: required },
                available: sys::ApplicationEngineUint256 { bytes: [0xff; 32] },
            },
        },
    }
}

#[test]
fn admission_maps_the_reasons_admission_may_give() {
    let ignored = refusal(sys::APPLICATION_ENGINE_INVALID_NONCE);
    assert_eq!(
        admission_from(sys::APPLICATION_ENGINE_STATUS_OK, &ignored).unwrap(),
        Admission::Admit
    );
    assert_eq!(
        admission_from(
            sys::APPLICATION_ENGINE_STATUS_INVALID,
            &refusal(sys::APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE)
        )
        .unwrap(),
        Admission::Refuse(PolicyRefusal::Underfunded {
            required: (U256::from(1) << 255) + U256::from(7),
            available: U256::MAX,
        })
    );
    // The policy text is whatever the engine left in its last error message.
    assert_eq!(
        admission_from(
            sys::APPLICATION_ENGINE_STATUS_INVALID,
            &refusal(sys::APPLICATION_ENGINE_REFUSED_BY_POLICY)
        )
        .unwrap(),
        Admission::Refuse(PolicyRefusal::Other("fixture refuses: quota spent".into()))
    );
}

#[test]
fn admission_refuses_retired_and_unknown_reasons_and_lifecycle_statuses() {
    for reason in [
        sys::APPLICATION_ENGINE_INVALID_NONCE,
        sys::APPLICATION_ENGINE_INVALID_MAX_FEE,
        sys::APPLICATION_ENGINE_NONCE_EXHAUSTED,
        sys::APPLICATION_ENGINE_REFUSED_BY_POLICY + 1,
    ] {
        assert!(matches!(
            admission_from(sys::APPLICATION_ENGINE_STATUS_INVALID, &refusal(reason)),
            Err(AppError::Internal { reason: text }) if text.contains("unsupported invalid reason")
        ));
    }
    let ignored = refusal(sys::APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE);
    for status in [
        sys::APPLICATION_ENGINE_STATUS_INTERNAL_ERROR,
        sys::APPLICATION_ENGINE_STATUS_NOT_FOUND,
        sys::APPLICATION_ENGINE_STATUS_INVALID_DUMP,
    ] {
        assert!(matches!(
            admission_from(status, &ignored),
            Err(AppError::Internal { .. })
        ));
    }
    assert!(matches!(
        admission_from(sys::APPLICATION_ENGINE_STATUS_IO_ERROR, &ignored),
        Err(AppError::Io(_))
    ));
}
