# ADR: The Runtime Authority Boundary

**Status: accepted design; cutover steps 1–5 landed and verified; step 6's
durable storage/execution foundation is landed and its hot path is
benchmarked.** Re-evaluated and ratified 2026-08-01 after
the containment review; the history-coordinate and SQLite-centered runtime
refinements were accepted 2026-08-02.
Supersedes the remaining phases of
[`2026-08-terminal-containment-structure.md`](2026-08-terminal-containment-structure.md).
Cutover step 4 establishes the deployable successor architecture: one bounded
fast chunk, a five-safe-block reconciliation clock, and a typed poisoned
frontier that the lane cannot reconcile. The earlier `LiveKernel`/mailbox
design is rejected; it was never implemented. The declared step-4
latency/throughput gate is satisfied by the measurement below.

## Context

Five review rounds found instances of one bug class: an effect, mutation, or
admission point forgot to consult a distributed terminal predicate. The root
cause was not one missing check. Authority over speculative state, promises,
process lifetime, and recovery admission was implicit across workers,
markers, triggers, classifiers, and shutdown guards.

The policy separates into three guarantees:

1. **G1 — in-process closure:** after terminal closure, no new
   authority-bearing work is accepted;
2. **G2 — cross-crash stickiness:** an interrupted or terminal run cannot
   silently fast-path the next boot;
3. **G3 — scoped divergence freeze:** once the R2 canonical-divergence fact is
   committed, the accepted frontier, batch tree, and snapshot promotion are
   frozen immediately. `DangerDetector` owns prompt process shutdown; the
   running inclusion lane also refuses opportunistically if its existing
   frontier read observes the poison first. User-op work authorized before
   runtime observation may still commit and acknowledge.

The enforceable unified policy is:

> After in-process terminal closure linearizes, no new **authority-bearing
> mutation or promise** may be authorized. Work already accepted may complete.
> A durable R2 divergence fact freezes its persisted acceptance domain
> immediately. The danger detector owns prompt runtime reaction; any lane
> frontier read returns poison instead of reconciliation authority. It is
> deliberately not a per-user-op transaction fence. Containment/audit writes
> and immutable operator reads are not
> authority-bearing. A later command may start only after exclusive process
> ownership and explicit lifecycle admission.

An acknowledgement, live feed event, nonce reservation, or L1 submission is
authority-bearing. Streaming an already-immutable operator snapshot is not.
Absolute cancellation of an effect already handed to the network is neither
claimed nor implementable; the zombie-transaction model already accounts for
that case.

## Re-evaluation: the smallest sufficient design

The earlier draft proposed a globally threaded `RunEpoch`, then a mutex
`EffectGate`, then a `LiveKernel` actor and mailbox. All three are rejected.

The OS lock cannot be released while any runtime-owned data-dir child is
alive; admitted scopes create fresh channels; and there is no in-process hot
restart. An old sender therefore cannot reach a new receiver, and there is no
second runtime against which to compare an internal epoch. Revisit an internal
fencing epoch only if in-process restart or multiple admitted runtimes under
one process lock are introduced.

A universal effect gate or actor would duplicate the linearization points the
system already needs: FULL-committed user-op chunks authorize acknowledgements,
sealed valid batches plus write-before-broadcast watermarks authorize L1
submission, and committed valid history authorizes feed output (step 6's
landed storage foundation supplies the version identity and canonical
attribution; its public feed projection remains deferred). It would
also make the input reader and latency-critical lane communicate through a new
in-memory authority protocol even though SQLite is their durable coordination
boundary. That adds a second state machine without making the narrow R2 check
a complete divergence oracle.

Four mechanisms remain because each solves a different problem:

1. `RuntimeScope`: process/task lifetime and exclusivity;
2. durable lifecycle: write-ahead cross-crash stickiness;
3. recovery reducer: exhaustive repair and admission ordering;
4. SQLite-centered runtime ownership plus the inclusion lane's explicit fast
   and L1-reconciliation phases.

## Decision

### 1. `RuntimeScope`: structured process ownership

(Implemented as `runtime::shutdown::RuntimeScope`: the lock, terminal abort
watchdog, containment authority, and fault recorder in one capability,
constructible only from a held `ProcessLock`; the pure notification half is
the slim `ShutdownSignal`. `RuntimeScope::authorize()` mints the borrow-scoped
`Authorized` externalization token the effect functions require.)

Every command acquires the OS-held exclusive data-directory lock before
inspection. For `run`, ownership is transferred into a runtime scope shared by
all runtime-owned data-directory work. The lock is released only after every
such child has actually stopped; even a nominally non-authority-bearing lease
or file task must not race a later rebuild or clear.

This is deliberately a cheap local foot-gun guard, not distributed fencing or
proof of operator intent. It prevents two processes from mutating the same
data directory. It does not detect copied directories, coordinate hosts, or
try to make unsupported operator mistakes impossible.

Runtime construction has two stages:

