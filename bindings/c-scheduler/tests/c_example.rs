// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Builds `c/feed_loop.c` against `libc_scheduler.a` with the system C compiler, runs it, and
//! compares what it prints with the Rust reference on the same inputs. It also parses the header
//! as C++, where its noexcept spelling lives.

use std::path::{Path, PathBuf};
use std::process::Command;

use alloy_primitives::Address;
use c_scheduler::sys;
use sequencer_core::batch::{Batch, Frame};
use sequencer_core::l2_tx::L2Tx;
use sequencer_core::scheduler::{InputOutcome, L1Input, Scheduler, SchedulerConfig};

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn include_args() -> [String; 2] {
    [
        format!("-I{MANIFEST_DIR}/include"),
        format!("-I{MANIFEST_DIR}/../c-app-engine/include"),
    ]
}

/// The newest `libc_scheduler-*.a` beside this test in `target/<profile>/deps`. Cargo builds the
/// staticlib there whenever it builds the library for this test, but copies it up to
/// `target/<profile>` only for a `cargo build` of the library itself, so the copy can be stale or
/// missing. Before the tests run, Cargo has rebuilt this test's own archive if a source changed,
/// so the newest one is built from the current sources.
fn static_library() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let deps = exe
        .parent()
        .expect("the test runs from target/<profile>/deps");
    let newest = std::fs::read_dir(deps)
        .expect("read the deps directory")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            let archive = name.starts_with("libc_scheduler-") && name.ends_with(".a");
            let modified = path.metadata().ok()?.modified().ok()?;
            archive.then_some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified);
    newest
        .unwrap_or_else(|| panic!("no libc_scheduler-*.a in {}", deps.display()))
        .1
}

/// The system libraries a Rust staticlib needs, as `rustc --print native-static-libs` lists them.
fn native_libraries() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["-liconv", "-lSystem", "-lc", "-lm"]
    } else {
        &[
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
            "-lc",
        ]
    }
}

fn compiler(variable: &str, fallback: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| fallback.to_owned())
}

fn run(command: &mut Command) -> String {
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("cannot run {command:?}: {err}"));
    assert!(
        output.status.success(),
        "{command:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

/// What `feed_loop.c` prints, computed by the reference over the inputs it feeds.
fn expected_output() -> String {
    let sequencer = Address::repeat_byte(0x11);
    let user = Address::repeat_byte(0x22);
    let mut scheduler = Scheduler::genesis(SchedulerConfig {
        sequencer_address: sequencer,
        max_wait_blocks: 10,
        fridge_bound: 16,
        max_direct_payload_bytes: 4,
    });
    let batch = ssz::Encode::as_ssz_bytes(&Batch {
        nonce: 0,
        frames: vec![Frame {
            user_ops: Vec::new(),
            safe_block: 2,
            fee_price: 0,
        }],
    });
    let inputs = [
        (user, 1, b"a".to_vec()),
        (user, 2, b"b".to_vec()),
        (user, 3, b"c".to_vec()),
        (user, 4, b"long!".to_vec()),
        (sequencer, 5, batch),
        (user, 20, Vec::new()),
    ];

    let mut lines = Vec::new();
    for (index, (sender, block, payload)) in (0_u64..).zip(inputs) {
        let outcome = scheduler.feed(L1Input {
            index,
            sender,
            block,
            timestamp: block * 12,
            chain_id: 1,
            app_contract: Address::ZERO,
            payload,
        });
        let kind = match outcome {
            InputOutcome::DirectQueued => sys::SCHEDULER_DIRECT_QUEUED,
            InputOutcome::DirectDropped => sys::SCHEDULER_DIRECT_DROPPED,
            InputOutcome::BatchAccepted => sys::SCHEDULER_BATCH_ACCEPTED,
            InputOutcome::BatchStale => sys::SCHEDULER_BATCH_STALE,
            InputOutcome::BatchRejected(_) => sys::SCHEDULER_BATCH_REJECTED,
        };
        lines.push(format!("input {index} outcome {kind}"));
        while let Some(scheduled) = scheduler.next_tx() {
            let batch = scheduled.batch_nonce;
            lines.push(match scheduled.tx {
                L2Tx::Direct(direct) => format!(
                    "direct {} block {} bytes {} batch {batch}",
                    direct.input_index,
                    direct.block,
                    direct.payload.len()
                ),
                L2Tx::UserOp(op) => {
                    format!(
                        "user_op nonce {} block {} batch {batch}",
                        op.nonce, op.block
                    )
                }
            });
        }
    }
    let state = scheduler.state();
    lines.push(format!(
        "state {} {} {} {} {}",
        state.l1_input_count,
        state.next_batch_nonce,
        state.clock,
        state.first_queued_input,
        state.fridge_len
    ));
    lines.iter().map(|line| format!("{line}\n")).collect()
}

#[test]
fn c_feed_loop_matches_the_reference() {
    let out_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("c-scheduler-example");
    std::fs::create_dir_all(&out_dir).expect("create the example's directory");
    let binary = out_dir.join("feed_loop");

    run(Command::new(compiler("CC", "cc"))
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-pedantic"])
        .args(include_args())
        .arg(format!("{MANIFEST_DIR}/c/feed_loop.c"))
        .arg(static_library())
        .args(native_libraries())
        .arg("-o")
        .arg(&binary));
    let printed = run(&mut Command::new(&binary));

    let expected = expected_output();
    // The run must reach every path the example is there to show.
    assert!(expected.contains("outcome 1\n"), "a dropped direct");
    assert!(expected.contains("outcome 2\n"), "an accepted batch");
    assert!(
        expected.contains("direct 2 block 3 bytes 1 batch 1\n"),
        "a backstop drain"
    );
    assert_eq!(printed, expected);
}

#[test]
fn header_parses_as_cpp() {
    let probe = Path::new(env!("CARGO_TARGET_TMPDIR")).join("c-scheduler-header-probe.cpp");
    std::fs::write(
        &probe,
        "#include \"scheduler.h\"\n\
         static_assert(noexcept(scheduler_abi_version()), \"the API is noexcept\");\n",
    )
    .expect("write the C++ probe");
    run(Command::new(compiler("CXX", "c++"))
        .args(["-std=c++17", "-c", "-Wall", "-Wextra", "-Werror"])
        .args(include_args())
        .arg(&probe)
        .arg("-o")
        .arg(probe.with_extension("o")));
}
