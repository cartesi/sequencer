# The Application contract

The FFI seam. An app plugs into the sequencer by implementing
[`Application`](../../sequencer-core/src/application/mod.rs). The sequencer
links the app on the hot path and during catch-up, and trusts it to be a pure
deterministic state machine. The shared execution boundary enforces the
scheduler-owned progress transition; application-specific determinism and
mutation semantics remain self-trusted. A violation is fatal because silent
scheduler/sequencer divergence is theft-equivalent under rollup semantics
([threat model](../threat-model/README.md), "Self-trust").

This document **owns** the contract. [`AGENTS.md`](../../AGENTS.md) §"Application
Trait Contract" is the map. The placeholder wallet
([`examples/app-core/`](../../examples/app-core/)) is the reference impl; a
production app will wrap a Cartesi Machine behind the same trait.

---

## The execution methods

| Method | Mutates? | Progress effect | Failure contract |
|---|---|---|---|
| `validate_user_op(sender, op, current_fee) -> Result<(), InvalidReason>` | **No** — pure, read-only | unchanged | `Err(InvalidReason)` ⇒ op skipped, no state change |
| `apply_valid_user_op(capability, valid, safe_block) -> Result<AppOutputs, AppError>` | application state only | the shared `execute_valid_user_op` commits count `+1` and `clock = max(clock, safe_block)` after `Ok` | any `AppError` is **fatal** (see *Replay safety*) |
| `apply_direct_input(capability, input) -> Result<AppOutputs, AppError>` | application state only | the shared `execute_direct_input` commits count `+1` and `clock = max(clock, input.block_number)` after `Ok` | any `AppError` is **fatal** |

`MAX_METHOD_PAYLOAD_BYTES` is the app's declared upper bound on a method
payload's encoded size (selector + args). The sequencer treats it as a sizing
input, not a gate: it derives the per-op byte cost
(`SignedUserOp::max_batch_metadata() + A::MAX_METHOD_PAYLOAD_BYTES`,
[`inclusion_lane/mod.rs`](../../sequencer/src/ingress/inclusion_lane/mod.rs)) to
compute how many ops fit a batch's byte budget. The app must not emit a method
payload larger than it declares.

### One execution entry point