1. **Prepare:** perform every fallible or awaited operation, including storage
   preflights, provider construction, and HTTP listener binding. No task exists.
2. **Launch:** spawn every worker in one infallible, non-yielding block and
   return the owning scope.

Dropping the scope requests shutdown. A detached Tokio handle is not treated as
a stopped task: each worker retains the same runtime-lifetime capability, so a
dropped handle cannot release the lock. Async RPC work remains promptly
cancellable; every nested blocking DB/file task retains its own capability
clone until the closure really stops. Any future nested task must likewise
retain the capability or be joined. This matters because dropping a
`JoinHandle` detaches it, and aborting `spawn_blocking` does not stop work that
has begun. The capability is installed at lock acquisition, including setup
and pre-worker recovery—not deferred until worker launch.

Fresh MPSC/oneshot receivers created inside each admitted scope are the
internal capability boundary. A previous process cannot coexist (OS lock), and
its tasks cannot survive lock release (structured lifetime), so its senders
cannot address the new runtime scope.

### 2. Pre-armed durable lifecycle

> **Amended 2026-08-19 (review L2) — this mechanism is superseded.** The
> lifecycle admission state machine and operator acknowledgement were
> removed after the maintainer's two-recovery analysis: the operator
> acknowledgement carried no decision the machine consumed (the reducer
> re-derives everything from facts; `NeedsRecovery` and `Ready` were
> mechanically indistinguishable at every admission), and its only cargo —
> crash-loop throttling — belongs to the R4 exit contract the supervisor
> already enforces. Admission is now fact-derived: process lock +
> two-sided `setup_complete` + `canonical_divergence`. The table below is
> retained as the historical design. *(Further narrowed 2026-08-22, review
> L3: the write-only attempt journal this amendment left in place was
> itself cut to a `terminal_faults` black box — append-only terminal-cause
> rows, written best-effort and verdict-neutrally; see
> [`2026-08-22-lifecycle-simplification.md`](../review/2026-08-22-lifecycle-simplification.md).)*
> G2's enforceable content — no boot may
> silently fast-path — was always carried by the unconditional reducer
> inspection, which is unchanged. Accepted trade, recorded in the review
> ledger: a known-terminal fault refuses at re-detection rather than at
> boot, so a restarted instance may serve until the faulty state is next
> read; the honesty backstops (rollbackable soft confirmations, the
> watchdog byte-compare, the divergence freeze) never depended on the boot
> gate.

The deleted terminal marker recorded a verdict after failure and was therefore
necessarily best-effort. The lifecycle records write-ahead intent before any
endpoint, worker, recovery mutation, or keyed L1 operation begins.

Lifecycle state is orthogonal to `HistoryVersion`; changing process state does
not change externally visible history. To avoid a second operation state
machine, lifecycle has one startup-controller phase. The command kind
(`Setup`, `Rebuild`, `Run`, or `MaintenanceFlush`) is audit metadata, not
branching lifecycle state:

```rust
enum ActivePhase { Starting, Live }

enum Lifecycle {
    Ready,
    Active { run_id: RunId, phase: ActivePhase },
    NeedsRecovery { reason: RecoveryReason },
    Poisoned { cause: TerminalReason },
}

struct LifecycleRecord {
    state: Lifecycle,
    // Persisted audit metadata, not a second transition state machine.
    active_command: Option<CommandKind>,
}
```

Database absence is `Uninitialized`; it is not another persisted enum arm.

| From | Event | To |
|---|---|---|
| `Uninitialized` | begin initial setup or explicit rebuild; one `synchronous=FULL` transaction creates the baseline schema, history state (fresh UUIDv4 era, generation zero, and command-appropriate base), and lifecycle | `Active { run_id, phase: Starting }` |
| `Ready` or `NeedsRecovery` | begin requested setup/rebuild/run command; validity and recovery need are re-derived from fresh facts | `Active { run_id, phase: Starting }` |
| setup-complete `Ready` or `NeedsRecovery` | begin maintenance flush; remember whether it began clean or recovery-gated | `Active { run_id, phase: Starting }` |
| starting `Active` | `Act(phase)` completes | stay starting; inspect again |
| starting `Active` | `Retry` | `NeedsRecovery { reason }`, then exit |
| starting `Active` | `Refuse` | `Poisoned { cause }`, then exit |
| starting `Active` | `Admit` (`run` only) | atomically `Active { phase: Live }`, then launch the admitted runtime |
| starting `Active` | `Complete` (setup/rebuild only) | `Ready` |
| maintenance `Active` | flush completes | `Ready` only when admitted from `Ready`; otherwise restore the prior `NeedsRecovery` verdict |
| live `Active` | proven-clean shutdown | `Ready` |
| live `Active` | classified repair required | `NeedsRecovery { reason }` |
| any `Active` | terminal verdict | `Poisoned { cause }` when recording succeeds; otherwise stale `Active` remains |
| any `Active` | crash, OOM, SIGKILL, or unknown-cleanliness exit | stale `Active` remains |
| stale `Active` | operator acknowledges the exact run record | `NeedsRecovery { UnclassifiedExit }` |
| `Poisoned` | operator acknowledges after remediation; no canonical divergence | `NeedsRecovery { OperatorCleared }` |
| `Poisoned(CanonicalDivergence)` | current operator supplies a fresh or explicitly wiped data directory and runs cockroach rebuild; its baseline transaction creates the new era | new era in starting `Active` |

