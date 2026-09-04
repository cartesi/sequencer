# ADR: The Runtime Authority Boundary

The architecture decision record for how authority — over speculative state,
promises, process lifetime, and recovery admission — is owned in the
sequencer. The mechanisms below are landed; the decision history and the
review trail that shaped them live in
[`../review/register.md`](../review/register.md) and the dated ledgers beside
it.

## Context

Repeated review rounds found instances of one bug class: an effect, mutation,
or admission point forgot to consult a distributed terminal predicate. The
root cause was not one missing check — authority was implicit across workers,
markers, triggers, classifiers, and shutdown guards.

The policy separates into three guarantees:

1. **G1 — in-process closure:** after terminal closure, no new
   authority-bearing work is accepted.
2. **G2 — no silent fast-path:** an interrupted or terminal run cannot skip
   inspection on the next boot. This is carried by the unconditional recovery
   reducer: every boot re-derives its decisions from durable facts, never
   from the previous process's verdict.
3. **G3 — scoped divergence freeze:** once the canonical-divergence fact is
   committed, the accepted frontier, batch tree, and snapshot promotion are
   frozen immediately ([I15](../invariants.md)). `DangerDetector` owns prompt
   process shutdown; the running inclusion lane also refuses
   opportunistically when its existing frontier read observes the poison.
   User-op work authorized before runtime observation may still commit and
   acknowledge.

The enforceable unified policy:

> After in-process terminal closure linearizes, no new **authority-bearing
> mutation or promise** may be authorized. Work already accepted may
> complete. A durable divergence fact freezes its persisted acceptance
> domain immediately. It is deliberately not a per-user-op transaction
> fence. Containment/telemetry writes and immutable operator reads are not
> authority-bearing. A later command may start only after exclusive process
> ownership and the admission facts pass.

An acknowledgement, live feed event, nonce reservation, or L1 submission is
authority-bearing. Streaming an already-immutable operator snapshot is not.
Absolute cancellation of an effect already handed to the network is neither
claimed nor implementable; the zombie-transaction model already accounts for
that case.

## The mechanisms

Four mechanisms, each solving a different problem:

### 1. `RuntimeScope`: structured process ownership

Every command acquires the OS-held exclusive data-directory lock before
inspection (`runtime/process_lock.rs`). For `run`, ownership transfers into a
`RuntimeScope` shared by all runtime-owned data-directory work: the lock,
terminal abort watchdog, containment authority, and fault recorder in one
capability, constructible only from a held lock. The pure notification half
is the slim `ShutdownSignal`. `RuntimeScope::authorize()` mints the
borrow-scoped `Authorized` token that three effect functions require in
their signatures — the user-op acknowledgement, the L1 send, and the WS
emit — so at those sites forgetting the containment consult is a compile
error, not a convention. The snapshot-stream start, the `POST /tx` success
body, and the lane's batch-close and reconciliation commits consult the same
bit by hand, bounded by the exit contract.

The lock is released only after every runtime-owned child has actually
stopped; a dropped `JoinHandle` detaches rather than stops, so each worker
and nested blocking task retains its own capability clone until its closure
really ends. This is a cheap local foot-gun guard, not distributed fencing:
it prevents two processes on one data directory and nothing more.

Runtime construction is prepare → admit → launch: every fallible or awaited
operation happens while zero tasks exist; final admission re-runs the
recovery reducer over one consistent fact set; launch spawns every worker in
one infallible, non-yielding block, consuming the single-use
`RuntimeAdmission` witness. A preparation failure cannot leave a partially
launched runtime, and no refusal or retry can mint the witness.

### 2. Fact-derived admission and the terminal-fault black box

Admission is governed by three facts, each with one owner: the kernel
process lock (concurrent owners), two-sided `setup_complete` (command
ordering), and `canonical_divergence` (the one absorbing refusal — only a
fresh-directory cockroach rebuild proceeds). There is no lifecycle admission
state machine and no operator acknowledgement: standard recovery is
automatic, and restart policy after a terminal fault is the exit-code
contract (30 = do not restart, page), enforced by the supervisor. The only
durable telemetry is the `terminal_faults` black box — append-only
terminal-cause rows, written best-effort and verdict-neutrally; nothing
reads it for decisions.

The accepted trade, eyes open: a known-terminal fault refuses at
re-detection rather than at a boot gate. Every fault whose evidence the boot
path reads re-refuses before the first soft confirmation; the narrow
residual window is recorded in the threat model, and the honesty backstops
(rollbackable soft confirmations, the watchdog byte-compare, the divergence
freeze) never depended on a boot gate.

### 3. Recovery as a pure run reducer

Normal `run` startup is one unconditional loop:

```text
inspect -> classify once -> decide -> perform at most one phase -> inspect again
```

