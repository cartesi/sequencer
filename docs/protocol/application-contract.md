# The Application contract

The FFI seam. An app plugs into the sequencer by implementing
[`Application`](../../sequencer-core/src/application/mod.rs). The sequencer
assumes the contracts below **without runtime enforcement** — it links the app,
calls it on the hot path and during catch-up, and trusts it to be a pure
deterministic state machine. A violation is not caught; it surfaces as
scheduler/sequencer divergence, which under rollup semantics is
theft-equivalent ([threat model](../threat-model/README.md), "Self-trust").

This document **owns** the contract. [`AGENTS.md`](../../AGENTS.md) §"Application
Trait Contract" is the map. The placeholder wallet
([`examples/app-core/`](../../examples/app-core/)) is the reference impl; a
production app will wrap a Cartesi Machine behind the same trait.

An app that is not written in Rust implements the same contract through a C
header instead. [`c-application-binding.md`](c-application-binding.md) describes
that path; everything below binds it identically.

---

## The execution methods

| Method | Mutates? | Clock advance | Failure contract |
|---|---|---|---|
| `validate_user_op(sender, op, current_fee) -> Result<(), InvalidReason>` | **No** — pure, read-only | — | `Err(InvalidReason)` ⇒ op skipped, no state change |
| `execute_valid_user_op(valid, safe_block) -> Result<AppOutputs, AppError>` | Yes | `clock = max(clock, safe_block)` | `Internal` is **fatal** (see *Replay safety*) |
| `execute_direct_input(input) -> Result<AppOutputs, AppError>` | Yes | `clock = max(clock, input.block_number)` | `Internal` is **fatal** |

`MAX_METHOD_PAYLOAD_BYTES` is the app's declared upper bound on a method
payload's encoded size (selector + args). The sequencer treats it as a sizing
input, not a gate: it derives the per-op byte cost
(`SignedUserOp::max_batch_metadata() + A::MAX_METHOD_PAYLOAD_BYTES`,
[`inclusion_lane/mod.rs`](../../sequencer/src/ingress/inclusion_lane/mod.rs)) to
compute how many ops fit a batch's byte budget. The app must not emit a method
payload larger than it declares.

### One execution entry point