`run_id` is a fresh opaque 128-bit audit handle for acknowledging the exact
stale record. It changes for every command, independently of the local
append-order `event_id`, so an acknowledgement captured before a database
replacement cannot target an unrelated command by numeric-ID reuse. It is not
an era, command capability, secret, or value threaded through runtime messages. The
requested `CommandKind` is persisted beside `Active` in the same lifecycle
transaction and retained in audit history. It guides operator UX and lets an
acknowledged interrupted setup/rebuild retry the same command, but the startup
controller still revalidates it from current facts rather than replaying a
stored action. The era is minted in the baseline transaction, so a retry that
retains that incomplete DB reuses the already-minted, not-yet-exposed era.

Stale `Active` is **operator-sticky**. A benign SIGKILL is indistinguishable
from a kill between terminal closure and cause recording, so automatic
inspection-and-admit would weaken G2. The operator exit is explicit but never
goes directly to `Ready`: acknowledgement transitions to `NeedsRecovery`, and
the reducer must inspect current facts.

`flush-mempool` remains an explicit, independently admitted operator
maintenance command. It has one job: settle every watermark-covered wallet
nonce without intending to launch the sequencer. Sending it through the run
reducer would silently add Sync/Cascade semantics—and possible batch
invalidation—to an operator request that asked only to flush. Maintenance may
start after completed setup from either `Ready` or `NeedsRecovery`; successful
maintenance returns to `Ready` only when it began there, and otherwise restores
the exact prior recovery reason/detail. This makes a failed flush retryable and
supports decommissioning without letting maintenance erase an unrelated
recovery verdict. It still refuses `Active`, `Poisoned`, and canonical
divergence.

Setup, rebuild, and `setup --recovery` likewise retain their explicit linear
controllers. Their checkpoint/genesis facts are not the run reducer's danger
facts, and combining them would create a larger cross-product state machine
without closing a missing authority boundary. The genesis application factory
is invoked only inside admitted plain setup when snapshot registration is
actually reached; completed setup no-ops and recovery never construct it.
`clear-terminal-fault` becomes the exact-record acknowledgement transition
above; it never deletes an unremediated durable fact or skips inspection.

Retryability is not cleanliness. A retryable provider error can occur after a
nonce reservation or broadcast. **Proven clean** means every accepted
authority-bearing action either completed or is durably represented such that
ordinary boot reconciliation is sound (a pending L1 send covered by the
wallet-nonce watermark is clean in this sense). An unknown outcome without
such coverage stays `Active` or becomes `NeedsRecovery`; it never reaches
`Ready` merely because retrying is operationally reasonable.

Terminal containment arms an independent two-second abort watchdog **before**
best-effort cause recording or cooperative cancellation. The watchdog holds a
weak witness to the process-lock lifetime, not another owning capability: at
the deadline it aborts only if a controller, worker, or nested blocking task
still owns that lifetime. Cleanup polls every remaining worker concurrently so
a hung drain cannot hide a terminal exit in another component. A blocked
SQLite audit write therefore cannot defeat the bound. Ordinary operator and
expected-recovery shutdown remain graceful and have no hard deadline.

Lifecycle admission is now authoritative: the marker file and
write-at-detection DB rung have been deleted. Lifecycle/audit history and the
exact-run acknowledgement UX remain.

### 3. Recovery as a pure run reducer

Cutover step 3 applies to normal `run` startup only. Initial setup, rebuild,
`setup --recovery`, maintenance flush, and exact acknowledgement deliberately
retain their command-specific controllers; step 5 verified and tightened those
boundaries instead of forcing their unrelated facts through this reducer. The
external history storage foundation is now landed, including the canonical
per-input coordinate; only its public API projection remains step 6/Track 3
work.

The implemented shape (names as in `sequencer/src/recovery/mod.rs`; the
draft's `RunInspectionOutcome` never needed to exist — inspection returns
`Result<RecoveryInspection, RecoveryError>` and one ranked classifier owns
the conversion):

```rust
enum RecoveryDecision {
    Admit,
    Act(RecoveryPhase),
    Retry(RecoveryRetryReason),
    Refuse(RecoveryRefusalReason),
}

// One enum is both the reducer's input and the driver's completion type;
// Flushed/PostFlushSynced carry the flush observation as the ephemeral,
// memory-only witness.
enum RecoveryProgress {
    NeedInitialSync,
    Inspecting,
    Flushed { observed_safe_block: u64 },
    PostFlushSynced { required_safe_block: u64 },
    Repaired,
}

fn reduce_recovery(
    progress: RecoveryProgress,
    facts: RecoveryInspection,
) -> RecoveryDecision;
```

