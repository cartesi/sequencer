# Application, inclusion lane, and DEX integration review

Status: accepted and implemented locally. The investigation below records the
pre-change evidence. Current contracts live in
[application-contract.md](../protocol/application-contract.md) and the snapshot
docs. The reference C bridge is ported on a separate integration branch, not
cherry-picked into the main implementation.

## Accepted follow-up decisions

- Keep `Send`; remove unused `Clone + Sync`. Independent state forks use
  checkpoint/restore, not a required `Clone`. A non-Clone, non-Sync runtime
  fixture exercises actual preparation and launch.
- `progress()` returns engine-owned count/clock by value. Apply hooks advance
  it; the shared boundary preflights overflow and verifies successful
  transitions. Validation distinguishes rejection from fatal engine failure.
- Mutable `create_dump` permits backing-resource changes while preserving
  logical state. File and directory prefixes remain opaque. Durable immutable
  checkpoints, independent restores, and source-deletion independence are
  required; public flush/clone/reopen machinery is deferred.
- Recovery state and canonical comparison bytes may differ. The wallet uses
  the same binary SSZ file for both; the DEX design compares `M` with its
  canonical drive. The current watchdog's drive extraction remains follow-up.
- Canonical inspection is a separate trait. Lane bookkeeping is simplified
  without changing ordering, attempt limits, commit/ACK, or drain/promotion
  atomicity. CORS and Lua executable parity remain a separate focused commit.

The private DEX scheduler and native engine are still unavailable. Reference
bridge tests verify the proposed seam, not private engine conformance. DEX
conformance and the [Track 3 history API](../plans/2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff)
remain follow-ups. This review establishes reference integration coverage;
it does not establish that the Application surface is production-proven.

Reviewed on 2026-09-09:

- Local PR #28 work: `7d6238ab2ae1aadee6a858806747b042d87b896c`.
- DEX integration branch: `c31bf18413d8e9677ad9d663b9a045f44fafa4a3`,
  compared from common ancestor `993d3310ac0da63380a62d6d3c93bff22d2317ff`.
