# The Application contract

An app plugs into the sequencer by implementing
[`Application`](../../sequencer-core/src/application/mod.rs). It owns its state,
including each sender's nonce and its executed-input count; the protocol
defines how they advance, and the shared executor decides which transactions
run and verifies each successful transition. Application
code remains self-trusted: Rust capabilities cannot establish the correctness
of a native engine, its FFI, or its canonical counterpart
([threat model](../threat-model/README.md), "Self-trust").

This document **owns** the contract. [`AGENTS.md`](../../AGENTS.md) is the map.
The [wallet](../../examples/app-core/) is the reference implementation. A
production application may execute natively or wrap a Cartesi Machine.
The [C application binding](c-application-binding.md) adapts this contract to
native engines; its [build guide](../../bindings/c-app-engine/README.md) includes
a reference wallet integration.

## The execution methods

| Method | Contract |
|---|---|
| `user_nonce(sender) -> Result<u32, AppError>` | Report the sender's next nonce, zero for an address that has sent nothing, without changing state. |
| `apply_user_op(op) -> Result<AppOutputs, AppError>` | Execute a user op found at its sender's next nonce, advancing that nonce and the count by exactly one each. |
| `apply_direct(direct) -> Result<AppOutputs, AppError>` | Execute a direct, advancing the count by exactly one and moving no nonce. |
| `executed_input_count() -> ExecutedInputCount` | Report the count ([§4](#4-canonical-history-cursor-executed_input_count)). |

A transaction reaches the application only through the shared executor,
[`execute`](../../sequencer-core/src/application/mod.rs), in the canonical
machine, the lane and every replay alike. It runs a direct always. For a user
op it reads `user_nonce` and skips the op, changing nothing, when the op's
nonce is not the sender's next or that next nonce is `u32::MAX`
([user nonces](#user-nonces)); otherwise it calls `apply_user_op`. A user op
reaches the application with its sender, signed nonce, the frame's fee
exponent and the frame's safe block as its block
([`UserOpTx`](../../sequencer-core/src/l2_tx.rs)); a direct, with its sender,
inclusion block, input index, timestamp and payload
([`Direct`](../../sequencer-core/src/l2_tx.rs)). The application keeps no clock;
the [scheduler clock](scheduler-semantics.md#the-scheduler-clock) is the
scheduler's.

`apply_user_op` never refuses: whatever the op does or pays, even a fee the
sender cannot cover, it executes. Charging what the sender has, a saturating
subtraction, is the recommendation; the wallet charges the smaller of the fee
and the balance and credits the sequencer with exactly that amount.

Before an apply hook, the executor computes the checked successor of the
count. After `Ok`, it asserts that the engine reports exactly that successor
and, for a user op, that the sender's nonce advanced by exactly one, and it
returns the pre-execution count as the transaction's history offset. An engine
may use `ExecutedInputCount::advance` or implement the same transition
natively. Adapters report the engine-owned count rather than maintaining a
separate mirror. Overflow fails before the hook runs. An error defines no
successor: callers terminate the execution path and discard the instance,
without attempting to roll back or inspect partially updated state.

## Cross-cutting contracts

### 1. Determinism & purity

Execution must be deterministic over `(transaction, current state)`, including
output bytes and their order. Do not depend on wall time, randomness, pointer
values, nondeterministic iteration, or host-specific arithmetic in consensus
paths. `user_nonce` and admission are read-only, and they run on a different
schedule during live execution, canonical execution, and replay. State changes
happen through apply hooks; dump creation may change backing resources but
preserves logical state.

Wrapping an existing engine must preserve its transaction encoding, inclusion
semantics, and canonical state bytes.

`Application: Send + Sized` permits moving the engine to the lane's blocking
worker. It requires neither `Sync` nor `Clone`: the lane owns one mutable
engine, and an independent state fork is a fallible checkpoint/restore
operation. A shared-handle `Clone` would not establish independence. Engines
using FFI must justify `Send` from their actual handle and thread-lifetime
rules.

### 2. Replay safety — rejection, inclusion, and failure

These outcomes have different protocol meanings:

- A **skipped** user op, one the executor finds at the wrong nonce, changes
  no state and consumes no nonce or fee. A **refused** op, one the lane turns
  away at `POST /tx` ([§3](#3-admission--policy-never-consensus)), is never
  included. Neither is persisted.
- An **executed** transaction is included even if the business operation
  fails, is ignored, or cannot pay its whole fee. It advances the count, and an
  executed user op consumes its sender's nonce ([below](#user-nonces)). The
  wallet also charges the fee for a malformed method or failed transfer;
  malformed direct inputs are included no-ops. These semantics must agree with
  the canonical application.
- `AppError`, whether `Internal` or `Io`, is fatal in every method. It must not
  be disguised as a refusal or an included no-op. Fatal here means discarding
  the engine: the host still distinguishes a terminal `Internal` fault from an
  operational I/O failure that may clear after restart.

Every transaction executed live must execute against the same prior state on
replay. The sequencer persists included transactions and replays them on
restart through the same executor, without admission. A stored user op the
executor skips on replay, or a live op it skips after the lane's pre-check
passed, is an impossible state: the lane stops with a terminal error. It does
not recover an instance after a failed hook.

#### User nonces

Every application follows one nonce rule, and the executor applies it the same
way for all of them. The sequencer relies on it: `GET /nonce` derives a
sender's next nonce from the user ops it persisted, without asking the engine.

- A genesis state starts every account at nonce 0.
- The executor runs an op only when its nonce equals the sender's next nonce,
  as `user_nonce` reports it.
- Each executed user op, business failures and unpaid fees included, advances
  its sender's next nonce by exactly one, which the executor asserts. Nothing
  else changes a nonce: not direct inputs, not other senders' ops. The executor
  cannot check a direct's side, since it cannot enumerate the senders a direct
  might touch.
- Nonces are `u32`. `u32::MAX` has no successor, so an account that reaches
  it is exhausted: the executor skips its op at `u32::MAX`
  (`NonceExhausted`). Any other op carrying `u32::MAX` is skipped as a wrong
  nonce.

Under this rule a sender's next nonce is one past its latest included op. The
exception is a sender with no surviving live op. `GET /nonce` counts only the
era's live rows, and every setup, genesis base included, rebuilds the history
up to its stop block first ([era baseline](application-history.md#era-baseline)),
so a sender whose last op lies in the rebuilt history or below the base count
reads 0 until its first live op, and again if a later recovery invalidates all
of its live ops. The lane's refusal names the expected nonce, which corrects
the client. An engine outside the rule (gapped or keyed nonces, a
deposit that resets one) makes `GET /nonce` quote nonces the lane refuses.

The rule keeps every account's nonce in state forever. A batch-scoped
alternative makes replay state prunable and lets the fee move to the batch; it
is a [deferred design](../plans/2026-10-batch-scoped-replay-protection.md).

### 3. Admission — policy, never consensus

Admission answers whether the operator wants an op. Only the sequencer's lane
asks: the canonical machine never does, a guest build need not link it, and
its answer never changes what an included op does
([a user op's fate](scheduler-semantics.md#a-user-ops-fate)).

| Method | Contract |
|---|---|
| `admit_user_op(sender, op, fee) -> Result<Admission, AppError>` | Say, without changing state and without checking the nonce, whether the operator wants the op at this fee exponent. |
| `max_method_payload_bytes() -> usize` | Return the engine's stable method payload bound without constructing an instance. |
| `scheduler_limits() -> SchedulerLimits` | Return the fridge bound and direct payload cap the application's template compiles in. |

The lane checks a live op with
[`check_live_user_op`](../../sequencer-core/src/application/mod.rs), in the
order whose texts clients see: the fee cap against the frame's price, then the
executor's own nonce rule, then `admit_user_op`. A refusal is the `POST /tx`
422, with the refusal's text as its body. `Underfunded` covers a sender who
cannot pay the frame fee; `Other` carries any other policy, its text verbatim.
The wallet refuses only underfunded ops.

A zero method payload bound permits only empty method payloads. HTTP rejects
oversized method payloads; the lane uses the declared bound plus signed-op
metadata to compute batch capacity. It is not a canonical scheduler rule. The
scheduler limits are canonical: the [fridge](scheduler-semantics.md#the-fridge)
admits by them, and the lane drops the same oversized directs.

### 4. Canonical history cursor: `executed_input_count`

The count is the next application-history offset, never a SQLite cursor. It
counts executed L2 transactions, not L1 inputs. A transaction executed at
count `X` returns offset `X` and advances it to `X + 1`. Included no-ops count;
skipped ops, refusals, queued or dropped directs, envelopes, and empty batches
do not ([what counts](scheduler-semantics.md#what-counts)). Arithmetic is
checked, never wrapping or saturating.

The count belongs to the checkpoint's logical state. Standard recovery may
roll it back to a retained prefix and then advance over replacement inputs; a
recovery from a canonical machine starts at that machine's count
([§7](#7-canonical-recovery-integration)). SQLite stores an independent expected snapshot count and per-input
execution offsets, checked during catch-up.

HTTP snapshot metadata and mandatory WS claims carry the history version and
this count. Both restart and subscriber replay read the same current application
sequence. The [history guide](application-history.md) owns coordinates and
replica bootstrap; the [API contract](../../README.md) owns wire shapes.

### 5. Operational capacity for L1 reconciliation

A supported production application must promptly execute the complete
accumulated input range the persisted frontier can expose in one L1
reconciliation turn, including backlog within the supported operating
envelope. The lane processes that range before returning to user-op work.
There is no elapsed-time cutoff, preemption, or durable timeout-and-resume
cursor. Paging may bound memory; the complete-range reconciliation commit remains atomic.

This is a deployment assumption. A request overlapping reconciliation or
synchronous checkpoint creation may see extra acknowledgement latency.
Revisit scheduling only when application cost, L1 capacity/finality/backlog,
or measured checkpoint latency demonstrates the need.

### 6. Checkpoint lifecycle

A **recovery checkpoint** contains everything needed to resume the engine.
A **canonical comparison file** contains the deterministic state the watchdog
compares against the canonical application. They may be the same file; a
machine checkpoint may instead contain a separate app-state projection.

| Engine | Recovery checkpoint | Canonical comparison file |
|---|---|---|
| Wallet | SSZ wallet state | The same SSZ file, also returned by canonical inspect |
| Cartesi Machine wrapper | Full multi-file machine state | Deterministic app-state projection stored alongside it |
| Native DEX design | Fixed-memory state `M` plus required resumable metadata | `M` exactly as the canonical machine's labeled state range holds it |

The DEX row describes an integration requirement, not a verified private
implementation. The [watchdog guide](../watchdog/README.md#state-sources) owns
how the comparison is transported and which state source a deployment uses;
the [wallet format](../snapshots/format.md) owns its SSZ representation.

**Comparison bytes.** The comparison file must byte-equal what the canonical
machine yields after the same inputs, through the deployment's state source,
whether the engine ran them uninterrupted or was restored with `from_dump` or
rebuilt by recovery partway. For a range source, anything that shapes the
range's layout (allocator state, hash seeds, capacity policy) belongs to the
checkpoint and must not depend on the process:

- **Range source**: the entire flash drive or NVRAM carrying the configured
  user label, all of its configured length including any unused zero tail,
  raw: no filesystem, header, or trimming. The guest must have written the
  state into the range before it finishes each input. NVRAM has no page cache;
  a flash drive goes through the guest page cache, so the guest must flush
  (`fsync`, `msync(MS_SYNC)`, or `O_DIRECT`) first. The
  [Cartesi Machine facts](../cartesi-machine.md#memory-ranges-for-application-state)
  explain why the range must be raw.
- **Inspect source**: the single report the canonical application returns for
  the inspect query `state`.

- `create_dump(&mut self, prefix)` creates a checkpoint at an absent path,
  which may become a file or directory. On `Ok`, all files and directory
  entries referencing them, including the parent of `prefix`, must be durable
  against an immediate kernel crash. SQLite references the checkpoint only
  after this returns. The live logical state and its count stay unchanged;
  subsequent execution cannot mutate the checkpoint.
- `from_dump(prefix)` restores equivalent logical state, including the count,
  into an independent mutable engine. Execution cannot change the source
  checkpoint or another restored instance. The restored engine must remain
  usable after the source checkpoint is garbage-collected.
- `state_file_in_dump(prefix)` is a pure path function naming a single file,
  possibly `prefix` itself. Its bytes are the comparison bytes above.
- All checkpoint-owned artifacts reside at or below `prefix`, including the
  canonical comparison file. Disposal requires only ordinary filesystem
  deletion: the sequencer removes the enclosing directory, including its own
  `info.toml`. Removing a checkpoint must leave other checkpoints and restored
  engines usable; external resource cleanup is outside this contract.

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
inspection trait required by `CanonicalRunner::inspect` and the canonical
harness, not by the native sequencer. Human-readable debugging state
also stays on the concrete application.

### 7. Canonical recovery integration

Every supported production application must rebuild its native instance from
a trusted canonical machine. `from_canonical_state(state_file, count)` builds
an instance from the machine's comparison bytes, which setup reads from the
machine's `state` inspect report, at the machine's executed-input count, which
the machine's
`scheduler` report carries with the rest of the scheduler's state
([state export](scheduler-semantics.md#state-export)). A dump of the result
holds the same comparison bytes. The count is passed in because an
application's bytes need not hold it; the wallet's do, and it refuses a file
whose count differs.

So all logical state needed for future execution lives in the comparison
bytes. Native caches and backing resources may be rebuilt, but indispensable
mutable application state cannot exist only in the sequencer's local storage.
For the native DEX integration, the comparison bytes are the designated
canonical machine drive or memory region; the integration pins its location
and layout for each supported image.

`setup --base <MACHINE_DIR>` is the sequencer's recovery from a canonical
machine ([what setup does](../recovery/cockroach.md#what-setup-does)): it loads
the stored machine, answers its `scheduler` and `state` inspect queries, and
builds the instance with `from_canonical_state`. The `state` report must fit
one 2 MiB report; a read through the labeled range, as the watchdog does, is
not built yet ([register](../review/register.md#sequencer-recovery-follow-ups)).
Each integration's conformance tests check the projection; setup's own tests
hand it a machine's export and state bytes directly, and the devnet round
trips from a stored machine (`recovery_from_stored_machine_round_trip_test`
and its C-host twin) build the reference wallet from a real machine's `state`
report ([drills](../watchdog/incident-runbook.md#drills)).
The runtime `Application` trait stays independent of machine image layouts and
host tooling: reading the state source and the `scheduler` report is the
host's work.
