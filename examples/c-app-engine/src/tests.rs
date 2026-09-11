// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use super::*;

// Only the drain seam is needed here; wallet integration tests cover execution and dumps.
// Every non-empty output borrows the same allocation, overwritten by the next drain.
#[derive(Default)]
struct OutputEngine {
    drained: usize,
    payload: [u8; 4],
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
unsafe extern "C" fn application_engine_destroy(engine: *mut sys::ApplicationEngine) {
    // SAFETY: EngineApp calls this once for the allocation transferred by the test.
    drop(unsafe { Box::from_raw(engine.cast::<OutputEngine>()) });
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_get_last_error_message() -> *const std::ffi::c_char {
    c"output fixture exhausted".as_ptr()
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
