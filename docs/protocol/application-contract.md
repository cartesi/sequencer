# The Application contract

An app plugs into the sequencer by implementing
[`Application`](../../sequencer-core/src/application/mod.rs). It owns its state,
including execution progress; the protocol defines how that progress advances.
The shared execution boundary verifies each successful transition. Application
code remains self-trusted: Rust capabilities cannot establish the correctness
of a native engine, its FFI, or its canonical counterpart
([threat model](../threat-model/README.md), "Self-trust").

This document **owns** the contract. [`AGENTS.md`](../../AGENTS.md) is the map.
The [wallet](../../examples/app-core/) is the reference implementation. A
production application may execute natively or wrap a Cartesi Machine.

## The execution methods

| Method | Contract |
|---|---|
| `validate_user_op(sender, op, current_fee) -> Result<ValidationOutcome, AppError>` | Pure predicate: `Accept`, `Reject(InvalidReason)`, or fatal engine failure. No state change. |
| `apply_valid_user_op(valid, safe_block) -> Result<AppOutputs, AppError>` | Execute and advance embedded progress exactly once on success, using the frame clock. |
| `apply_direct_input(input) -> Result<AppOutputs, AppError>` | Execute and advance embedded progress exactly once on success, using the input's L1 block. |
| `progress() -> ApplicationProgress` | Return the count/clock pair from the engine by value. |

Execution callers use `validate_and_execute_user_op`, `execute_valid_user_op`,
and `execute_direct_input`. The first enforces `max_fee >= current_fee` before
app validation. App validation checks the nonce and fee balance; a rejection
is not persisted. Trusted replay uses the stored `ValidUserOp`, whose fee and
sender were established at inclusion, without validating a second time.

Before an apply hook, the boundary computes the checked expected successor.
After `Ok`, it asserts that the engine reports exactly that successor and
returns the input's pre-execution count as its history offset. An engine may
use `ApplicationProgress::advance` or implement the same transition natively.
Overflow fails before the hook runs. An error defines no successor: callers
terminate the execution path and discard the instance, without attempting to
roll back or inspect partially updated state.

`MAX_METHOD_PAYLOAD_BYTES` is both an ingress payload limit and a batch-sizing
input. HTTP rejects oversized method payloads; the lane uses the declared bound
plus signed-op metadata to compute batch capacity. It is not a canonical
scheduler rejection rule.

## Cross-cutting contracts

### 1. Determinism & purity

Execution must be deterministic over `(input, current state)`, including output
bytes and their order. Do not depend on wall time, randomness, pointer values,
nondeterministic iteration, or host-specific arithmetic in consensus paths.
Validation is read-only even though it may run on a different schedule during
live execution, canonical execution, and replay. State changes happen through
apply hooks; dump creation may change backing resources but preserves logical
state.

`Application: Send + Sized` permits moving the engine to the lane's blocking
worker. It requires neither `Sync` nor `Clone`: the lane owns one mutable
engine, and an independent state fork is a fallible checkpoint/restore
operation. A shared-handle `Clone` would not establish independence. Engines
using FFI must justify `Send` from their actual handle and thread-lifetime
rules.

### 2. Replay safety — rejection, inclusion, and failure

These outcomes have different protocol meanings:

- A validation rejection changes no state, consumes no nonce or fee, and is
  not persisted. `InvalidReason` covers nonce mismatch, insufficient max fee,
  and insufficient fee balance.
- A successfully applied input is included even if the business operation
  fails or is ignored. It advances progress. The wallet charges the fee and
  consumes the nonce for a malformed method or failed transfer after
  validation; malformed direct inputs are included no-ops. These semantics
  must agree with the canonical application.
- `AppError`, whether `Internal` or `Io`, is fatal in validation and execution.
  It must not be disguised as a client rejection or an included no-op.
  Fatal here means discarding the engine: the host still distinguishes a
  terminal `Internal` fault from an operational I/O failure that may clear
  after restart.

Every input executed successfully live must execute successfully against the
same prior state on replay. The sequencer persists included inputs and replays
them on restart. It does not recover an instance after a failed hook.

### 3. The safe-block clock — `last_executed_safe_block`

The clock is the maximum block carried by an executed input: frame `safe_block`
for user ops, L1 inclusion block for directs, or zero at genesis. On success,
the engine advances it with `max(old_clock, input_clock)`. Count zero implies
clock zero. Both fields survive checkpoint round-trips.