Normal run boot is an unconditional loop:

```text
inspect -> classify once -> decide -> perform at most one run phase -> inspect again
```

One startup controller owns the exhaustive conversion from typed storage,
provider, and phase errors into `RunInspectionOutcome`/`PhaseOutcome`; raw errors
do not escape to a second supervisor classifier. `Facts` is the only input to
the pure decision; `Completed` always loops through inspection again.
“Recovered but forgot the admission re-check” and “new error variant silently
defaulted to retry” both become compiler-visible mistakes.

Inspection is ordered, not an arbitrary fallible bundle. It first reads all
local absorbing facts in one snapshot and returns `Refuse` immediately for
`CanonicalDivergence` (and any other terminal local fact). Only after those
checks pass may it query fallible external/provider facts or return `Retry`.
The process lock excludes a concurrent local writer; a phase that ingests new
facts returns `Completed` and must re-enter this local-first inspection before
anything else. Thus a transient RPC error cannot mask a persisted divergence.

Recovery actions are phase-granular. `FlushAndCascade` is not one action:

```text
Flush -> inspect -> Sync -> inspect -> Cascade -> inspect
```

A successful `Flush` produces a small `FlushWitness` containing the safe block
at which nonce-slot resolution was observed. The startup controller carries
that witness only in memory across the following inspection, Sync, inspection,
and Cascade. Sync must catch the persisted view up through the witnessed block
before Cascade is eligible. The witness is deliberately not lifecycle state or
a durable recovery-phase ledger: if the process dies, the next boot has no
witness and may run the idempotent flush again to establish a fresh one. This
keeps crash recovery fact-derived without adding another persisted state
machine.

`CanonicalDivergence` is absorbing and matched before every action. Each
mutation runs under the exclusive process lock and transactionally asserts
its durable phase preconditions and absence of divergence. No runtime epoch is
needed because there is one boot controller and no concurrent admitted runtime.

Runtime construction then has one capability boundary, in three ordered
parts:

1. **Prepare:** after the reducer decides `Admit`, perform every fallible or
   awaited runtime preparation while lifecycle remains `Active(Starting)`;
   launch zero tasks.
2. **Admit:** append `Active(Live)` for the exact run and construct the
   single-use `AdmittedRuntime` capability.
3. **Launch:** consume `AdmittedRuntime` in the infallible, non-yielding launch
   block. Only that capability may launch the runtime worker set.

A preparation failure therefore cannot leave a partially launched runtime or
a false `Live` record, and a repaired run cannot reach preparation until the
unconditional inspection loop has returned `Admit`. The reducer cutover's
separate admission TLA+ model verifies lifecycle admission and phase ordering;
the existing preemptive model continues to prove batch/zombie safety, not
lifecycle admission.

### 4. SQLite-centered runtime and an explicit two-phase inclusion lane

SQLite remains the intended durable coordination boundary between components.
The input reader fetches L1 facts and atomically commits `safe_inputs`,
`l1_safe_head`, `safe_accepted_batches`, and any R2
`canonical_divergence` fact. The lane reads that durable projection; it does
not receive reader-owned cursors or observations through a new channel. This
keeps crash recovery fact-derived and avoids a reader↔lane authority protocol.
Moving the R2 decision itself into the lane would split this transaction and
let `l1_safe_head` outrun `safe_accepted_batches`, unless we introduced staged
observations, a second processed cursor, and boot reconciliation. That is a
new recovery state machine, so only reaction—not detection—moves.

The deliberate exception is HTTP ingress ↔ inclusion lane. User-op handling
is a latency-sensitive request/response over the lane's in-memory
`Application`, so the existing bounded MPSC + oneshot path is clearer than
using SQLite as a request bus. It is an exception for one local interaction,
not a precedent for moving all workers behind an actor.

The lane has two regimes:

1. **Fast user-op regime.** One turn dequeues at most one existing
   `max_user_ops_per_chunk` chunk, regardless of how many requests are accepted
   or rejected. It validates/executes the accepted subset and, when that subset
   is nonempty, commits it at most once with `synchronous=FULL`; only that
   commit authorizes accepted acknowledgements. Rejections mutate nothing and
   require no durability commit. Chunking amortizes the fsync. The outer loop
   then gets another opportunity to enter L1 reconciliation. No R2 marker
   query, provider call, reader mailbox, timer, or new fairness knob is added
   to this per-chunk path. The product contract remains `POST /tx`
   acknowledgement below 500 ms. The maintainer's last localhost round-trip
   observation was about 14 ms; that is a useful regression baseline, not a
   guarantee or a substitute for measurement.
