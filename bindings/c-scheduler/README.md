# C scheduler export

[`include/scheduler.h`](include/scheduler.h) is the canonical scheduler's C
API, and this crate is its reference implementation: sequencer-core's
`Scheduler` behind the C functions, built as `libc_scheduler.a` (and an rlib
for its own tests). The transaction records are
[`application-engine.h`](../c-app-engine/include/application-engine.h)'s, so a
C or C++ guest hands what the scheduler gives straight to its engine;
[`c/feed_loop.c`](c/feed_loop.c) is that loop without an engine.

The header is a proposal to settle with the C port's author: whether a guest
links this export or a port implements the header is still open
([todo](../../docs/plans/2026-11-sequencer-recovery/todo.md#questions-for-bart)).
No Rust adapter over a foreign scheduler exists yet; the sequencer and setup
call the Rust `Scheduler` directly.

The reference panics on impossible states; the export turns each into a
negative status and a dead handle, so no panic unwinds into C. That needs the
default `panic = "unwind"`: a build with `panic = "abort"` aborts instead.

`cargo test -p c-scheduler` replays the
[conformance vectors](../../docs/protocol/scheduler-semantics.md#conformance)
through the header's declarations alone in
[`tests/conformance.rs`](tests/conformance.rs), the check a port of the header
is held to. It also compares the export with the Rust `Scheduler` input by
input, and builds and runs the C example with the system compiler (`CC`, else
`cc`), linked against the archive Cargo built for that test run (in
`target/<profile>/deps`), so no separate `cargo build` comes first.