- [C application bridge, PR #32](https://github.com/cartesi/sequencer/pull/32):
  `0fa1755a882ce8aeeb6ba7877ba4ea7c479da9d1`, based on `01030fd7107e360d9a4d7e0e2852183eadb4b327`.
- Uncommitted lane annotations in the main checkout were read without alteration.
  Its unresolved index entry was not resolved by this review.

The actual DEX bridge and C++ scheduler have not been shared. PR #32 is evidence
of the intended integration approach, not proof of the private implementation.

## Findings and proposed Application boundary

### 1. Native progress ownership fits the supplied bridge

The current trait calls progress scheduler-owned, stores it inside the app,
requires immutable and mutable references to it, and controls the latter with a
separate capability. Execution then checks that application hooks did not change
it and that the getter and mutator agree. See
[`Application`](../../sequencer-core/src/application/mod.rs).

PR #32 describes a different, coherent ownership model: native execution advances
and persists count and clock, and two C functions read those values. The adapter
holds an opaque engine pointer. Mapping this directly onto the current trait
would conflict with the assertion that native execution must leave progress
unchanged. A Rust shadow value would introduce two representations of the same
fact and require synchronization at save/load boundaries.

Recommend treating `Application` as the complete execution engine:

- Read progress by value: `progress(&self) -> ApplicationProgress`.
- Let successful native execution update its own complete state, including
  progress. The protocol still defines the transition.
- The shared Rust execution boundary preflights the checked count successor,
  invokes execution, checks the exact expected count/clock after success, and
  returns the pre-execution offset.
- Remove the mutable progress accessor and both capability types. Remove
  progress-only validation-purity and post-error coherence checks; an execution
  error already defines no successor and the instance must be discarded.
- Preserve `clock = max(previous_clock, input_block)`, count zero implying clock
  zero, durable round trips, replay attribution checks, and deterministic
  application behavior.

PR #32 can implement the value read using its existing two scalar getters. No
C ABI change is required for that read, and the single-owner handle prevents
concurrent mutation between them. Rust implementations may share a small
progress-transition helper; the C++ implementation follows the same formula.

This deliberately gives up capability-enforced routing for safe Rust callers.
Production call sites must use the shared boundary and conformance tests must
verify the native contract. The current capabilities never prove validation:
`execute_valid_user_op` accepts a publicly constructible `ValidUserOp` because
trusted replay needs to bypass admission. They are not an FFI isolation boundary.

The earlier blanket rejection of `AppWithProgress<A>` in the review register was
too broad. A wrapper shared by canonical execution, the lane, and recovery could
own progress correctly, with codecs preserving existing canonical bytes. The
invariant rejects an off-chain-only sidecar, not composition. Nevertheless, that
would require coordinated ownership and codec changes, while the native-owned
value interface fits PR #32 without inventing a second owner. Prefer the latter.

### 2. Remove unused Clone and Sync requirements

The entry chain in [`harness.rs`](../../sequencer/src/harness.rs),
[`run/mod.rs`](../../sequencer/src/commands/run/mod.rs), and
[`workers.rs`](../../sequencer/src/commands/run/workers.rs) requires
`Application + Clone + Sync`. Prepared runtime state holds no application value;
the lane constructs the application inside its blocking thread.

This already has a concrete cost in PR #32: `EngineApp` supplies a panicking
`Clone` and an `unsafe Sync` justified by the current runtime never sharing it.
The C ABI forbids concurrent use of a handle. A public `Sync` implementation
licenses shared-reference calls from multiple threads, so current call-site
discipline is not a sound general justification for that implementation.

Removing all five `Clone + Sync` bounds compiled across the complete workspace
and all targets in an isolated archive. Remove the bounds and those adapter
workarounds. PR #32 explicitly permits moving handles, so `Send` is supported by
this ABI; there is no need to redesign thread confinement for this integration.
Its necessity belongs to the async host, rather than canonical transition
semantics, if a future engine is thread-affine.

### 3. Preserve all three validation outcomes

Rust validation currently returns only success or `InvalidReason`. PR #32's C
validation function returns OK, INVALID, or INTERNAL. The adapter therefore has
to abort when validation fails internally.

Recommend `Result<ValidationOutcome, AppError>`, where `ValidationOutcome` is
`Accept` or `Reject(InvalidReason)`. The shared protocol guard still checks
`max_fee >= current_fee` first. Expected rejection remains a nonmutating response;
internal failure propagates to the host's failure policy.

Keep validation and execution separate for now. Catch-up currently replays
`ValidUserOp { sender, fee, data }`, without nonce or max-fee fields. Combining
the methods would require rebuilding original operations and checking their
admission result on replay. That is possible, but PR #32 already supports the
split, so it has no demonstrated benefit here.

The bridge's rationale for aborting execution errors predates the local scheduler
fix: the reviewed scheduler propagates application errors instead of swallowing
them. The adapter can now report execution failures as `AppError`; process policy
belongs to the host. Exceptions must still never unwind across the C ABI.

### 4. Fix the rejection contract before asking clients to implement it

The current application contract recommends `ExecutionOutcome::Invalid` for
malformed application payloads, but application execution hooks cannot return
that type. The wallet consumes nonce and fee before decoding a method; malformed
methods and unsuccessful business operations return `Ok` with no outputs.
Malformed or unsupported direct inputs likewise execute as counted no-ops.

The distinction to document and test is:

| Outcome | Included? | Progress | Meaning |
|---|---|---|---|
| Admission rejection | No | Unchanged | Invalid nonce, insufficient fee balance, or max fee below frame fee |
| Included business failure/no-op | Yes | Advances once | Method fails under application rules; fee/nonce behavior remains the application's defined included semantics |
| Internal execution failure | No canonical successor | Instance discarded | A bug or unrecoverable execution failure, never an ordinary bad DEX order |

Do not change fee/nonce or rejection semantics as part of this interface cleanup.
Also correct two smaller documentation errors: ingress does enforce the declared
payload bound, and the wallet no longer repeats the shared max-fee check.

### 5. Keep checkpoint requirements, remove representation assumptions

Retain the simple lifecycle: load state; create an immutable, crash-durable
checkpoint; locate its canonical file; dispose of obsolete state. A returned
checkpoint must be durable before SQLite references it. HTTP readers retain
their leases, including for directory-shaped dumps.

PR #32 describes private live mutations over an immutable source image and an
explicit durable save. It does not require a caller-managed write-through
working image. The Track 6 draft's `open/flush/clone` lifecycle should not be
imposed on this integration. CoW and sparse writing can remain engine details.
Measure checkpoint tail latency before adding asynchronous staging.

The sequencer-owned outer dump is a directory containing `info.toml`. Its
application prefix can already be treated opaquely by create/load/delete and
canonical-file lookup. Explicitly allowing that prefix to be either a file or a
directory would accommodate PR #32 without requiring a dummy subtree. Pin this
with a single-file lifecycle fixture if adopted. Preserve directory support.

The load contract should explicitly state how an instance remains usable after
its source checkpoint is collected. PR #32 promises that property through its
private mapping; other implementations must provide equivalent independence.
Do not generalize that implementation into an assumption that every app consists
of one mapped file.

Durable deletion is unnecessary for the sequencer's safety: SQLite references
are removed first, and orphan files after a crash are acceptable. Durable creation
remains necessary. Preserve meaningful load-error classification: PR #32 maps
every IO_ERROR to `ErrorKind::Other`, losing the missing-artifact distinction the
current host uses to refuse a broken referenced checkpoint. Carry the needed
typed distinction across the ABI rather than parsing diagnostic text.

### 6. Put optional capabilities on their actual consumers

`export_state` has no generic Rust consumers; keep human-readable debugging on
the concrete app. `canonical_snapshot_bytes` belongs to canonical inspection,
not every native engine adapter. The Rust scheduler's inspection method should
require an inspection capability where used. A separate C++ canonical scheduler
can provide inspection itself while the sequencer serves the canonical file.
Do not turn the current default runtime error into a globally mandatory bridge
method merely to make the trait uniform.

Execution and durable checkpointing are distinct contracts with actual distinct
consumers. Separating those traits is reasonable if it makes the implementation
clearer, but no generalized capability or lifecycle framework is needed.

## Inclusion lane

No new supported canonical-order or acknowledgment correctness defect was found
in the reviewed lane, replay, snapshot, and storage paths.

The useful simplifications are local:

1. Collapse `ChunkOutcome`, the accepted-count return, and `FastTurnSummary`
   into one bounded-turn result. Preserve the cap on attempted requests, so a
   rejected flood cannot starve reconciliation; preserve commit-before-ACK.
2. Store `next_safe_input_index` rather than the previous complete
   `last_drained_direct_range`, whose end is the only subsequently used value.
   Advance it only after a successful commit.
3. Consider one storage frame-transition operation with an optional promotion
   argument, replacing the duplicated promoting/nonpromoting entry points.
   Drain attribution, progress mapping, frame creation, and promotion must
   remain in one transaction. The observation accumulator remains useful.

Answers to the in-tree annotations:

- `frontier_min_interval` limits SQL observation frequency (default one second).
  User-op chunks continue during that interval. The five-safe-block criterion
  controls logical frame advancement and deposit visibility; it is a separate
  policy. Removing it changes behavior while saving little code.
- The durable divergence check must precede the clock threshold, including when
  no new frame is due. It detects a poisoned accepted-batch projection.
- The reintroduced `is_storage_invariant_contained` check belongs to the old
  global terminal mechanism removed from the reviewed HEAD. It historically
  checked a different signal, not the same database fact twice.
- The divergence check is polling-based diagnosis. SQL does not fence every
  post-marker ordinary frame/user-op append; do not describe it as doing so.

Whole-range reconciliation remains appropriate under the explicit capacity
assumption: fix one safe frontier, execute its direct prefix, and atomically
attribute it to the advanced frame before later user ops. Paging bounds payload
scratch memory, not the full receipt vector or turn duration. Reconciliation,
checkpoint creation, and GC can delay overlapping requests. Measure them with
the DEX engine before introducing preemption or resumable ordering state.

## DEX branch parity

The five feature-only commits do not establish a large missing runtime surface.

| Capability | Local status |
|---|---|
| WS user-op nonce, frame safe block, batch nonce | Present; same wire fields |
| Direct-input input index, batch nonce, block timestamp, transaction hash | Present; same wire fields and encodings |
| WS catch-up close reason carrying live-start offset | Present |
| Avoid historical getLogs when the safe input count has not changed | Present |
| HTTP transaction request and acknowledgment | Same schema |
| Browser POST /tx | Works; CORS policy differs |
| Explicit Lua 5.4 executable selection | Not carried over everywhere |

The CORS discrepancy is concrete. The feature branch allows any origin, POST,
and request headers, with a 3600-second preflight cache, on ingress only. Local
`http.rs` applies `CorsLayer::permissive()` to the merged ingress/egress router,
including internal reads, and configures no preflight max-age. The branch's
egress-isolation expectation fails locally. Restore the narrow scope without
waiting for a port split; carry its error-path and preflight contract tests.

Reconcile Lua executable selection with the supported Nix/native environments;
blindly replacing every invocation can break an environment that exposes its
pinned Lua 5.4 as `lua` rather than `lua5.4`.

These larger items are absent from both public API variants, not lost fork
features:

- Recovery-aware public history: local storage has canonical count and
  era/generation, but WS and snapshot headers still expose physical rowids.
- Full remote recovery-dump export: local dumps support multiple files, while
  snapshot HTTP routes serve one canonical file.
- Public application-output delivery: current WS sends inputs and POST returns
  the inclusion acknowledgment, not notices or vouchers.

Keep the established history-protocol work separate. Add archive export or an
output stream only for a concrete consumer requirement. PR #32's C engine
binding is also separate integration work, not already provided by the CORS
branch.

## Scheduler integration and approach

Sharing the DEX scheduler between its canonical machine and cockroach recovery
would remove an important independent implementation. The Rust scheduler should
not be presumed more correct. However, compile-time selection alone does not
remove all possible disagreement: the live lane and SQLite acceptance/nonce
projection still encode protocol assumptions and must agree with that scheduler.

The eventual scheduler interface should be separate from per-application input
execution and should cover the actual recovery needs: restore a checkpoint,
seed pending directs, process L1 inputs, drain at the recovery stop, and return
application state and next batch nonce. Determine that interface from their real
scheduler rather than encoding the Rust implementation's convenience methods
into a new requirement now.

Future microbatch priority is a different decision. It can select an order among
pending, unacknowledged operations before validation/execution and persist that
chosen order; it need not create a protocol frame every 500 ms. Preserve direct
drain attribution and validate sequentially against the chosen order. Under the
current nonce contract, a cancel at nonce 11 cannot simply move before a new
order at nonce 10 from the same sender. A full 500 ms collection window also
spends the entire advertised acknowledgment budget before execution and commit.
These are design constraints for that later work, not reasons for a policy
framework today.

Recommended sequence:

1. Simplify Application ownership and error outcomes, remove unused bounds,
   and port the reference C bridge alongside the Rust wallet. Preserve canonical
   bytes and existing rejection behavior.
2. Polish the lane's local bookkeeping with its current ordering intact.
3. Close the narrow CORS/tooling parity differences in a focused integration
   change. Keep public history cutover as its own API change.
4. Integrate the real scheduler once available, then consider measured ordering
   policy requirements.

The main process improvement is to use the native adapter as an acceptance test
for an interface change. One source engine exercised through Rust, the C ABI,
replay, dump/load, and canonical execution exposes more useful integration
mistakes than additional capabilities around two self-trusted fields. Include
admission rejection, included no-ops, exact progress, output order, snapshot
immutability, and error classification. Cross-language scheduler tests should
compare behavior against the protocol, not automatically bless either side.

## Validation and limits

All commands used the pinned environment via
`direnv exec /Users/gcdepaula/projects/cartesi-dev/sequencer` from the reviewed
checkout unless stated otherwise.

- `cargo check --workspace --all-targets --locked --offline`: passed.
- `cargo test --offline --locked -p sequencer --lib ingress::inclusion_lane -- --nocapture`:
  50 passed.
- `cargo test --offline --locked -p sequencer-core -p app-core --lib`:
  107 core and 23 app tests passed.
- Isolated archive, removal of all five `Clone + Sync` bounds:
  full workspace/all-target check passed.
- Isolated archive, two real-listener tests adapted from the feature branch's
  CORS expectations: reproduced both differences (`/livez` returns allow-origin
  `*`; `/tx` preflight has no max-age). These are expected repro failures, not
  failures of the existing suite.

The existing cheap-app 5,000-direct backlog test took about 47 ms in this run.
That is neither a DEX benchmark nor a concurrent ACK latency measurement.
No private DEX engine/scheduler, native bridge runtime, or canonical-machine
end-to-end execution was validated in this pass.

## Implementation validation (2026-09-09)

The local implementation passes `cargo check --workspace --all-targets`,
strict workspace/all-targets/all-features Clippy, and formatting checks.
`cargo test --workspace --exclude canonical-test -- --test-threads=1` passed
697 tests; one pre-existing doc example remains ignored. Wallet SSZ golden
bytes are unchanged. The watchdog Lua 5.4 suite passed 62/62.

New coverage includes pure validation, native progress mismatch and overflow,
fatal validation propagation, no reads after a failed apply hook, independent
wallet and file/directory checkpoint restores, atomic frame/promotion rollback,
a Send-but-not-Clone-or-Sync runtime, ingress-only CORS on success and rejection,
and POST preflight policy. The CORS fixture initially queried an uninitialized
snapshot service and correctly triggered a terminal fault; it now seeds the
required finalized checkpoint. An unrelated lock-lifetime test failed once in
a parallel run and passed alone and in the final serial workspace suite.

CORS and Lua invocation changes close the reviewed public branch's remaining
narrow parity gaps. This does not deliver the separately planned history API,
remote full-dump export, output stream, or private scheduler integration.

Real-process follow-up rebuilt the devnet binaries at `5773b833` and passed
`restart_and_replay_test` with deposit, transfer, withdrawal, and restart replay.
`setup_recovery_round_trip_test` restored checkpoint `B=26, N=1`, accepted the
continuing nonce, passed its anchor/divergence checks, and finalized the resumed
snapshot at block 35. The test then failed during watchdog initialization:
`expected "archive_version" 7 (got 6)`. The installed in-process Lua Cartesi
binding expects the newer archive, while the repository image is pinned to
CM 0.20. The verified CM 0.20 CLI shim does not affect that Lua binding. This
E2E remains incomplete; no emulator pin or image was changed to make it pass.

The separate reference bridge integration also exposed a concrete host need:
lazy genesis construction must be fallible, and a custom CLI must reuse the
library's command-task exit projection. Its integration branch makes the
factory return `Result<A, AppError>` and exposes `run_command` for parsed
commands. Completed setup still avoids opening the original genesis. This
keeps file-load errors in the existing bootstrap error policy without adding
a second setup-admission check in the C host.