2. **L1 reconciliation regime.** At the lane's existing safe-frontier turn,
   read the frontier and R2 marker in one SQLite snapshot. A marker returns a
   typed terminal state such as:

   ```rust
   enum SafeFrontierState {
       Open(SafeInputFrontier),
       CanonicalDivergence {
           nonce: u64,
           safe_input_index: u64,
       },
   }
   ```

   On divergence, do not execute another direct input, promote a snapshot, or
   return to the fast regime; close intake, reject queued work, and terminate.
   A user-op chunk already executing, or committed before this observation,
   may still acknowledge. This is an honest rollbackable-soft-confirmation
   bound, not an instantaneous cross-worker barrier. `DangerDetector` owns
   prompt process-wide shutdown on its existing two-second cadence. The lane's
   typed result is an opportunistic refusal at a read it already performs, not
   another detector or reaction-time guarantee.

The pre-step-4 inner drain's queue-empty/batch-target rule was not intrinsically
finite: rejected requests added no batch bytes, so sustained rejected traffic
could keep the queue nonempty without ever hitting the target. Step 4 makes the
bounded dequeue chunk itself the fast-turn boundary. This is a count bound at
an existing seam, not a timeout or another scheduler, and it makes entry to the
slow turn independent of acceptance outcome.

Once entered, the reconciliation turn consumes the complete accumulated
newly-safe range exposed by the persisted frontier, including supported
catch-up/backlog conditions. We explicitly assume that this whole range is
quickly digestible by the application. There is therefore no elapsed-time
budget, yield deadline, durable partial cursor, or timeout-and-resume scheduler
inside a turn. Internal scratch paging bounds memory and read-query size only;
the drain/promotion commit remains atomic and the turn is not resumable.
Revisit this only if a production application or supported backlog disproves
the digestibility assumption with measurements.

L1 reconciliation has no separate per-turn latency SLA and is not preempted to
serve a request that overlaps it; the capacity assumption keeps it compatible
with the overall acknowledgement objective in practice. The semantic trigger
is block-only: for latest persisted safe head `H` and open-frame clock `S`, an
open projection reconciles when `H - S >= 5`. It consumes the complete
accumulated direct range, promotes at most once, and opens exactly one frame at
`H`, even when the direct range is empty. A delayed or epoch-sized observation
jump is never interpolated; `H` becomes the new persisted anchor. The existing
time gate controls SQLite observation load, not clock semantics. Direct-input
presence is work discovered at the turn, not another trigger.

Batch closure remains orthogonal: the successor batch necessarily owns a
structural first frame carrying the unchanged `safe_block`. Thus block distance
is the only reason logical frame time advances, not the only reason a frame row
exists. Snapshot promotion waits for the same reconciliation turn so it stays
atomic with the drain. Frame rotation also resamples the current frame fee
today, but that coupling is not part of the clock decision; fee policy may be
hoisted to the batch in a later focused design.

The typed frontier read has an honest race bound. A turn that already observed
`Open` may finish if the reader commits divergence concurrently; eliminating
that window would require a lock or transaction spanning application
execution. Existing SQLite triggers stop conflicting persisted promotions and
batch-tree writes, while the detector and next typed read stop the runtime. No
cross-worker lock is added.

`WriteHead` is a lane-local, reconstructible cache of SQLite facts. The lane
loads it on startup, updates it only after successful commits, and discards it
on error or restart. It is not an inter-component authority or a required
architectural state machine. Re-deriving more of it from SQLite on each turn
is a valid independent simplification to benchmark later, not part of this
cutover.

Authority remains role-local and auditable:

- a FULL-committed user-op chunk authorizes its HTTP acknowledgement;
- a valid sealed batch plus the durable write-before-broadcast watermark
  authorizes an L1 submission;
- committed valid rows plus their canonical `executed_inputs` attribution
  authorize future canonical feed output; the landed step-6 foundation
  supplies `HistoryVersion`, while enforcing and projecting it at the public
  feed boundary remains deferred.

An already-authorized HTTP write, L1 broadcast, or WS delivery may finish
after a later terminal transition. No global actor is needed to restate those
three boundaries.

#### Scope of R2 canonical-divergence detection

R2 is complete for one narrow question: for every at/above-anchor landing that
the off-chain `scheduler_accepts` simulation accepts, is there a valid local
closed batch at the same nonce with identical wire bytes? Its outcomes are
`Foreign` (no local batch), `Mismatch` (different bytes), and `Match`. The
detecting reader transaction records `Foreign`/`Mismatch` and freezes the
acceptance frontier atomically with the new safe head, so the head is never
exposed without the terminal fact.

R2 is not a general canonical/application divergence oracle. It does not prove
the trusted checkpoint or application state correct; a wrong-high cockroach
checkpoint nonce can escape it; it does not independently detect bugs in
direct-input or user-op execution; and it shares the
`scheduler_accepts` implementation, including its documented self-trust
omissions. Absence of the marker therefore does not prove global agreement.
Unlike modeled staleness recovery, an R2 finding is not automatically
repairable: it requires operator-driven cockroach recovery. These limits are
why adding the check to every roughly 14-ms user-op chunk would be
over-engineering rather than closing a complete safety boundary.