The [frame-clock policy](scheduler-semantics.md#sequencer-frame-clock-policy)
may deliver a delayed head jump as one step. Newly covered directs execute
first with their exact inclusion blocks; all user ops in the frame receive
the same frame clock. An empty clock-only frame does not execute an input or
advance the app clock.

Recovery reads this clock as `A`, the safe block reflected in the checkpoint;
a wrong clock mis-defines the reconstructed `(A, B]` fridge range
([cockroach recovery](../recovery/cockroach.md)).

### 4. Canonical history cursor: `executed_input_count`

The count is the next application-history offset, never a SQLite cursor.
An input executed at count `X` returns offset `X` and advances to `X + 1`.
Included no-ops count; rejections, merely queued inputs, envelopes, and empty
batches do not. Arithmetic is checked, never wrapping or saturating.

The count belongs to the checkpoint's logical state. Standard recovery may
roll it back to a retained prefix and then advance over replacement inputs;
cockroach recovery supplies an absolute starting count from the recovered
engine. SQLite stores an independent expected snapshot count and per-input
execution offsets, checked during catch-up.

The current HTTP/WS feed still uses physical SQLite rowids. History-version
and canonical-offset projection remain Track 3 work; clients must follow the
current README until that cutover.

### 5. Operational capacity for L1 reconciliation

A supported production application must promptly execute the complete
accumulated input range the persisted frontier can expose in one L1
reconciliation turn, including backlog within the supported operating
envelope. The lane processes that range before returning to user-op work.
There is no elapsed-time cutoff, preemption, or durable timeout-and-resume
cursor. Paging may bound memory; the drain/promotion commit remains atomic.

This is a deployment assumption. A request overlapping reconciliation or
synchronous checkpoint creation may see extra acknowledgement latency.
Revisit scheduling only when application cost, L1 capacity/finality/backlog,
or measured checkpoint latency demonstrates the need.

### 6. Checkpoint lifecycle

A **recovery checkpoint** contains everything needed to resume the engine.
A **canonical comparison file** contains the deterministic state the watchdog
compares against the canonical application. They may be the same file; a
machine checkpoint may instead contain a separate app-state projection. The
[format contract](../snapshots/format.md) describes three relevant layouts.

- `create_dump(&mut self, prefix)` creates a checkpoint at an absent path,
  which may become a file or directory. On `Ok`, all files and directory
  entries referencing them, including the parent of `prefix`, must be durable
  against an immediate kernel crash. SQLite references the checkpoint only
  after this returns. The live logical state and progress stay unchanged;
  subsequent execution cannot mutate the checkpoint.
- `from_dump(prefix)` restores equivalent logical state, including progress,
  into an independent mutable engine. Execution cannot change the source
  checkpoint or another restored instance. The restored engine must remain
  usable after the source checkpoint is garbage-collected.
- `state_file_in_dump(prefix)` is a pure path function naming a single file,
  possibly `prefix` itself. Its bytes match the canonical application's
  deterministic comparison representation, whether obtained through inspect
  or from a designated state drive.
- `delete_dump(prefix)` removes the app-owned checkpoint. The sequencer owns
  the outer directory and `info.toml`; the app owns its opaque `state` prefix.

Mutable checkpoint creation permits flushing, replacing mappings, or changing
working backing files inside an adapter. It does not permit a logical state
transition. CoW sharing is allowed if writes remain isolated and durability is
met. No public flush/clone/reopen protocol or delta chain is required.

`from_dump` must preserve `Io(NotFound)` for absent checkpoints. Structural
corruption may return `Internal` (as the wallet decoder does) or a specific
I/O kind such as `InvalidData` or `UnexpectedEof`. Operational I/O failures
retain their actual error kind; startup uses these distinctions to classify
a broken referenced checkpoint versus a retryable storage failure.

Genesis construction stays on the concrete engine because its inputs vary by
implementation. `CanonicalState::canonical_snapshot_bytes` is a separate
inspection trait required by the shared Rust scheduler's inspection method and
canonical harness, not by the native sequencer. Human-readable debugging state
also stays on the concrete application.

## Adapter migration

1. Remove the capability parameters and mutable progress accessor. Return the
   native count/clock pair from `progress()` without a Rust-side mirror.
2. Advance both fields inside each successful native apply transition,
   including no-ops. Keep validation pure and map its fatal failures to
   `AppError`, with expected rejection as `Ok(ValidationOutcome::Reject(...))`.
3. Change checkpoint creation to `&mut self` and establish durable, immutable
   checkpoints with independent restores. Preserve existing canonical bytes.
4. Implement `CanonicalState` only where canonical inspection needs it.
   Remove any `Clone` or `Sync` added solely to satisfy the old host bounds;
   justify `Send` against the native engine's ownership contract.

Changing these Rust interfaces preserves transaction encoding, expected
rejection semantics, snapshot bytes, scheduler ordering, and the database
schema. Fatal `AppError` propagation is an intentional exception: validation
and execution failures discard the engine rather than becoming a rejection
or an included no-op. The host's terminal-versus-retryable classification
still applies.