`reduce_recovery` is pure policy over one transactionally consistent
`RecoveryInspection` plus boot-local phase progress. Local absorbing facts
are inspected before any provider call, so a transient RPC error can never
mask a persisted divergence. Closed recovery is phase-granular —
`Flush → inspect → Sync → inspect → Cascade → inspect` — with the flush's
safe-block observation carried as an ephemeral, memory-only witness: a crash
loses it and the next boot repeats the idempotent flush. There is
deliberately no durable recovery-phase state machine. The
[`admission.tla`](../recovery/admission.tla) model verifies the controller
ordering; [`docs/recovery/README.md`](../recovery/README.md) owns the design.

Setup/rebuild, maintenance flush, and normal-run recovery retain distinct
typed controllers: their facts are unrelated, and a generic command reducer
would enlarge the state machine without closing an enforcement hole.

### 4. SQLite-centered runtime and the two-regime inclusion lane

SQLite is the durable coordination boundary between components. The input
reader atomically commits `safe_inputs`, `l1_safe_head`,
`safe_accepted_batches`, and any `canonical_divergence` fact in one sync
transaction; the lane reads that durable projection. The one deliberate
exception is HTTP ingress ↔ inclusion lane (bounded MPSC + oneshot), because
low-latency request/response over the lane's in-memory application is
unwieldy through SQLite — an exception for one local interaction, not a
precedent for an in-memory component bus.

The lane has two regimes. The **fast user-op regime** dequeues at most one
bounded chunk per turn — accepted or rejected — and commits the accepted
subset at most once with `synchronous=FULL`; only that commit authorizes
acknowledgements. Making the dequeue chunk itself the turn boundary keeps
entry to reconciliation independent of acceptance outcome, so rejected
floods cannot starve the frontier check. The **L1 reconciliation regime**
fires when the observed safe head is at least five blocks past the open
frame's clock: it consumes the complete accumulated newly-safe range,
promotes at most once, and opens exactly one frame at the observed tip —
jumps are never interpolated. There is no elapsed-time budget, preemption,
or resumable partial cursor inside a turn: the supported deployment assumes
the application promptly digests the whole range (revisit only if production
measurements disprove that).

Authority remains role-local and auditable: a FULL-committed user-op chunk
authorizes its acknowledgement; a valid sealed batch plus the durable
write-before-broadcast watermark authorizes an L1 submission; committed
valid rows plus their canonical `executed_inputs` attribution authorize feed
output. An already-authorized effect may finish after a later terminal
transition.

## Rejected alternatives (do not re-propose without new evidence)

- **`RunEpoch`** (a globally threaded internal fencing epoch): the OS lock
  plus structured task lifetime plus fresh per-scope channels already make
  an old sender unable to reach a new receiver, and there is no in-process
  hot restart to fence against. Revisit only if in-process restart or
  multiple admitted runtimes under one lock are introduced.
- **`EffectGate` / `LiveKernel`** (a universal effect mutex or actor): would
  duplicate the role-local linearization points the system already needs and
  force the reader and latency-critical lane through a new in-memory
  authority protocol, adding a second state machine without making the
  narrow content-identity check a complete divergence oracle. The
  `Authorized` token is not this: no mutex, no actor, no runtime state —
  the same predicate moved into the signatures of the operations it guards.
- **A generic command controller** over setup/rebuild/run/maintenance:
  their facts are unrelated; combining them enlarges the cross-product state
  machine without closing an enforcement hole.
- **A per-user-op divergence query** (or reader mailbox) on the hot path:
  the content-identity check is complete only for accepted-batch content
  identity ([I9](../invariants.md)); paying a per-chunk query would not buy
  a complete safety boundary.
- **A durable recovery-phase ledger**: the flush witness is boot-local by
  design; persisting it would re-create a state machine whose only effect is
  skipping an idempotent flush.
- **A durable boot gate on terminal verdicts**: a gate on a non-fact needs
  an operator acknowledgement to exit, and the acknowledgement carries no
  information the fact-derived reducer doesn't already re-derive.

## External history

```text
HistoryVersion = (EraId, RecoveryGeneration)
HistoryPosition = (HistoryVersion, ExecutedInputCount)
```

`EraId` (UUIDv4, minted write-once in the baseline transaction) identifies a
setup/rebuild era; `RecoveryGeneration` increments exactly once in the
standard-recovery transaction iff it invalidates at least one valid batch; a
clean restart changes neither. The pair is an equality/discontinuity token,
not an ordered counter. The feed coordinate is the canonical
`Application::executed_input_count()`, never a SQLite cursor. The durable
foundation is landed ([I18](../invariants.md), [I20](../invariants.md));
the public wire projection is owned by the
[Track 3 handoff](2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff).

## Performance posture

The product contract is `POST /tx` acknowledgement under 500 ms. Same-host
release sweeps across the cutover found no material regression: ACK p99 at
or below ~50 ms through concurrency 256 with zero rejections, concurrency-1
HTTP ACK p50 around 13 ms (submit-to-matching-WS-event p50 roughly double —
name which metric "round-trip" means). Same-host numbers are method-specific
regression evidence, never capacity claims: at high concurrency the load
clients contend with the sequencer, so the plateau is machine saturation. A
separate-machine load generator is required for capacity measurement, and
round-trip remeasurement belongs with the public history/API projection.
