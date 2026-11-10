// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! An engine built against another header is refused before it is asked anything else. A binary
//! of its own, because the host caches the version check for the life of the process.

use std::ffi::c_char;
use std::path::Path;

use c_app_engine::{Application, EngineApp, sys};
use sequencer_core::application::AppError;
use sequencer_core::history::ExecutedInputCount;

const REPORTED: u32 = 1;

#[unsafe(no_mangle)]
extern "C" fn application_engine_abi_version() -> u32 {
    REPORTED
}

// Every other entry point aborts the test binary if reached: a panic cannot unwind out of
// an extern "C" function.

#[unsafe(no_mangle)]
extern "C" fn application_engine_max_method_payload_bytes() -> u64 {
    panic!("max_method_payload_bytes reached past the version check")
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_scheduler_limits(_: *mut sys::ApplicationEngineSchedulerLimits) {
    panic!("scheduler_limits reached past the version check")
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_state_file_in_dump(_: *const c_char) -> *const c_char {
    panic!("state_file_in_dump reached past the version check")
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_from_dump(
    _: *const c_char,
    _: *mut *mut sys::ApplicationEngine,
) -> i32 {
    panic!("from_dump reached past the version check")
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_from_canonical_state(
    _: *const c_char,
    _: u64,
    _: *mut *mut sys::ApplicationEngine,
) -> i32 {
    panic!("from_canonical_state reached past the version check")
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_destroy(_: *mut sys::ApplicationEngine) {
    panic!("destroy reached without a handle")
}

#[unsafe(no_mangle)]
extern "C" fn application_engine_get_last_error_message() -> *const c_char {
    panic!("get_last_error_message reached past the version check")
}

fn names_the_version(reason: &str) -> bool {
    reason.contains(&format!("ABI version {REPORTED}"))
        && reason.contains(&format!("version {}", sys::APPLICATION_ENGINE_ABI_VERSION))
}

#[test]
fn an_engine_of_another_abi_version_is_asked_nothing_else() {
    // The constructors return the refusal; the infallible getters can only panic with it.
    for result in [
        EngineApp::from_dump(Path::new("/nonexistent/dump")),
        EngineApp::from_canonical_state(Path::new("/nonexistent/state"), ExecutedInputCount::ZERO),
    ] {
        assert!(matches!(
            result,
            Err(AppError::Internal { reason }) if names_the_version(&reason)
        ));
    }

    let getters: [fn(); 3] = [
        || {
            let _ = EngineApp::scheduler_limits();
        },
        || {
            let _ = EngineApp::max_method_payload_bytes();
        },
        || {
            let _ = EngineApp::state_file_in_dump(Path::new("/nonexistent/dump"));
        },
    ];
    for getter in getters {
        let payload = std::panic::catch_unwind(getter).expect_err("a getter must refuse");
        let message = payload
            .downcast_ref::<String>()
            .expect("the refusal is a formatted message");
        assert!(names_the_version(message), "{message}");
    }
}