### 5. External history: identity plus canonical application position

```text
HistoryVersion = (EraId, RecoveryGeneration)
HistoryPosition = (HistoryVersion, ExecutedInputCount)
```

- **`EraId`** identifies one setup/rebuild era. It is a random UUIDv4 newtype
  (16 bytes in storage; canonical lowercase hyphenated form on JSON), minted
  write-once in that era's baseline transaction; `created_at` is stored
  separately. A bare timestamp is insufficient under clock rollback or
  simultaneous setup, and no ordering property is needed.
- **`RecoveryGeneration`** starts at zero and increments exactly once in the
  same standard-recovery transaction iff that transaction invalidates at least
  one valid batch. A no-invalidation repair/Tip ensure does not bump it.
- A clean restart or inspection that admits without changing history changes
  neither field.
- Cockroach recovery/fresh setup mints a new `EraId` and resets generation to
  zero. If an interrupted attempt retains its incomplete DB, retry reuses the
  already-minted, externally unexposed era. A fail-loud partial-fill refusal
  instead requires the operator to wipe and retry, which mints another
  unexposed era. Once matching root Tip plus the atomically bound finalized
  snapshot/`K` pair exist, that completed fill is authoritative; retry does not
  compare its base with a later fold at a newer `C`. A running era never rotates
  implicitly; changing it is an explicit operator-driven setup or cockroach
  rebuild.

Copying an initialized data directory also copies its `EraId`; the pair cannot
detect that clone by itself. We do not add clone detection, distributed
fencing, implicit rotation, automated database replacement, or a resumable
rebuild protocol. The supported catastrophe path is deliberately simple: the
operator supplies a fresh or explicitly wiped data directory and runs the
one-shot rebuild. The local process lock only prevents concurrent owners of
one data directory.

The pair is an equality/discontinuity token, not a globally ordered counter.
Within one era, a changed generation means “discard and replay the soft
suffix.” The feed offset itself is the canonical application boundary
`Application::executed_input_count()`: an application at count `X` subscribes
at `X`, receives the input at `X`, and advances to `X + 1`. Standard recovery
restores the stable application state (rolling the count back), executes the
force-drained directs (rolling it forward), and may therefore reuse suffix
offsets under the incremented generation.

Cockroach recovery folds to recovered application state `S'`, then binds
`K = S'.executed_input_count()` as that era's
`base_executed_input_count` — together with its physical sibling
`base_safe_input_index`, a write-once pair bound atomically — in the same
transaction that registers the initial finalized snapshot. Rebuild baseline creation deliberately leaves `K` NULL,
and setup completion refuses until both the bind and finalized snapshot exist.
New history starts at `K`. The absolute
coordinate survives even though entries below `K` do not: a request for
`X < K` fails with typed history-unavailable metadata including
`available_from = K` and the current bootstrap recipe. A changed era therefore
still requires current-era bootstrap even when an old state's numeric count
happens to be in range.

The scheduler specification owns which transitions advance the count; the
application's canonical state persists it; storage records the input at each
boundary. Safe-input drain attribution, including batch envelopes and
cockroach-fill cursor padding, is not application history and must not consume
this coordinate. In particular, `K` is independent of the snapshot's physical
`l2_tx_index` and the current rowid replay cursor; those values still include
cursor-padding rows. SQLite now records the canonical offset sparsely beside
each executable valid physical row; invalidation deletes only the derived
suffix mappings so replacements reuse those offsets under the new generation.
Snapshot rows carry both coordinates and catch-up checks every mapping before
execution. The subscription request will carry
`(EraId, RecoveryGeneration, ExecutedInputCount)`; once admitted, subscription
responses carry `RecoveryGeneration` but do not repeat `EraId`.

Track 3 owns the exact wire projection: the request carries the pair, WS
responses repeat only the generation, and finalized HTTP pages identify their
era. Finalized pages are immutable within an era, not forever across a rebuild.
The exact post-cockroach bootstrap artifact/API remains a Track 3
consumer-design question.

#### Reference-scheduler hardening (landed)

`Scheduler<A>` has been audited against the executed-input-count transition
table. Successful user ops and directs share one checked typed execution
boundary with the live lane, catch-up, and recovery fold; reject, skip,
enqueue, and envelope paths leave the count unchanged; overdue directs still
execute first; overflow fails before mutation; and `AppError` is fatal rather
than silently skipped. Distinct opaque capabilities keep application hooks
from mutating scheduler-owned progress. The durable attribution and replay
tests exercise the same boundary.

## Cutover and deletion order

1. **Structured runtime ownership (landed).** Prepare before launch and retain
   the lock through child termination. The temporary marker hardening from
   this step was deleted when step 2 made lifecycle authoritative.
