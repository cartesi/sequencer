# Automatic Recovery

Automatic recovery repairs the sequencer's optimistic history after a liveness
failure. It stops issuing soft confirmations, settles outstanding submissions
when necessary, and replaces the affected suffix before resuming. It uses the
existing SQLite database and assumes the sequencer's own code and accepted
local history are correct.

For lost or unusable local state, including after a sequencer bug, use
[cockroach recovery](cockroach.md): fix the bug, choose a trusted canonical
application checkpoint, and rebuild from L1 in a fresh data directory. Automatic
recovery does not establish trust in a corrupted application state.

This page owns the automatic procedure and its rationale. Start with the
lifecycle and dispatch below; read the safety arguments and model boundaries
when changing recovery. The [scheduler contract](../protocol/scheduler-semantics.md)
owns canonical acceptance rules; the [invariant register](../invariants.md) owns
cross-module enforcement.

## The state being repaired

The local batch tree has one valid path: an **accepted prefix**, followed by an
**optimistic suffix** ending at the open **Tip**. Recovery invalidates a suffix
and opens a new Tip from the surviving path. Invalidated batch, frame, and
user-op source facts remain available for audit.

“Accepted” (also called **Gold** in code and models) means the safe-input
projection applied the scheduler's acceptance rules and matched the landed
bytes to a valid local closed batch. It does not mean an independent canonical
machine was observed executing it. A foreign or different accepted payload
records canonical divergence and forbids automatic repair.

Keep three identities separate:

| Identity | Meaning during recovery |
|---|---|
| Local `batch_index` | Unique creation identity; never reused. Invalidation targets a local suffix. |
| Scheduler batch nonce | Ordering identity; derived when storage creates the batch. A replacement branch reuses the invalidated suffix's nonces. |
| L1 wallet nonce | Transaction slot; a batch transaction and a flush no-op may compete for it. Covered slots must be consumed at safe depth before post-flush repair. |