User ops are **never** executed by calling `execute_valid_user_op` directly.
They go through the free function
[`validate_and_execute_user_op`](../../sequencer-core/src/application/mod.rs),
which enforces the protocol guard `max_fee ≥ current_fee` *before* app
validation, then calls `validate_user_op`, then `execute_valid_user_op`. It is a
free function — not an overridable trait method — precisely so no impl can skip
the guard. Both consumers (the inclusion lane and the canonical scheduler) call
it; that shared call path is half of the
[duality](scheduler-semantics.md#the-three-implementations-and-why-they-agree)
agreement.

Consequently `validate_user_op` must **not** re-implement the `max_fee` guard as
its contract (the free function already owns it); it checks only app-level
predicates — nonce match for user-op replay protection, and fee-balance
coverage. (An impl *may* additionally check `max_fee`, as the placeholder does,
but must not *rely* on being the only guard.)

---

## Cross-cutting contracts

### 1. Determinism & purity

Execution must be a pure function of `(input, current state)`:

- **No** `SystemTime::now()`, `HashMap`/`HashSet` iteration order, floating
  point, threads, or any other nondeterminism in a consensus path.
- `validate_user_op` is **pure and read-only** — no mutation, no time
  dependence, no randomness. State changes flow *exclusively* through the two
  execute methods. Mutating from `validate_user_op` breaks replay (validation
  runs on a different schedule than execution).
- The same bytes against the same state must always produce the same outcome and
  the same `AppOutputs`. This is what lets the off-chain mirror predict the
  canonical fold and what lets recovery's `fold_replay` reconstruct state from
  L1 alone.

### 2. Replay safety — `Invalid` vs `Internal`

The sequencer persists every executed input and, on restart, replays them in
order against a fresh instance to rebuild state (catch-up). Therefore:

- **Any input that executed successfully live must execute successfully on
  replay.** Catch-up treats `AppError::Internal` as **fatal** — it aborts
  startup and the sequencer cannot resume. Never return `Internal` for a byte
  sequence that previously succeeded.
- Prefer `ExecutionOutcome::Invalid` for malformed or ill-typed input caught at
  the app level — `Invalid` is replay-safe (it deterministically skips, live and
  on replay). Reserve `AppError::Internal` for genuine invariant violations
  ("validated user op cannot pay fee") — real bugs, deliberately fatal, not
  adversarial inputs.

### 3. The safe-block clock — `last_executed_safe_block`

`last_executed_safe_block() -> u64` returns the **maximum block carried by any
input this instance has executed** (frame `safe_block` for user ops, L1
`inclusion_block` for directs), or 0 if nothing has executed.

- It is **carried in execution, not a setter** — every execute method advances
  it via `max`, so an app cannot execute an input and forget to move the clock.
- It **must survive `create_dump`/`from_dump` round-trips** (it is part of the
  logical state a dump captures).
- Recovery reads it as `A`, the safe block a checkpoint state reflects, and the
  `(A, B]` fridge range is reconstructed from it
  ([cockroach recovery](../recovery/cockroach.md)). A wrong clock mis-defines
  that range.

`executed_input_count() -> u64` is a diagnostic seam — replay/catch-up and the
snapshot byte-comparison compare a live instance against a replayed one with it.

### 4. Dump lifecycle round-trip

The snapshot lifecycle ([snapshots](../snapshots/lifecycle.md)) drives four
dump methods. `Self`-typed methods load/construct; the associated functions are
pure over the path:

- `create_dump(prefix)` — `prefix` must not already exist; the impl creates and
  populates it. **Durability:** on `Ok`, the dump must survive an immediate
  kernel crash — the impl must `fsync` the dump's files *and* the directory
  entries referencing them (on POSIX: the prefix dir and its parent) **before
  returning**. The sequencer inserts the DB row pointing at this path *after*
  `create_dump` returns; without the in-method fsync, a crash can leave a
  WAL-flushed row pointing at absent bytes.
- `from_dump(prefix)` — rehydrate **equivalent logical state** from a dump this
  same impl wrote. Loading another impl's dump is undefined. `create_dump` then
  `from_dump` must round-trip: equal logical state, equal
  `last_executed_safe_block`, equal `executed_input_count`.
- `state_file_in_dump(prefix)` — a **pure function of `prefix`** (callable
  without loading the dump or instantiating the app), returning a single file
  (not a directory) whose bytes match what an independent canonical machine's
  `inspect_state` would produce for the same logical state. For impls whose
  persistence representation already *is* the canonical state, this can be the
  same file `create_dump` wrote ([format](../snapshots/format.md)).
- `delete_dump(prefix)` — remove a previously-created dump (GC of superseded
  snapshots).

Genesis construction is intentionally **not** on the trait — it varies per impl
(CLI config for the toy wallet, a machine-image path for a CM-wrapping app) and
lives on the concrete type, called by the runtime at bootstrap.

---

## Who depends on each contract

| Contract | Depended on by |
|---|---|
| Purity / determinism | the [duality](scheduler-semantics.md) (off-chain prediction = canonical fold); recovery `fold_replay` |
| Replay safety (`Internal` fatal) | catch-up on every restart |
| Safe-block clock survives dumps | cockroach recovery's `A`; snapshot offset accounting |
| `create_dump` in-method fsync | crash-safety of the dump/row ordering ([I13](../invariants.md)) |
| `state_file_in_dump` = canonical bytes | the watchdog / indexers reading finalized state |
| One execution entry point | the `max_fee` protocol guard's non-bypassability |

## Rejection semantics every app implements

`InvalidReason` is the protocol's rejection vocabulary — these produce **no state
mutation and are not persisted**:

- `InvalidNonce` — user-op replay protection.
- `InvalidMaxFee` — the op won't pay the current frame fee (log-space exponent,
  base 129/128; see [`fee`](../../sequencer-core/src/fee.rs)).
- `InsufficientFeeBalance` — the sender cannot cover the fee. "Fee", not "gas":
  it tracks DA, not compute.

Deposits are **direct-input-only** (L1 → L2) and must never be represented as
user ops; that is why `execute_direct_input` is required (no default) — a no-op
default would silently strand every deposit.