2. **Durable lifecycle (landed).** The append-only lifecycle and stale-`Active`
   acknowledgement run under the existing process lock. It is the sole boot
   authority; marker-based boot authority is deleted.
3. **Recovery reducer (landed and verified).** Normal `run` startup now uses
   the pure run reducer, ephemeral flush witness, guarded phase-granular
   actions, and the `AdmittedRuntime` prepare/admit/launch boundary. Final
   facts, reducer decision, and exact-run `Active(Live)` append share one
   transaction; that transaction returns the unforgeable launch witness.
   Raw worker/HTTP launch functions are crate-private, so production app
   crates cannot bypass this path; integration-style tests live inside the
   crate.
   The admission TLA+ model belongs to this step. Maintenance flush, initial
   setup/rebuild, and `setup --recovery` remain outside this reducer.
4. **Lane reconciliation reaction and clock (landed).** R2 detection remains in
   the input reader's atomic SQLite sync. One bounded dequeue chunk—accepted or
   rejected—is the fast-turn boundary. The lane's existing time-gated read now
   returns `Open(frontier)` or typed canonical divergence, with poison winning
   before direct execution, promotion, or clock arithmetic. `DangerDetector`
   remains the prompt shutdown owner. For an open projection, `H - S >= 5`
   advances directly to one frame at observed tip `H` and consumes the complete
   accumulated direct range; smaller deltas wait and jumps are not
   interpolated. Tests pin reject-only bounded progress, poison precedence at
   zero delta, direct delay/drain at the threshold, empty ticks, jump collapse,
   and reset-to-tip semantics. The per-chunk transaction is unchanged. This is
   the first deployable successor architecture; the operational measurement
   below found no material regression.
5. **Command/shutdown closure (landed).** Keep setup/rebuild, maintenance, and
   exact acknowledgement as separate typed protocols rather than a generic
   command reducer. Genesis construction moved behind plain-setup admission.
   Maintenance admission now carries its clean/recovery-gated origin so a
   flush can be retried without erasing an unrelated verdict. Every lifecycle
   settlement consumes the semantic failure classifier, not its numeric exit
   projection. Terminal containment arms the independent two-second abort
   watchdog before cancellation/audit work, and cleanup polls all workers
   concurrently so a hung component cannot conceal the terminal exit that
   should arm it. Ordinary shutdown remains graceful.
6. **History protocol (storage/execution foundation landed; API open).** The baseline transaction
   now mints `EraId`, initializes `RecoveryGeneration = 0`, and pre-arms the
   initial lifecycle row. Standard recovery bumps the generation in the
   invalidation transaction iff it invalidates at least one valid batch.
   Cockroach fill atomically binds the folded application's `K` with its
   initial finalized snapshot, and setup cannot complete until both exist. The
   scheduler-owned application count, canonical per-input projection,
   standard-recovery suffix rewind/reuse, snapshot count, and catch-up
   validation are implemented. Remaining public protocol work and its sequence
   are owned by the
   [Track 3 ordered handoff](2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff).
   The current public rowid feed has not changed.

   Canonical attribution adds chunk-level SQL inside the existing FULL
   user-op commit — one physical-row lookup plus the next-count reads and
   mapping inserts (three queries per chunk, measured post-attribution
   below); it adds no transaction, fsync, mailbox, or worker. The post-attribution acknowledgement sweep below
   satisfies this hot-path gate. Round-trip remeasurement belongs with the
   later public history/API projection because this foundation does not change
   the current rowid feed or its wire format.

Temporary defensive overlap is allowed during branch-only steps, but each
stage has one documented owner for each decision. Merge only at a complete
guarantee boundary; “delete checks one client at a time” is not itself a
deployable intermediate.

Freeze triggers already landed for batch-tree, promotion, and pending-clear
mutations remain the immediate persisted R2 freeze. They are deliberately not
generalized to every hot-path table. `DangerDetector` owns prompt runtime
reaction; the lane refuses opportunistically when its existing frontier read
encounters poison.

## Ratified decisions and validation

- stale `Active`: operator-sticky, with acknowledged transition to
  `NeedsRecovery`;
- terminal abort: hard two-second watchdog, armed before audit work;
- `NeedsRecovery`: distinct persisted state; actions always re-derived;
- command boundaries: setup/rebuild, run recovery, maintenance flush, and
  exact acknowledgement keep separate typed protocols; no generic controller;
- maintenance: flush-only, retryable from setup-complete `NeedsRecovery`, and
  never authorized to clear the recovery verdict it began from;
- internal fencing: structured scope + fresh channels, no `RunEpoch`;
- local coordination: SQLite is the durable plane; HTTP ingress ↔ lane is the
  intentional latency-sensitive request/response exception;
- effect authorization: role-local durable linearization points, no
  `LiveKernel` and no separate `EffectGate`;