A parentless root uses the deployment's immutable anchor nonce: zero at genesis,
or the scheduler's next nonce after cockroach recovery. Production needs no
accepted ancestor or submitted sentinel to repair a fully invalidated branch
([I16](../invariants.md#i16-the-batch-tree-has-exactly-one-valid-parentless-root-carrying-the-deployments-anchor-nonce)).

## Lifecycle and startup dispatch

The recovery cycle crosses a process boundary:

1. **Detect and stop.** `DangerDetector` polls local `Storage::check_danger`;
   any non-`Safe` result stops normal operation. It neither writes the database
   nor calls L1. Expected-recovery and retryable exits close intake and drain
   workers; diagnosed terminal runtime faults abort immediately.
2. **Respawn.** The orchestrator restarts expected recovery (`10`) and retryable
   exits (`20`). Terminal exit `30` or `SIGABRT` requires investigation before a
   deliberate restart. Process
   ownership and shutdown belong to the [authority-boundary ADR](../plans/2026-08-authority-boundary-adr.md).
3. **Recover under the process lock, with no workers.** Check local terminal
   facts before any provider call, attempt initial L1 sync, select at most one
   repair, then inspect again after a repair.
4. **Prepare, admit, launch.** Prepare resources without starting tasks. A final
   current inspection must still authorize serving. It creates a single-use
   `RuntimeAdmission` witness, consumed synchronously by worker launch.

Startup first refuses a persisted canonical divergence or a missing rollback-safe
checkpoint. After initial sync, one consistent `RecoveryInspection` selects:

| Local fact | Action |
|---|---|
| `Safe` + open Tip | Ready for runtime preparation. |
| `Safe` + no Tip | `EnsureOpenTip`: create it under a transaction guard. |
| `TipInDanger(N)` | `RecoverTip { N }`: invalidate that Tip and reopen, without flushing. |
| `ClosedBatchInDanger(N)` | Flush → sync → guarded post-flush cascade. |
| `L1ViewStale` | Retry; the persisted view or clock cannot authorize serving. |
| `EstimatedBatchInDanger(N)` | Retry; an estimate alone cannot authorize invalidation. |
| `CanonicalDivergence(N)` or missing recovery checkpoint | Refuse; automatic recovery cannot repair this trust failure. |

Only an initial-sync **provider failure** may fall back to a still-usable
persisted view. Other failures keep their typed retry/refuse classification.
Post-flush sync has no such fallback: it must establish the view required by
the cascade.

Every repair must commit an open Tip. A fresh inspection afterward must report
`Safe` with a Tip and no terminal facts; otherwise the boot exits. Startup does
not attempt a second repair in that invocation. Final admission repeats the
same policy after preparation, because preparation can outlive the freshness
of the L1 view. A new repair requirement also exits rather than launching.

Preparation validates the rollback checkpoint's metadata and performs snapshot
hygiene, but does **not** restore the application. After launch, the inclusion
lane loads a surviving snapshot and completes catch-up before processing new user ops.
Admission authorizes worker launch; application restoration can still fail.

Startup logs `danger_status`, `danger_batch_index`, and `recovery_decision`,
then any invalidated indexes. Errors retain their retry/refuse classification
and diagnostic cause; the orchestrator owns restart policy and alert routing.

## Detection and timing

Canonical staleness and local danger use different reference blocks and thresholds:

```text
scheduler rejects a batch with at least one frame when:
    inclusion_block - first_frame.safe_block >= MAX_WAIT_BLOCKS

sequencer observes danger when:
    current_safe_block - first_frame.safe_block >= danger_threshold
    danger_threshold = MAX_WAIT_BLOCKS - preemptive_margin_blocks
```

An old batch can already have landed while fresh. Its inclusion block decides
acceptance; its age at the current safe head does not undo that acceptance.
The detector therefore examines the first unaccepted closed batch and the Tip,
not accepted history. A wire batch with zero frames is never stale and consumes
its nonce; a normal local batch with zero user ops still has a first frame and
can age.

### Danger threshold

The threshold means “stop and recover,” not “this batch cannot land.” The Tip
may still be canonically fresh when invalidated, and closed batches can become
accepted while the flush is running.

The margin provides headroom before canonical expiry; it is not a grace period
after detection. Startup can repair immediately. Defaults and validation live
in [`TimingArgs`](../../sequencer/src/commands/config.rs): with `MAX_WAIT_BLOCKS`
1200 and margin 300, observed danger starts at age 900 blocks. Neither that
margin nor the fee policy bounds the time required to finish recovery.

### When safe-head progress stops

A responsive RPC endpoint can keep returning an old view. The detector uses
both the safe block's timestamp and local time since the last recorded safe-head
advance. [`check_danger_in`](../../sequencer/src/storage/recovery.rs) checks in
this order:

1. Canonical divergence.
2. Missing or old safe-block timestamp → `L1ViewStale`.
3. Observed closed-batch danger, then observed Tip danger.
4. Clock regression of at least one block-time against either persisted time
   baseline → `L1ViewStale`; sub-block skew is tolerated.
5. Estimated missed blocks (`elapsed / seconds_per_block`) reduce the danger
   threshold. An unresolved batch crossing it gives `EstimatedBatchInDanger`.
6. Otherwise `Safe`.

An old view blocks repair selection before observed-age checks. A regressed
clock does not suppress danger already established by observed block numbers;
a remaining clock fault still prevents admission after repair. Estimates stop
new soft confirmations but never decide which work to invalidate.

## Repairs and their guards

### Closed batches: flush, sync, cascade

**Flush the covered wallet slots.** The durable wallet-nonce watermark `W` is an
upper bound on every slot this deployment may have broadcast. Every broadcaster
raises it durably **before** sending at a new nonce. A crash between those steps
may cover a slot that was never used; it must not leave a sent slot uncovered
([I14](../invariants.md#i14-watermark--wallet-nonce-of-every-tx-ever-broadcast)).

The flusher submits zero-value self-transfers at unresolved slots from the
account's Latest nonce through `max(Pending, W + 1) - 1`. It completes only when:

```text
Pending <= Safe  &&  Safe >= W + 1
```

Here Latest, Pending, and Safe are account transaction counts, not block
numbers; absent `W` contributes a lower bound of zero. An original batch or a
no-op may win each slot. Completion means every covered slot is consumed at
safe depth, even if the local node forgot an original transaction. It does not
require erasing that transaction's bytes from every mempool.

The flusher returns the **safe block number** at which it observed completion.
Startup keeps it only in the current call; a crash or retry loses that
observation and the next attempt flushes again. Flush changes no local recovery
facts except the wallet watermark.

**Sync through that observation.** Sync ingests safe InputBox events and updates
the local scheduler-acceptance projection. It does not query a canonical
application machine. A provider failure here retries the boot; there is no
fallback to the pre-flush view.

**Cascade under an immediate SQLite transaction.** Its guard requires no
canonical divergence, a rollback-safe checkpoint, and a persisted safe head at
least as high as the flush observation. Then choose the pivot:

- First valid closed batch beyond the accepted frontier, regardless of age.
- If none remains, the Tip only if it has reached `danger_threshold`.
- Otherwise invalidate nothing, retaining a fresh Tip or opening a missing one.

The cascade deliberately runs even if refreshed danger is now `Safe`. The
closed-suffix policy follows from having completed flush and sync; it does not
repeat the trigger test. There is no extra inspection between flush and sync:
the process lock and absence of workers exclude competing local writers, and
sync is the step that can discover new divergence. Revisit this sequence if
startup gains concurrent writers.

Flush completion depends on L1 progress. Replacement attempts can be rejected
or remain uncompetitive, and provider failures can interrupt the attempt.
Retries preserve safety but establish no recovery deadline. Pricing and its
accepted liveness limits belong to the [L1 fee policy](../l1-fee-policy.md).

### Open Tip: repair without flushing

An open Tip has never been submitted, so invalidating it creates no L1-slot
race. `RecoverTip { N }` rechecks divergence, checkpoint availability, and
**exactly** `TipInDanger(N)` inside its transaction, then invalidates that Tip
and opens a fresh one. It does not invalidate closed batches or fall back to
repairing a changed decision.

The threshold is a policy choice: retaining an aging Tip would make the
runtime detector stop service again. Waiting for `MAX_WAIT_BLOCKS` would keep
the same suspected prediction alive without solving that cycle.

`EnsureOpenTip` is a separate action. Its transaction requires `Safe`, no Tip,
no divergence, and a rollback-safe checkpoint. It opens the Tip without
invalidating history. Guarded writes matter even without concurrent writers:
wall-clock aging alone can change the decision after inspection.

### Atomic history change and replay

Invalidation, history rewind, generation change, and Tip creation share one
transaction. Invalidation removes the suffix's `application_inputs` projection;
raw source facts remain. `RecoveryGeneration` increments once iff at least one
valid batch was invalidated. Each advance appends an immutable generation cut:
the surviving count after suffix deletion, before reopening can insert any
replacement directs. Failed reopening rolls all of this back. The cuts let
readers determine whether a saved checkpoint survived several recoveries;
the [history contract](../protocol/application-history.md#checkpoint-compatibility-after-standard-recovery)
owns that query. Invalidating an empty batch records the old head; a no-op repair
records no transition.

The new Tip follows the latest surviving batch, or uses the immutable root
anchor if none survives. It attributes direct inputs after the surviving
frame's drain boundary, with the era baseline block as a floor. Storage records
those application entries; the launched lane executes them during catch-up.
This prevents a restored prefix's directs from being executed twice.

Recovery requires the latest accepted batch snapshot, or the era baseline
before any local batch is accepted. A newer surviving optimistic snapshot can
reduce replay, but cannot replace that rollback guarantee. Snapshot publication,
artifact validation, leases, and GC are owned by the
[snapshot lifecycle](../snapshots/lifecycle.md); history coordinates and
subscription behavior by the [API contract](../../README.md).

## Why the closed-suffix policy is safe

**Settling slots removes the zombie race.** Before the flush, an old batch may
still win an L1 slot after local invalidation. Reusing its scheduler nonce too
early can let later old batches execute against the replacement branch. The
[historical counterexample](history/README.md) demonstrates this failure.
Detecting danger before settlement is necessary; mutating the closed suffix
before settlement is the unsafe step.

After completion, the original transactions cannot newly win those consumed
slots on descendants of the observed safe chain. Sync through the observation
accounts for the originals that did win. Replacements use later wallet slots
and reuse only the scheduler nonces beyond the accepted prefix. This relies on
the trusted, consistent L1 view and dedicated submitter key in the
[threat model](../threat-model/README.md), and on every broadcaster preserving
the watermark.

**A skipped batch does not advance the scheduler nonce.** When nonce `N` arrives
stale, its frames are skipped and later `N+1`, `N+2`, … envelopes encounter a
nonce mismatch. The overdue-direct backstop still runs before envelope
classification, so “skipped batch” does not mean the whole input has no state
effect. A missing batch whose slot was consumed by a no-op also leaves the
expected nonce unchanged. Later input cannot retroactively make already
rejected envelopes execute.

**Discarding the entire remaining closed suffix is a convergence policy.** It
can include rejected landings, no-op-replaced transactions, and batches never
submitted at all. Some of that work could theoretically be resubmitted fresh;
“everything past Gold is doomed” is not a general impossibility proof. Recovery
sacrifices it to avoid preserving a partly submitted suffix and restarting into
the same danger/flush cycle. The cost is invalidated soft confirmations.

If every closed batch became accepted, an aging Tip can still need repair:
its first frame may share the preceding batch's safe block, while its age is
measured at the later post-flush head. A fresh Tip survives without a generation
change. Thus flush alone does not imply invalidation.

**Content identity is a prerequisite.** The input reader checks at/above-anchor
accepted wire bytes against the local valid closed batch. A foreign or different
payload persists divergence and freezes the acceptance frontier. Startup checks
that marker before L1 access, after sync, inside repair transactions, and before
admission. Repairing the tree's shape cannot recover missing canonical effects;
investigate the fault and use [cockroach recovery](cockroach.md). Check scope
and enforcement are owned by [I9 and I15](../invariants.md).

## Formal Verification

Two bounded TLA+ models check complementary safety obligations. They are not
a refinement proof of the Rust implementation, a proof of their composition,
or a liveness guarantee. Read both before changing recovery code.

### `preemptive.tla`: batches and wallet slots

[`preemptive.tla`](preemptive.tla) models safe-block advancement, wallet-slot
competition between batches and no-ops, scheduler acceptance, and branch
invalidation. Its `Inv` checks `ZombieSafety` at every reachable state:
`schedulerExpected = CountGold(spine)`. It also checks batch-nonce contiguity,
invalid-branch ancestry, wallet-slot uniqueness, and L1/scheduler cursor bounds.

Several details must not be read as literal production behavior:

| Model | Production mapping or limit |
|---|---|
| A Gold genesis sentinel at nonce zero | Production opens a parentless root at its stored anchor, without a submitted sentinel. Root/anchor cases are tested in Rust. |
| `SubmitBatch` assigns the pending suffix with `max(walletNonce, nextL1Slot)` | The poster derives the suffix and Latest account nonce, and raises the durable watermark before sending. The model expression is not a Rust nonce-allocation recipe. |
| Tip advancement and submission can interleave with recovery; dead batches can race after model invalidation | Production stops workers and settles covered slots before the closed cascade. These additional modeled interleavings do not establish coverage of different production actions. |
| `Resolve` handles a stale Silver frontier or a Tip at `MAX_WAIT_BLOCKS` | Production also invalidates a killed/unsubmitted closed suffix after flush and repairs a Tip at the earlier danger threshold. Those actions need the arguments above and Rust tests. |

The model's `Gold`, `Silver`, `Bronze`, `Pending`, and `Tip` colors describe
stages of inclusion and acceptance. `Gold* Silver* Bronze* Pending* Tip` is
**not** an invariant: flushing can leave a killed Pending before a surviving
Silver. Do not build implementation assumptions on that ordering.

The configured finite bounds are in [`preemptive.cfg`](preemptive.cfg).
The model has neither crash/restart nor the wall-clock freshness policy.

### `admission.tla`: startup and permission to launch

[`admission.tla`](admission.tla) models the local terminal gate, initial-sync
fallback, at most one repair, flush observation and mandatory sync, guarded
cascade, post-repair inspection, task-free preparation, and final admission.
Retry, refusal, or owner loss starts a fresh attempt over surviving durable
facts; the flush observation and admission witness do not survive.

Its invariants cover terminal dominance, repair preconditions, a caught-up
post-flush view, and admission soundness. It abstracts successful repair as
producing a Tip; concrete transactions and rollback are checked by Rust tests.
Neither model covers external era/generation metadata, the application-input
projection, or snapshot artifact/lease/GC durability. Those obligations remain
in storage constraints, tests, and the [snapshot lifecycle](../snapshots/lifecycle.md).

Run the configured checks with:

```bash
tlc -workers auto -deadlock docs/recovery/admission.tla
tlc -workers auto -deadlock docs/recovery/preemptive.tla
just -f docs/recovery/justfile check-all
```

## Implementation and test map

| Concern | Owner and useful tests |
|---|---|
| Startup dispatch, error classification, final admission | [`recovery/mod.rs`](../../sequencer/src/recovery/mod.rs); procedure tests substitute only L1 sync/flush, keeping real SQLite inspections and repairs. |
| Detection, mutation guards, pivot and atomic cascade | [`storage/recovery.rs`](../../sequencer/src/storage/recovery.rs), [`recovery_tests.rs`](../../sequencer/src/storage/recovery_tests.rs); exact Tip guard, safe-view floor, unconditional post-flush policy, generation rollback, root nonce, and direct replay. |
| Observed danger versus estimates, accepted frontier and content identity | [`storage/l1_submission.rs` tests](../../sequencer/src/storage/l1_submission.rs), [`safe_accepted_batches.rs`](../../sequencer/src/storage/safe_accepted_batches.rs); stale-view precedence, clock faults, reused nonces, divergence freeze. |
| Slot settlement and broadcast coverage | [`recovery/flusher.rs`](../../sequencer/src/recovery/flusher.rs), [`l1/watermark.rs`](../../sequencer/src/l1/watermark.rs), [`submitter/poster.rs`](../../sequencer/src/l1/submitter/poster.rs). |
| Task-free preparation, launch, restore and catch-up | [`commands/run/`](../../sequencer/src/commands/run/), [`inclusion_lane/mod.rs`](../../sequencer/src/ingress/inclusion_lane/mod.rs). |

The accepted-frontier cache stores acceptances, not scan progress. Rejected
inputs after the frontier may be rescanned on later syncs; a separate persistent
cursor would need nonce-reuse reasoning and tests. This is a storage performance
tradeoff, not an extra recovery phase or a modeled TLA+ invariant.
