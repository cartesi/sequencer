# ADR: The Runtime Authority Boundary

The architecture decision record for how authority — over speculative state,
promises, process lifetime, and recovery admission — is owned in the
sequencer. The mechanisms below are landed; the decision history and the
review trail that shaped them live in
[`../review/register.md`](../review/register.md) and the review history it
records.

## Context

Authority is split between durable facts, their writer roles, and exclusive
process ownership. A diagnosed terminal runtime fault stops the process;
there is no supported partially failed runtime that continues serving work.

The policy separates into three guarantees:

1. **G1 — terminal stop:** a diagnosed terminal runtime fault aborts the
   process without worker drain or database settlement.
2. **G2 — no silent fast-path:** every boot inspects durable facts and runs
   recovery, regardless of the previous process's verdict.
3. **G3 — scoped divergence freeze:** a committed canonical-divergence fact
   freezes its persisted acceptance domain immediately. The mechanism, its
   runtime reaction, its race bound, and the watchdog boundary are stated
   in full at
   [I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen).

A later command may start only after exclusive process ownership and the
admission facts pass. Effects handed to the network before process termination
may still complete remotely; the zombie-transaction model accounts for them.

## The mechanisms

### 1. `RuntimeScope`: structured process ownership

Every command acquires the OS-held exclusive data-directory lock before
inspection (`runtime/process_lock.rs`). For `run`, ownership transfers into a
`RuntimeScope` containing the lock and a `ShutdownSignal`. Workers may hold
the scope or the lock and signal separately. Operator shutdown, expected
recovery, and transient exits signal all workers and drain them normally.

Terminal runtime errors call `abort_terminal`: emit a diagnostic and call
`std::process::abort`. They do not signal a graceful drain, write a terminal
row, or return an error to an embedding caller. A terminal error discovered
during an ordinary drain follows the same path. Acknowledged user operations
already have a `synchronous=FULL` commit; interrupted transactions and
unacknowledged requests are covered by ordinary crash recovery. Downloads
and other active requests may be interrupted.

The supported deployment dedicates the process to the sequencer and assumes
the configured tracing subscriber returns promptly. Logging is best-effort;
there is no wall-clock termination bound if that subscriber blocks. Revisit
this policy if the sequencer must share a host process with independently
surviving services, or if production diagnostics can block.

The lock is released only after every runtime-owned child has actually
stopped; a dropped `JoinHandle` detaches rather than stops, so each worker
and nested blocking task retains its own lock clone until its closure ends.
This prevents two processes on one data directory; it is not distributed
fencing. Cleanup polls every worker concurrently, so one hung drain cannot
hide another worker's terminal exit. Ordinary shutdown has no hard deadline.
The reader can cancel a pending RPC read, but awaits any started SQLite
append before joining, so the final clean-exit divergence check sees every
committed sync.

Runtime construction is prepare → admit → launch: every fallible or awaited
operation happens while zero tasks exist; final admission checks one
consistent fact set; launch spawns every worker in one infallible,
non-yielding block, consuming the single-use `RuntimeAdmission` witness. A
preparation failure cannot leave a partially launched runtime, and no
refusal or retry can mint the witness.

### 2. Fact-derived admission and the terminal-fault black box

Admission is governed by three facts, each with one owner: the kernel
process lock (concurrent owners), two-sided `setup_complete` (command
ordering), and `canonical_divergence` (the one absorbing refusal — only a
fresh-directory cockroach rebuild proceeds); baseline schema and history
creation and setup completion are each one `synchronous=FULL` transaction.
There is no lifecycle admission state machine or operator acknowledgement.
Standard recovery is automatic, and restart policy after a terminal fault
is the exit contract (30 or SIGABRT = do not restart, page), which the
supervisor is expected to honor.

`terminal_faults` stores append-only causes for terminal errors returned
through a command bracket, best-effort and verdict-neutrally. Runtime aborts
do not pass through that bracket and leave only process diagnostics. Nothing
reads the black box for decisions.

A known-terminal fault refuses at re-detection rather than at a boot gate.
Every fault whose evidence the boot path reads re-refuses before the first
soft confirmation; the residual window is recorded in the threat model.
The honesty backstops (rollbackable soft confirmations, the watchdog
byte-compare, and the divergence freeze) do not depend on a boot gate.

### 3. Ordered startup recovery

Normal `run` startup inspects local terminal facts, syncs L1, selects a repair
from current facts, and checks the result. The flush branch orders flush →
sync through the returned safe block → cascade explicitly. There is no
phase driver or progress ledger; the flush witness is a local value.
Setup/rebuild, maintenance flush, and normal-run recovery retain distinct
typed controllers. The dispatch table, boot-local witnesses, and final
admission check are owned by
[`docs/recovery/README.md`](../recovery/README.md);
[`admission.tla`](../recovery/admission.tla) verifies the controller ordering.

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
valid physical replay rows authorize the current feed output. Effects handed to the network before process termination may still
complete remotely.

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
not an ordered counter. The durable canonical-coordinate foundation is
landed ([I18](../invariants.md), [I20](../invariants.md)). The current public
feed still uses physical SQLite rowid offsets; replacing them with
`ExecutedInputCount` and exposing history versions is owned by the
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