- R2 policy: complete for simulated-accepted batch content identity at/above
  the anchor, not a general divergence oracle; immediate persisted freeze,
  detector-owned prompt shutdown, typed lane refusal at its existing frontier
  read, manual cockroach repair; the watchdog does not subsume R2 because the
  freeze prevents the offending finalized checkpoint from advancing;
- lane scheduling: one bounded dequeue chunk per fast turn, including rejects,
  then a complete accumulated newly-safe reconciliation turn when the observed
  safe-block delta reaches five; jump directly to the observed tip, never
  synthesize missed frames, and add no timeout/resume protocol under the
  supported-backlog digestibility assumption;
- lane state: `WriteHead` is a trusted coherent cache reconstructed from
  SQLite, not durable authority;
- external history: `(EraId, RecoveryGeneration, ExecutedInputCount)`, with
  the first two fields as the history version and the count as the canonical
  application boundary;
- rebuild operations: explicit fresh/wiped-directory setup, with no automated
  replacement, clone detection, distributed fencing, or partial-fill resume
  state machine;
- latency acceptance: the product contract remains below 500 ms. Benchmark
  results are method-specific; same-host numbers are regression evidence, not
  portable production guarantees.

### Step-4 latency and capacity measurement (2026-08-02)

The comparison used release builds on the same host and the benchmark harness's
self-contained Anvil stack. Each load ran for 30 seconds after a five-second
warmup, using the funded-transfer workload, `max_fee = 1200`, and the same
1,000-account file. The baseline binary was committed pre-step-4 `02fabb0`,
built in an isolated worktree; “current” was the step-4 working tree. Each cell
is one run, so small differences are local run-to-run noise rather than a
statistical performance claim.

The load clients and sequencer share this host. At concurrency 128 → 256 the
machine, not the sequencer in isolation, saturates: additional clients contend
with and starve the sequencer instead of offering proportionally more usable
load. The resulting throughput plateau and latency rise therefore do **not**
identify the sequencer's capacity ceiling. These runs compare revisions under
one local resource profile; a separate-machine load generator is required for
capacity measurement.

| Concurrency | Pre-step ACK tx/s | Current ACK tx/s | Pre-step ACK p99 | Current ACK p99 |
|---:|---:|---:|---:|---:|
| 1 | 73.62 | 73.99 | 17.074 ms | 16.838 ms |
| 64 | 3,795.97 | 3,811.66 | 29.416 ms | 29.556 ms |
| 128 | 8,168.59 | 7,984.14 | 28.644 ms | 29.920 ms |
| 256 | 8,067.49 | 7,940.53 | 49.047 ms | 49.253 ms |

All ACK runs had zero rejections. The secondary feed sweep also matched every
accepted request to a WS event with zero rejections:

| Concurrency | Pre-step submit→WS p99 | Current submit→WS p99 |
|---:|---:|---:|
| 1 | 43.198 ms | 43.625 ms |
| 64 | 52.850 ms | 53.713 ms |
| 128 | 52.684 ms | 51.868 ms |

Conclusion: step 4 causes no material accepted-path regression in this profile
and remains far inside the 500-ms ACK contract. The historical “roughly 14 ms”
observation needs a metric qualifier: this harness measured current
concurrency-1 HTTP ACK p50 at 13.231 ms, but submit-to-matching-WS-event p50 at
25.313 ms. Reject-heavy frontier progress is enforced separately by the
saturated-queue regression test; the accepted-only harness cannot prove it.

### Step-6 attribution remeasurement (2026-08-03)

The same release ACK sweep was repeated after canonical executed-input
attribution landed, using the same host, self-contained Anvil stack,
five-second warmup, 30-second loads, funded-transfer workload,
`max_fee = 1200`, and account file. “Step 4” below is the prior current column;
“post-attribution” is one new run, so the comparison is regression evidence,
not a statistical performance claim.

| Concurrency | Step-4 ACK tx/s | Post-attribution ACK tx/s | Step-4 ACK p99 | Post-attribution ACK p99 |
|---:|---:|---:|---:|---:|
| 1 | 73.99 | 80.79 | 16.838 ms | 16.427 ms |
| 64 | 3,811.66 | 4,085.12 | 29.556 ms | 29.504 ms |
| 128 | 7,984.14 | 7,901.99 | 29.920 ms | 32.532 ms |
| 256 | 7,940.53 | 7,659.56 | 49.253 ms | 56.963 ms |

Every post-attribution run completed with zero rejections. The lower-load
points were flat or better. At 128 and 256, shared-host client contention
produced the machine-saturation behavior described above while remaining below
57 ms p99. This finds no material revision-to-revision regression and leaves
ample margin under the 500-ms ACK contract; it does not claim a server capacity
limit.

A deliberately oversized slow reconciliation turn is not a preemption gate:
the supported-envelope assumption is that all L1-fit work is promptly
digestible. Revisit that assumption only if production measurements disprove
it. Still to design with the consumer: the current-era bootstrap API/artifact
after cockroach recovery.
