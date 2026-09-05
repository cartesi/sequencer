# ADR: The Runtime Authority Boundary

The architecture decision record for how authority — over speculative state,
promises, process lifetime, and recovery admission — is owned in the
sequencer. The mechanisms below are landed; the decision history and the
review trail that shaped them live in
[`../review/register.md`](../review/register.md) and the review history it
records.

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
3. **G3 — scoped divergence freeze:** a committed canonical-divergence fact
   freezes its persisted acceptance domain immediately. The mechanism, its
   runtime reaction, its race bound, and the watchdog boundary are stated
   in full at
   [I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen).

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
`RuntimeScope` — the lock, terminal abort watchdog, and containment
authority in one capability, constructible only from a held lock — shared
by the workers that externalize or contain faults (the lane, the HTTP
server, the submitter); the workers that only need to stop (the reader, the
detector, the fee oracle) take its pure notification half, the slim
`ShutdownSignal`, and hold the lock directly. `RuntimeScope::authorize()` mints the
borrow-scoped `Authorized` token that three effect functions require in
their signatures — the user-op acknowledgement, the L1 send, and the WS
emit — so at those sites forgetting the containment consult is a compile
error, not a convention. The token is not an effect gate (the rejected
`EffectGate`, below): no mutex, no actor, no runtime state — the same
predicate moved into the signatures of the operations it guards. The
remaining externalization consults are
hand-placed and bounded by the exit contract: the three snapshot routes once
at request start (serving an already-immutable snapshot is not
authority-bearing, and per-chunk stream cancellation is deliberately not a
containment guarantee), the `POST /tx` success body, and the lane's fast-turn
entry
and its batch-close and reconciliation commits. Inside the token-covered L1
send, the poster re-consults the same bit before each keyed send and before
the write-before-broadcast watermark raise; the tick's chain-id gate, fee
estimate, nonce read, and confirmation watch are not re-gated. Those
re-checks narrow the bounded lag within an effect already authorized and
are not separate boundaries. The lane consults once per effect boundary,
not per line: adjacent re-reads of the bit would narrow the window by
nanoseconds in a design that already accepts the honest TOCTOU bound.

Containment is classification-at-birth. The first reporter is elected by
compare-and-swap, the sticky bit and its cause become visible together, the
abort watchdog is armed before cooperative shutdown is requested (a drain
may block), and nothing durable is written. A token proves the bit was
consulted and found clear at some point in its borrow, not at the instant
of the effect: a consumer that awaits between minting and effect carries
that bounded lag.

The lock is released only after every runtime-owned child has actually
stopped; a dropped `JoinHandle` detaches rather than stops, so each worker
and nested blocking task retains its own lock clone — through the scope or
directly — until its closure really ends. This is a cheap local foot-gun guard, not distributed fencing:
it prevents two processes on one data directory and nothing more. Cleanup
polls every worker concurrently, so one hung drain cannot hide a terminal
exit that must arm the bound. The watchdog holds only a weak
process-lifetime witness: it fires at the deadline exactly when a
controller, worker, or nested blocking task still retains the lock, and
ordinary operator/recovery shutdown has no hard deadline.

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
fresh-directory cockroach rebuild proceeds); baseline schema and history
creation and setup completion are each one `synchronous=FULL` transaction.
There is no lifecycle admission
state machine and no operator acknowledgement: standard recovery is
automatic, and restart policy after a terminal fault is the exit-code
contract (30 = do not restart, page), which the supervisor is expected to
honor. The only durable telemetry is the `terminal_faults` black box —
append-only terminal-cause rows, written best-effort and verdict-neutrally;
nothing reads it for decisions.

The accepted trade, eyes open: a known-terminal fault refuses at
re-detection rather than at a boot gate. Every fault whose evidence the boot
path reads re-refuses before the first soft confirmation; the narrow
residual window is recorded in the threat model, and the honesty backstops
(rollbackable soft confirmations, the watchdog byte-compare, the divergence
freeze) never depended on a boot gate.

### 3. Recovery as a pure run reducer

Normal `run` startup is one unconditional loop over a pure decision
function — inspect, classify once, decide, perform at most one phase,
inspect again — with no durable recovery-phase state machine.
Setup/rebuild, maintenance flush, and normal-run recovery
retain distinct typed controllers. The design, the dispatch table, the
boot-local witnesses, and the phase bound are owned by
[`docs/recovery/README.md`](../recovery/README.md);
[`admission.tla`](../recovery/admission.tla) verifies the controller
ordering; the arguments against a generic command controller and a durable
phase ledger are in the [register](../review/register.md).

### 4. SQLite-centered runtime and the two-regime inclusion lane

SQLite is the durable coordination boundary between components. The input
reader atomically commits `safe_inputs`, `l1_safe_head`,
`safe_accepted_batches`, and any `canonical_divergence` fact in one sync
transaction (a `setup --recovery` interim sync defers the frontier half);
the lane reads that durable projection and receives no in-memory cursor
from the reader. The one deliberate
exception is HTTP ingress ↔ inclusion lane (bounded MPSC + oneshot), because
low-latency request/response over the lane's in-memory application is
unwieldy through SQLite — an exception for one local interaction, not a
precedent for an in-memory component bus.

The lane has two regimes. The **fast user-op regime** dequeues at most one
bounded chunk per turn — accepted or rejected — and commits the accepted
subset at most once with `synchronous=FULL`; only that commit authorizes
acknowledgements, which are tied to chunk durability and never to frame or
batch closure. All-rejected chunks mutate nothing and open no transaction.
Making the dequeue chunk itself the turn boundary keeps entry to
reconciliation independent of acceptance outcome — the batch target counts
only included bytes, so rejected requests never advance it, and a rejected
flood cannot starve the frontier check; this adds no timer, cursor, or
fairness knob, and
returning to the outer loop costs only time-gate bookkeeping — no fsync and
no frontier read per chunk. The **L1 reconciliation regime** fires when the
observed safe head is at least five blocks past the open frame's clock: it
consumes the complete accumulated newly-safe range, catch-up and backlog
conditions included, promotes at most once, and opens exactly one frame at
the observed tip — jumps are never interpolated. There is no elapsed-time
budget, preemption, or resumable partial cursor inside a turn: the supported
deployment assumes the application promptly digests the whole range
([application contract §5](../protocol/application-contract.md#5-operational-capacity-for-l1-reconciliation); revisit
only if production measurements disprove that).

Authority remains role-local and auditable: a FULL-committed user-op chunk
authorizes its acknowledgement; a valid sealed batch plus the durable
write-before-broadcast watermark authorizes an L1 submission; committed
valid rows plus their canonical `executed_inputs` attribution authorize feed
output. An already-authorized effect may finish after a later terminal
transition.

## Rejected alternatives

`RunEpoch` (an internal fencing epoch); `EffectGate` / `LiveKernel` (a
universal effect mutex or actor); a generic command controller (one reducer
over setup/rebuild/run/maintenance); a
per-chunk divergence query, provider call, or reader mailbox on the hot
path; a durable recovery-phase ledger; a durable boot gate on terminal
verdicts. Each argument, its evidence, and its revisit trigger live in the
review register's refuted list
([`../review/register.md`](../review/register.md#refuted--do-not-re-propose-without-new-evidence));
do not re-propose without new evidence.

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