Application hooks are never called directly. User ops go through the free
function
[`validate_and_execute_user_op`](../../sequencer-core/src/application/mod.rs),
which enforces the protocol guard `max_fee ≥ current_fee` *before* app
validation, then calls the shared `execute_valid_user_op`; directs go through
the shared `execute_direct_input`. Those two functions stage and commit
`ApplicationProgress` around the app's `apply_*` hook. The raw hooks require a
borrowed opaque apply capability, and mutable progress access requires a
separate borrowed opaque commit capability; only the shared boundary can
construct either one. A caller therefore cannot invoke a raw hook or mutate
count/clock directly. The inclusion lane, catch-up, recovery fold, and
canonical scheduler all use these paths; that shared boundary is half of the
[duality](scheduler-semantics.md#the-three-implementations-and-why-they-agree)
agreement.

The boundary checks that progress remains unchanged after validation and after
an application hook, on both `Ok` and `Err`. After a successful hook it commits
the precomputed successor and re-reads the immutable getter to assert that the
application's getter/mutator pair is coherent. Count exhaustion is checked
before the hook runs.

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
  `apply_*` hooks. Mutating from `validate_user_op` breaks replay (validation
  runs on a different schedule than execution).
- The same bytes against the same state must always produce the same outcome and
  the same `AppOutputs`. This is what lets the off-chain mirror predict the
  canonical fold and what lets recovery's `fold_replay` reconstruct state from
  L1 alone.

### 2. Replay safety — `Invalid` vs `Internal`

The sequencer persists every executed input and, on restart, replays them in
order against a fresh instance to rebuild state (catch-up). Therefore:

- **Any input that executed successfully live must execute successfully on
  replay.** Catch-up treats every `AppError` as **fatal** — `Internal` and
  `Io` alike (every caller propagates both identically) — it aborts startup
  and the sequencer cannot resume. Never return an error for a byte sequence
  that previously succeeded.
- Prefer `ExecutionOutcome::Invalid` for malformed or ill-typed input caught at
  the app level — `Invalid` is replay-safe (it deterministically skips, live and
  on replay). Reserve `AppError::Internal` for genuine invariant violations
  ("validated user op cannot pay fee") — real bugs, deliberately fatal, not
  adversarial inputs.
- `AppError` defines no canonical successor. Application-specific mutation is
  not rolled back when a hook fails; every production caller terminates that
  execution path and discards the instance rather than continuing from it.

### 3. The safe-block clock — `last_executed_safe_block`

`last_executed_safe_block() -> u64` reads the safe-block field of the embedded
`ApplicationProgress`: the **maximum block carried by any input this instance
has executed** (frame `safe_block` for user ops, L1
`inclusion_block` for directs), or 0 if nothing has executed.

The live sequencer advances its frame clock on the best-effort safe-block
policy owned by
[scheduler-semantics](scheduler-semantics.md#sequencer-frame-clock-policy);
a delayed or epoch-sized head jump therefore arrives as a single step, not
a sequence of intermediate clocks.
All user ops in a frame execute sequentially with the same logical block
value; newly covered directs execute first but retain their own exact L1
inclusion blocks. A clock-only empty frame does not call the application or
autonomously advance this method—it supplies a newer clock to a later executed
user op.

- It is **scheduler-owned, not a setter** — the shared execution boundary
  advances it via `max` only after the application hook returns `Ok`.
- Count zero implies clock zero: no input has executed from which a non-zero
  clock could have been derived.
- It **must survive `create_dump`/`from_dump` round-trips** (it is part of the
  logical state a dump captures).
- Recovery reads it as `A`, the safe block a checkpoint state reflects, and the
  `(A, B]` fridge range is reconstructed from it
  ([cockroach recovery](../recovery/cockroach.md)). A wrong clock mis-defines
  that range.

### 4. Canonical history cursor: `executed_input_count`

`executed_input_count() -> ExecutedInputCount` is the authoritative boundary
coordinate of application execution, not a diagnostic counter. It starts at
zero. Each successful shared `execute_valid_user_op` or `execute_direct_input`
call advances it by **exactly one**; validation failures, rejected user ops,
inputs merely queued for later execution, and any other unexecuted input leave
it unchanged. An `AppError` is fatal and does not define a canonical successor
state. The boundary checks the `u64` successor before calling the application,
so exhaustion fails before application mutation; wrapping and saturating
arithmetic are unavailable on the newtype.

The count names the next history entry the application is ready to execute. If
an application is at count `X`, history input `X` is the input that must move it
to count `X + 1`. A subscriber holding that application state therefore resumes
from offset `X`; it must not translate between an application count and a
separate feed position.

The count is logical application state and **must survive
`create_dump`/`from_dump` round-trips**. Replay and both recovery procedures
must reconstruct the value implied by the resulting application state: a
standard recovery may roll it back to the retained prefix and advance it over
replacement canonical inputs, while a cockroach-recovered dump supplies the
absolute count from which the newly available history continues.

> **Cutover status:** the typed `ApplicationProgress` execution boundary,
> scheduler transition audit, placeholder application's durable count,
> per-input SQLite attribution, and snapshot/catch-up agreement checks are
> landed. The current feed still exposes a SQLite-rowid `from_offset`; the
> history-version and canonical-offset HTTP/WS projection remain Track 3 work.
> Until that API cutover, clients must follow the README.

### 5. Operational capacity for L1 reconciliation

A supported production application must promptly execute the complete
accumulated input range the persisted frontier can expose in one L1
reconciliation turn, including catch-up/backlog within the supported operating
envelope. Once the lane enters that turn, it processes the whole range before
returning to user-op work. The sequencer deliberately provides no elapsed-time
cutoff, preemption, or durable timeout-and-resume cursor for application
execution. Scratch paging may bound memory or read-query size; the
drain/promotion commit remains atomic and the logical turn is not resumable.

This is an explicit deployment assumption, not a deterministic state-transition
rule. A request that overlaps reconciliation may see additional acknowledgement
latency. Revisit the scheduling design only if application cost, L1
capacity/finality/catch-up behavior, or measurements show that the complete
newly-safe range is not promptly digestible. Do not add a second scheduler
speculatively.

### 6. Dump lifecycle round-trip

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
| Executed-input count survives dumps | canonical history offsets; replay; standard and cockroach recovery; planned Track 3 subscription continuity |
| `create_dump` in-method fsync | crash-safety of the dump/row ordering ([I13](../invariants.md)) |
| `state_file_in_dump` = canonical bytes | the watchdog / indexers reading finalized state |
| One execution entry point | non-bypassable `max_fee` and count/clock transitions |

## Rejection semantics every app implements

`InvalidReason` is the protocol's rejection vocabulary — these produce **no state
mutation and are not persisted**:

- `InvalidNonce` — user-op replay protection.
- `InvalidMaxFee` — the op won't pay the current frame fee (log-space exponent,
  base 129/128; see [`fee`](../../sequencer-core/src/fee.rs)).
- `InsufficientFeeBalance` — the sender cannot cover the fee. "Fee", not "gas":
  it tracks DA, not compute.

Deposits are **direct-input-only** (L1 → L2) and must never be represented as
user ops; that is why the `apply_direct_input` hook is required (no default) —
a no-op default would silently strand every deposit.
