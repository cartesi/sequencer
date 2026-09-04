# Batch Recovery

This document describes the recovery design for the sequencer: how the system detects that batches are failing to land on L1, how startup recovers to a consistent state, and where runtime authority begins. Two complementary bounded TLA+ models cover the design: [`preemptive.tla`](preemptive.tla) for batch/slot safety and [`admission.tla`](admission.tla) for startup phase ordering and admission. They do not currently model the external era/generation/base metadata or the derived canonical `executed_inputs` projection; their crash atomicity is enforced by the SQLite transaction boundaries and schema triggers described below.

See `AGENTS.md` "Batch Staleness and Recovery" for quick-reference tables and function names.

## Runtime lifecycle at a glance

The sequencer's recovery loop spans two process lifetimes:

1. **In-process detection.** The `DangerDetector` polls `Storage::check_danger` on a cadence. When any non-`Safe` status fires (`CanonicalDivergence`, `L1ViewStale`, `ClosedBatchInDanger`, `TipInDanger`, or `EstimatedBatchInDanger`), the runtime converts that into `WorkerExit::DangerDetected` under `CommandError::Worker`, closes intake, and drains the workers before returning a non-zero status. Canonical divergence is terminal and arms the independent two-second abort bound; expected-recovery and retryable arms remain cooperatively graceful.
2. **External respawn.** An orchestrator (systemd, k8s, …) restarts the process.
3. **Startup reducer.** The fresh boot reads divergence, danger, finalized-snapshot presence, Tip presence, and the safe head in one local transaction. The pure reducer selects at most one phase. Every completed phase returns to local inspection before another phase or admission. Initial Sync is itself a phase, so an already-persisted divergence refuses before the first provider call.
4. **Prepare, admit, launch.** A clean decision permits task-free, fallible runtime preparation. Startup then invokes the same reducer once more over one consistent fact set, mints the single-use `RuntimeAdmission` witness, and consumes it in an infallible, non-yielding worker launch.

The detector trip and the startup dispatch share the same `check_danger` function; the detector cares only that *some* arm fired, while the startup dispatch examines *which* arm fired to pick the right action.

Key abstractions, by responsibility:

- **`DangerDetector`** ([`recovery/detector.rs`](../../sequencer/src/recovery/detector.rs)): tiny background task that calls `Storage::check_danger` on a cadence. Never writes to the DB, never talks to L1. Exits with `DetectorExit::RecoveryRequired` when any non-`Safe` status fires. The runtime converts that into a `WorkerExit::DangerDetected` worker exit, requests process-wide drain, and returns non-zero after cleanup. A terminal classification gets the hard two-second abort fallback; ordinary recovery does not. The reducer re-derives the authoritative response from fresh facts on the next boot.
- **`BatchSubmitter`** ([`l1/submitter/worker.rs`](../../sequencer/src/l1/submitter/worker.rs)): makes L1 progress only — never checks danger. Productive ticks re-enter immediately; idle/transient ticks sleep `idle_poll_interval`. A pure `decide_submit_start` function folds observed L1 nonces over the scheduler-accepted frontier.
- **Startup recovery reducer** ([`recovery/mod.rs`](../../sequencer/src/recovery/mod.rs)): pure policy over one `RecoveryInspection` plus boot-local phase progress. It selects `Admit`, one phase, `Retry`, or `Refuse`. The production driver owns exhaustive error classification; raw provider/storage/flush errors do not escape to a second recovery classifier.
- **Guarded recovery storage** ([`storage/recovery.rs`](../../sequencer/src/storage/recovery.rs)): reads the reducer facts in one transaction and reasserts the selected mutation's durable preconditions in its write transaction. Divergence is checked before the flush-view coherence check and before every batch-tree mutation.
- **`MempoolFlusher`** ([`recovery/flusher.rs`](../../sequencer/src/recovery/flusher.rs)): submits no-op transactions to consume all pending wallet-nonce slots and waits for safe finality. Does **not** retry internally on provider errors — the orchestrator's respawn loop is the retry mechanism.
- **`ProtocolTiming`** ([`sequencer-core/src/protocol.rs`](../../sequencer-core/src/protocol.rs)): single source of truth for scheduler timing (`max_wait_blocks`) plus the sequencer-local tuning knobs (`preemptive_margin_blocks`, `l1_read_stale_after_blocks`, `seconds_per_block`). The batch-submitter address is deployment identity and is passed separately to `scheduler_accepts`.

These pieces remain independently testable: the decision is pure, the phase driver has a discriminating trace, storage returns a fact struct rather than ad-hoc tuples, and the detector/submitter remain separate workers.

## The Batch Tree

Batches form a tree where each node is a batch and edges point from child to parent. Each batch has a single parent: the preceding batch in the valid chain.

Batches have two identifiers:

- **Index** (`batch_index`): monotonically increasing, unique, never reused. Creation order.
- **Nonce** (`batch_nonce`): depth of the node in the tree. Assigned by the batch submitter to valid closed batches.

In normal operation the tree degenerates into a list -- index and nonce increase in lockstep. Branches appear only after recovery, when a suffix of the chain is invalidated and a new batch forks from the last valid ancestor.

There is always exactly one **valid path** (root to leaf) that constitutes the current batch chain. The valid path splits into a **prefix** (safe on L1, accepted by the scheduler) and a **suffix** (pending or confirming).

### Genesis sentinel (nonce-0 edge case)

Recovery requires at least one Gold ancestor (the cascade invalidates a suffix and forks from the last Gold batch). If the very first batch (nonce 0) goes stale before any batch becomes Gold, there is no ancestor to fork from.

The TLA+ model handles this with a **genesis sentinel**: the initial state starts with a Gold batch at nonce 0. This is a modeling technique that eliminates the nonce-0 special case, allowing Resolve to use uniform logic (the `fng > 1` guard is always satisfied). Without it, the model would need a separate Resolve action with different arithmetic for the "no Gold ancestor" case.

The implementation handles the nonce-0 case **structurally**: `open_fresh_tip_in_tx` (`storage/ingress.rs`) roots a nonce-0 batch whenever the valid path is empty (genesis, or a fully-torn cascade) — no sentinel batch is submitted and no recovery branch is special-cased. The model's sentinel and the implementation's structural root play the same role.

#### Cockroach recovery generalizes the root nonce (the anchor)

Cockroach recovery (`setup --recovery`) rebuilds a wiped DB from a trusted checkpoint and must resume submitting at nonce `N'` without replaying history — so the rebuilt tree is rooted at `N'`, not 0. Rather than plant a fake "sentinel" batch at `N'-1`, the batch-tree anchor generalizes the structural root: a `batch_tree_anchor` singleton holds the nonce the parentless root carries (default `0`; recovery sets `N'`). The same `open_fresh_tip_in_tx` / `compute_next_nonce(parent = None)` path then roots `run`'s first tip at `N'`, and `trg_enforce_nonce_contiguity` validates the root against the anchor (exact match) instead of a hard-coded 0. There is **no sentinel batch row** — the root tip *is* the anchored batch. Normal deployments keep anchor `0` and are byte-identical. See [I16](../invariants.md) and the [cockroach-recovery design](#cockroach-recovery-setup---recovery) below.

A sealed `N'-1` sentinel was considered and rejected: a valid closed batch at `N'-1` is a legal cascade pivot, so a runtime cascade could invalidate it and leave the tree re-rooting at 0 (ABORTed by the unchanged contiguity trigger) — an unguarded reliance on "the frontier never drops to `N'-1`". The anchor has no such hidden dependency.

## Coloring

Every batch on the valid path has exactly one color. Dead branches are lead (permanently invalid).

### Simplified model (three colors)

| Color      | Meaning                                                        | Terminal? |
|------------|----------------------------------------------------------------|-----------|
| **Gold**   | Safe on L1 and accepted by the scheduler                       | Yes       |
| **Silver** | Valid, optimistically executed, but not yet safe/accepted      | No        |
| **Lead**   | Invalid (has `batches.invalidated_at_ms` set)                  | Yes       |

Gold batches form a contiguous prefix of the valid path. Silver batches form a contiguous suffix (after the gold prefix up to the open batch). Lead batches hang off gold nodes as dead branches -- the first lead in any cascade always has a gold parent.

### Extended model (five colors)

To model the full lifecycle including L1 submission:

| Color       | Meaning                                                | Has `w_nonce`? |
|-------------|--------------------------------------------------------|----------------|
| **Tip**     | Open batch, not yet closed                             | No             |
| **Pending** | Closed, may or may not be submitted to mempool         | Maybe          |
| **Bronze**  | Included in an L1 block, block not yet safe            | Yes            |
| **Silver**  | Included, block has reached safe finality              | Yes            |
| **Gold**    | Safe, accepted and executed by the scheduler           | Yes            |

The spine ordering invariant: `Gold* Silver* Bronze* Pending* Tip`

A Pending batch may have a `w_nonce` (submitted to the L1 mempool but not yet included in a block) or not (not yet submitted). The batch submitter assigns `w_nonce`s to all unsubmitted Pending batches at once, in spine-position order.

## Nonce Poisoning

The scheduler maintains a single counter: "I expect batch nonce N next."

When a batch with nonce N arrives stale, the scheduler **skips it entirely** -- no nonce increment, no state change, no report. It is a true noop in nonce-space.

This poisons the nonce counter. Every subsequent batch (nonce N+1, N+2, ...) is dead on arrival. Not because they are individually stale, but because the scheduler still expects nonce N. The only batch with nonce N was stale and skipped, so the counter will never advance past N.

Cascade invalidation is therefore **exact, not conservative**. The sequencer's `WHERE batch_index >= stale_batch_index` mirrors precisely what the scheduler will do (refuse). The entire silver suffix is unreachable once any batch in it is stale.

Recovery is the only way forward: create a new batch with nonce N, giving the scheduler what it needs to resume.

## Two Staleness References

The staleness formula is `reference_block - first_frame_safe_block >= MAX_WAIT_BLOCKS`, but the reference block differs by context:

### Inclusion staleness (scheduler's perspective)

```
inclusion_block - first_frame_safe_block >= MAX_WAIT_BLOCKS
```

Used by `populate_safe_accepted_batches` to simulate what the scheduler accepts. Each batch has its own inclusion block (the L1 block where its submission landed). **Not monotonic** across batches -- a promptly submitted old batch can be healthy while a late-submitted newer batch is stale.

Inclusion staleness determines the **gold frontier**: the set of batches the scheduler has accepted.

### Current staleness (sequencer's detection)

```
current_safe_block - first_frame_safe_block >= MAX_WAIT_BLOCKS
```

Used by the danger threshold detector. The reference block (`current_safe_block`) is the same for all batches. **Monotonic within the valid path** -- earlier batches have smaller `first_frame_safe_block`, so larger difference. If the frontier batch is not stale by this measure, no batch is.

Current staleness triggers **preemptive recovery** (see below).

## Nonce Uniqueness on the Valid Path

`batches.nonce` can repeat across the full table -- a recovery batch inherits `parent.nonce + 1` from the last valid ancestor, which is the same nonce the first invalidated suffix batch had. Among **valid batches** (those with `invalidated_at_ms IS NULL`), nonces are unique because the valid path is a strict chain via `parent_batch_index`.

This matters because L1 works in nonce-space (the scheduler identifies batches by nonce) while the sequencer works in index-space (local `batch_index`). The recovery path needs to translate between them: "which batch indexes should we invalidate?" Nonce uniqueness on the valid path is what makes this mapping unambiguous.

## The L1 Stream

L1 processes transactions in `w_nonce` order. At each slot (a given `w_nonce` value), exactly one transaction is included. If multiple transactions compete for the same slot (e.g., a dead batch and a flush no-op), L1 non-deterministically picks one. The loser is discarded.

This is the interface between the sequencer and the scheduler. The scheduler sees a stream of entries ordered by `w_nonce`, each with a `batch_nonce`, `inclusion_block`, and `safe_block`. It processes them in order, accepting or rejecting based on nonce match and staleness.

## The Uncertainty Interval

The core insight behind the recovery design is that **mempool uncertainty is bounded by a time interval**.

Once a batch's `safe_block` is old enough that `current_safe_block - safe_block >= MAX_WAIT_BLOCKS`, we know it is stale no matter when it lands on L1 (because `inclusion_block >= current_safe_block`). Any batch in the mempool with that `safe_block` is dead-on-arrival. This means mempool uncertainty has a natural expiration: after `MAX_WAIT_BLOCKS`, the L1 outcome doesn't matter.

This gives us three regimes:

```
|---------- safe ----------|-- danger zone --|-- past MAX_WAIT --|
        no action             flush + recover     self-resolved
```

- **Before the danger zone**: batches are young. Nothing to do.
- **In the danger zone**: batches might land stale, or might still make it. This is the window of uncertainty. For **closed unresolved batches**, the flush resolves it by forcing every `w_nonce` slot to finalize (batch wins or no-op wins). After the flush, the sequencer reads the scheduler's finalized state and cascades if needed. An **open Tip** has no `w_nonce` slot yet, so it is not part of this uncertainty set.
- **Past MAX_WAIT**: all unresolved batches are guaranteed stale by L1 monotonicity (`inclusion_block >= current_safe_block >= safe_block + MAX_WAIT`). For closed unresolved batches, the L1 outcome no longer matters because every eventual inclusion is stale, but wallet-nonce slots may still need to be flushed (or naturally consumed) before recovery can reconstruct the scheduler frontier. For an aging open Tip, there is no L1-slot uncertainty at all, so startup recovery can invalidate it directly.

**What TLA+ proves vs external reasoning**: the TLA+ model ([`preemptive.tla`](preemptive.tla)) proves that after all `w_nonce` slots are resolved (however that happens), ZombieSafety holds. It does not model the danger threshold or the passage of time. The claim that "past MAX_WAIT, staleness self-resolves" is an external argument from L1 monotonicity (`inclusion_block >= current_safe_block`), not something TLA+ checks.

Any recovery design must wait out this uncertainty. The question is how. The preemptive design (implemented here) forces resolution by going offline and flushing. An alternative optimistic design lets the uncertainty resolve naturally but keeps serving soft confirmations -- see [`history/`](history/) for that approach and why we preferred preemptive.

## Silver-Only for Submitted Batches

The Silver-only constraint applies to **submitted batches whose L1 slot outcome is still relevant**. This is the zombie path, and it is where the optimistic-design counterexample from [`history/`](history/) still matters.

A Silver batch's L1 entry is permanent -- no mempool competition can kill it. The scheduler **will** see it, at a `w_nonce` lower than any recovery batch, and be poisoned. This ordering guarantee is what makes nonce poisoning reliable.

Detecting staleness on Pending or Bronze submitted batches *before wallet-nonce uncertainty is resolved* is unsafe: a recovery batch can take the frontier's L1 slot via wallet-nonce mutual exclusion, preventing the scheduler from ever seeing the stale frontier, and allowing non-frontier dead batches to pass the nonce check. TLA+ model checking found this bug; see [`history/`](history/) for the counterexample.

The open Tip is different. It has no L1 transaction yet, so there is no `w_nonce` competition and no zombie risk. Once `current_safe_block - first_frame_safe_block >= danger_threshold`, startup recovery can invalidate the aging Tip directly and open a fresh one. Likewise, after a preemptive flush has resolved all competing `w_nonce` slots for closed batches, the atomic recovery transaction can safely use **current staleness** on the oldest unresolved batch (closed or open).

## Preemptive Recovery Design

The sequencer uses a preemptive approach: detect danger early, go offline, flush the mempool, then recover on solid ground. This design was preferred over the optimistic alternative because it is simpler to reason about and produces fewer invalidated soft confirmations (the sequencer stops issuing them before the cascade).

### Step 1: Danger threshold

Define `DANGER_THRESHOLD = MAX_WAIT_BLOCKS - MARGIN`. When the frontier batch's current staleness (`current_safe_block - safe_block`) reaches `DANGER_THRESHOLD`, **trigger preemptive recovery**.

The threshold is *only* a trigger. It says "stop running, hand off to recovery." It does **not** say "this batch is doomed." The cascade decision belongs to step 5, which examines the post-flush state and acts on what's actually there.

#### Why a margin at all (Sorites argument)

The right value of `MARGIN` is not derived from the recovery procedure's runtime — it falls out of a sharper question: **at what age do we give up on the current batches and start anew?**

Two endpoints are clear:

- A batch that's 1 minute behind shouldn't be invalidated. The infra hiccup might pass; pre-confirmations issued against it will likely still land.
- A batch that's 1 minute *before* `MAX_WAIT_BLOCKS` shouldn't be left to die. We've already tried for hours. The last minute won't save us, and pre-confirmations issued in this window are knowingly dishonest — we have strong evidence they won't land.

Somewhere between those, we want to switch from "keep waiting" to "give up." The exact crossover is a Sorites question with no canonical answer, but two design pressures pin it:

1. **Stop issuing pre-confirmations on state we reasonably know won't land.** As current staleness approaches `MAX_WAIT_BLOCKS`, the probability that the current batch lands gracefully drops. Pre-confs issued past that point are increasingly dishonest to users.
2. **Give the operator runway to fix infra.** If L1 is misbehaving, network is degraded, mempool is congested — the operator needs hours, not minutes, to diagnose and act before the system commits to recovery and invalidates work.

The recovery procedure's own runtime (flush submission + L1 safe finality wait of ~13 min on Ethereum + atomic SQLite cascade) is a *floor* on `MARGIN`, not the deciding factor. It must fit, but fitting it is far from the operating point.

#### Defaults

With `MAX_WAIT_BLOCKS = 1200` (~4 hours), the default `MARGIN = 300` blocks (~1 hour at 12s/block) gives the operator ~1 hour after danger-zone entry before the system commits to recovery. That's well above the procedure-runtime floor (~15 min) and meaningful runway under the second design pressure.

Production tunings with a longer `MAX_WAIT_BLOCKS` (e.g. 24h) should keep the margin in the hours range — there's no benefit to a tighter margin once `MARGIN` exceeds the procedure-runtime floor several times over.

### Step 2: Go offline

Stop accepting new user operations. From the outside world, the sequencer is temporarily unavailable. This eliminates concurrent batch creation during recovery.

### Step 3: Flush mempool

Read the persisted **wallet-nonce watermark** `W` — the highest `w_nonce` this deployment ever broadcast (`wallet_nonce_watermark` singleton; see Implementation Constraint 1). Query the latest confirmed `w_nonce` (N) and the pending `w_nonce` (M). Submit no-op transactions (self-transfers of 0 ETH) at nonces N, N+1, ..., `max(M, W+1) - 1`. These compete with any of our transactions still alive anywhere in the network — including zombies the local node's pool has forgotten.

Wait until both `pending <= safe` **and** `safe >= W + 1`: every slot this deployment ever used is consumed at safe depth. The second conjunct is the durable anchor — without it the flush trusts the local node's volatile mempool memory, which a dropped-locally-but-alive-elsewhere zombie evades entirely. The flush reports the safe block at which it observed resolution; Step 5 refuses to cascade until the re-synced view reaches at least that block.

### Step 4: Post-flush state

Every `w_nonce` slot from N to M-1 is now resolved:

- **Batch won**: the batch is on L1 and safe (Silver or Gold)
- **No-op won**: the batch is dead forever, its slot consumed

There are no more mempool entries. All uncertainty is resolved.

**Flush safety does not depend on eviction.** A no-op may fail to evict a still-pending batch tx (e.g. our local node rejects the replacement under EIP-1559's ≥10% bump rule). That's fine: a rejected send surfaces as a hard `FlushError` and the process exits, the orchestrator respawn re-runs the flush, and *eventual* inclusion of either the original batch tx or the no-op resolves the slot — the unbounded retry lives in the respawn loop, not inside `flush_and_wait`. Safety holds regardless of which lands; eviction is only an operational efficiency concern.

### Step 5: Run recovery

This is an atomic SQLite transaction operating on the best available L1 state. The storage work splits cleanly by whether a flush ran first.

#### Mental model: "everything past gold is doomed"

After the flush has resolved every wallet-nonce slot, and `populate_safe_accepted_batches` has been re-synced, the gold spine is at its **maximum extent**: the simulation walked safe-inputs in inclusion order, accepting each one until it hit a barrier (a stale batch, or a missing batch where a no-op consumed the slot).

Any batch past that gold frontier is **doomed**, in one of three concrete senses:

| State | What happened | Why doomed |
|---|---|---|
| **Silver-stale** | Original tx landed, scheduler skipped (`inclusion_block - first_frame ≥ MAX_WAIT`) | Scheduler's expected nonce never advances past it; downstream batches are nonce-poisoned |
| **Silver-fresh poisoned** | Original tx landed fresh, but a preceding stale or missing batch poisoned the nonce | Scheduler skipped on nonce mismatch; on-chain row can't be retroactively re-evaluated |
| **Pending (no-op'd)** | Flush no-op consumed the wallet-nonce slot; original tx never landed | The L1 transaction is dead. Re-submission at a fresh slot would land *after* the existing on-chain Silver-poisoned batches; the scheduler sees those at lower `safe_input_index`, advances expected past them on the resub generation, but the per-original-tx work is gone |

**Why isn't this just "stale"?** Under self-trust (we don't defend against malformed self-submissions), the *first* non-gold closed batch can only be Silver-stale or Pending. Nonce-mismatch is impossible at the frontier — nonces are contiguous on the valid path (`trg_enforce_nonce_contiguity`). But *downstream* batches past that first non-gold are typically Silver-fresh-poisoned: their inclusion-staleness was fine, but they were processed when expected was stuck at the poisoned nonce.

A **fourth shape** sits outside this taxonomy: a closed batch that was **never submitted** (closed after the submitter's last tick before the detector exit). It has no L1 footprint, no killed tx, and is not literally doomed — it could simply be submitted after recovery. The cascade invalidates it anyway: once committed to recovery, cascading the entire non-gold suffix converges in one cycle and avoids spine-order reasoning about a half-submitted suffix. The cost is real (its soft confirmations are rolled back); this is a deliberate convergence-over-preservation policy choice.

Cascading from the first non-gold catches all four. **No per-batch age check is needed for the cascade pivot itself** — every closed batch past gold is either doomed by construction or sacrificed by the convergence policy.

#### Path A — guarded post-flush Cascade

After step 3 (flush) and step 4 (re-sync), the gold frontier is fresh. Run the atomic recovery transaction:

1. **Find the cascade pivot.** First try the closed pivot: first valid closed batch with `nonce >= frontier_nonce`. By the contiguity invariant, this batch's nonce is exactly `frontier_nonce`. If one exists, cascade from it.
2. **No closed pivot? Check the Tip.** When all closed batches landed fresh and were accepted (the "everything worked" aftermath), there's no closed pivot — but the Tip can still be in the danger zone. When the lane rotates without a safe-block advance between frames (e.g. immediately after init, both frames share the bootstrap `safe_block`), `S_tip = S_closed`. The closed batch can become gold by inclusion-staleness while the Tip's age — measured against `current_safe_block` after the flush wait — has crossed the danger zone. Pure monotonicity (`S_tip ≥ S_closed`) doesn't rule this out: equality is allowed. So fall through to `find_tip_batch_in_danger(danger_threshold)`. If the Tip's age clears `danger_threshold`, cascade it.
3. **Cascade-invalidate the suffix**: set `invalidated_at_ms` on every valid batch with `batch_index >= pivot.batch_index`. This catches all non-gold batches in cases (2)/(3) above, and the Tip alone in the no-pivot-but-Tip-aging case. The invalidation trigger retains physical replay rows but deletes their derived `executed_inputs` mappings, rewinding canonical head `H` to the surviving prefix.
4. **Advance external history reality**: iff step 3 invalidated at least one valid batch, increment `RecoveryGeneration` exactly once in this same SQLite transaction. A no-invalidation repair does not bump it. Mapping rewind and generation change are therefore one visible transition.
5. **Open recovery batch**: parent is the last valid ancestor (`MAX(batch_index) FROM valid_batches` after the cascade). Nonce is structurally `parent.nonce + 1`, which equals `frontier_nonce` — the scheduler's `expected_nonce`. Re-drain direct inputs from the invalidated batches starting at `max(base_safe_input_index, MAX(valid safe_input_index) + 1)`. Their new physical rows reuse the rewound logical offsets under the incremented generation.

**Threshold = `danger_threshold`, not `MAX_WAIT_BLOCKS`**. We're already committed to recovery; the Tip is past gold; if it's also past the threshold that would have triggered recovery had it been a closed batch, cascade it. Otherwise the next danger detector tick after resume would re-trip on the Tip's eventual close + submission anyway (the closed batch would inherit its first frame's safe_block).

#### Path B — guarded `RecoverTip`

The `RecoverTip` action is dispatched when `check_danger` returns `TipInDanger(idx)`: no closed batch is past the gold frontier in the danger zone, but the open Tip's first frame has aged past `danger_threshold`. **No flush ran** — the Tip has no L1 footprint, so there's nothing to flush.

Closed batches past gold (if any) are still in their natural lifecycle — pending in the mempool, recently included, awaiting safe finality. Cascading them would prematurely abort their progression. We act only on the Tip:

1. Run `find_tip_batch_in_danger(danger_threshold)`. If `Some(tip_index)`, cascade-invalidate from there (which only touches the Tip — no closed batches have `batch_index >= tip_index`) and increment `RecoveryGeneration` exactly once in that same transaction.
2. Open a fresh recovery batch in the same transaction.
3. If no Tip in danger and no Tip exists at all (torn-state crash recovery), open a Tip anyway.

The `Safe` decision with no open Tip selects `EnsureOpenTip` as its own reducer phase. That phase rechecks `Safe`, finalized-snapshot presence, and Tip absence in the write transaction, then uses the shared `open_fresh_tip_in_tx` mechanism, and re-reads the Tip inside that transaction after opening: it refuses (terminal) rather than commit without one, so the reducer's single `Repaired` → `EnsureOpenTip` → `Repaired` edge cannot cycle. Tip creation is therefore inside the same inspect → one phase → inspect discipline, never a worker-construction side effect.

#### Why `danger_threshold`, not `MAX_WAIT_BLOCKS`, for the Tip threshold

The Tip threshold is a **policy choice**, not a mathematical staleness bound. A Tip whose first frame is at age `danger_threshold` could in principle still close, submit, and land fresh by inclusion-staleness — `inclusion_block - first_frame` would be roughly `danger_threshold + (rotation + submit latency)`, which (with a reasonable margin) is still below `MAX_WAIT_BLOCKS`.

We invalidate at `danger_threshold` because:

1. **Pre-confirmation honesty.** Once the Tip's age crosses the danger zone, the system has decided this generation is operationally suspect. Continuing to issue soft confirmations against it is dishonest to users.
2. **Avoid retrip risk.** The runtime danger detector also fires on `DangerStatus::TipInDanger`. Without invalidating at startup, we'd resume operation, the detector would re-trip on the next tick, and we'd cycle. Cascading at startup converges in one cycle.
3. **Symmetry with the closed-batch trigger.** The closed-batch detector trips at `danger_threshold`. Using the same threshold for the Tip preserves the framing: "danger zone = committed to recovery."

### Step 6: Resume

Restart the batch submitter and user-op acceptance. If this recovery invalidated
any valid batch, the generation bump already committed atomically with that
invalidation; otherwise the history version is unchanged. The sequencer is
back online.

### Why post-flush cascade is unconditional (and not threshold-based)

An earlier design considered using `MAX_WAIT_BLOCKS` as the cascade threshold even in the post-flush path: only invalidate the frontier if its `current_safe_block - first_frame.safe_block ≥ MAX_WAIT`. The intuition was to preserve soft confirmations when re-submission could still land fresh.

**This doesn't hold up.** Walk through the boundary case:

1. Frontier batch has `current_staleness ∈ [danger_threshold, MAX_WAIT)`. Detector trips, flush runs.
2. `recover_post_flush` (with hypothetical threshold) sees age below MAX_WAIT, declines to cascade. Resume.
3. Submitter wakes up, resubmits the Pending frontier (and any non-gold closed batches) at fresh wallet-nonce slots. They enter the mempool.
4. Detector polls again. Frontier age has barely moved or the published safe
   head is unchanged; providers may later expose several newly-safe blocks as
   one jump, but no cadence assumption makes the frontier clean again. It is
   still above `danger_threshold`, so the detector trips again.
5. Recovery 2 starts. Flush submits no-ops at the slots the submitter just used for resubs. Bumped fees on no-ops typically out-bid resubs. Resubs killed.
6. Goto step 2. Loop converges only when `current_staleness` finally crosses `MAX_WAIT_BLOCKS` and the threshold check fires.

Each loop iteration burns gas (no-ops + doomed resubs), takes ~12 minutes (the flush's safe-finality wait), and the soft confirmations are rolled back at the end anyway. Cascading on first non-gold converges in **one cycle** with predictable cost.

### Startup behavior summary

The first local inspection always ranks `CanonicalDivergence` and missing finalized state ahead of phase progress. If neither terminal fact exists, `NeedInitialSync` selects the initial Sync phase. A provider failure during that one phase may still admit a warm database whose persisted view remains fresh; every non-provider reader failure is classified terminal or retryable by its typed provenance.

After the initial Sync attempt, ordinary inspection maps facts as follows:

| Local fact | Reducer decision | Why |
|---|---|---|
| `Safe` + open Tip | `Admit` | The local prediction is clean and structurally resumable. |
| `Safe` + no Tip | `EnsureOpenTip` | Open the genesis/torn-state Tip under the phase guard, then re-inspect. |
| `L1ViewStale` | `Retry` | The persisted view cannot honestly authorize new soft confirmations. |
| `TipInDanger(N)` | `RecoverTip { N }` | The Tip has no L1 footprint; invalidate and reopen directly, then re-inspect. |
| `ClosedBatchInDanger(N)` | `Flush` | Closed batches have uncertain L1 slots that must be resolved before tree mutation. |
| `EstimatedBatchInDanger(N)` | `Retry` | Observed safe state did not cross danger; recovery never mutates from an estimate alone. |
| `CanonicalDivergence(N)` | `Refuse` | Standard recovery assumes content identity and is forbidden. |

Closed recovery is structurally `Flush → inspect → post-flush Sync → inspect → Cascade → inspect`. Flush produces a boot-local witness carrying its observed safe block. The post-flush Sync preserves that witness, and Cascade is selected only if the persisted safe head caught up through it. A crash drops the witness, so the next boot repeats the idempotent flush instead of trusting a half-remembered phase. The guarded Cascade transaction checks divergence first, then the required finalized-state fact and the flush-view floor, then mutates.

**Observed repair still outranks clock refusal.** `check_danger` evaluates observed closed/Tip danger before local-clock faults. Once an observed danger selected a repair, the reducer finishes that repair even if the clock arm is also active; the next mandatory inspection returns `Retry` rather than admitting. A successful repair is never itself an admission fact.

After the first clean decision, runtime preparation launches zero tasks. The same reducer is invoked again after preparation, over one transactionally consistent fact set — the process lock plus the task-free prepare phase make that read the decision's linearization. Only another `Admit` decision mints the single-use `RuntimeAdmission` witness; launch consumes it synchronously. Raw component launch functions are crate-private, so external app crates can enter the runtime only through `run`/`run_main`. Runtime mutation and output authorization remains role-local at the durable boundaries in the authority ADR; the reducer establishes admission, not a new global authority service.

The two formal models split responsibility deliberately: `preemptive.tla` proves slot/batch safety, while `admission.tla` proves local-first terminal dominance, one-phase-per-inspection ordering, witness requirements, crash/restart soundness (a crashed attempt leaves nothing behind that gates the next boot), and capability soundness. The “everything past gold is doomed” policy argument remains external to both bounded models.

### Startup observability

Startup recovery logs each reducer decision and repair outcome with stable structured fields:

- `danger_status` — `safe`, `l1_view_stale`, `closed_batch_in_danger`, `tip_in_danger`, or `estimated_batch_in_danger`.
- `danger_batch_index` — set for batch-specific danger statuses.
- `recovery_progress` — initial sync, ordinary inspection, flushed, post-flush synced, or repaired.
- `recovery_decision` — `admit`, a single phase label, `retry`, or `refuse`.
- `invalidated_count` on the completion log, plus `batches` when any batch was invalidated.

The orchestrator remains the source of restart-loop policy and alert routing. Exit projection consumes the controller's already-classified `Retry`/`Refuse` result instead of reclassifying raw recovery errors.

### L1 view freshness

The safety policy does not branch directly on a provider-reachability boolean. Reachability is an execution concern: the initial Sync may fail while a warm persisted view remains usable, whereas a post-flush Sync failure must retry because Cascade requires a newly caught-up view. The decision primitive is the freshness of the L1 view recorded in SQLite plus the post-flush witness floor when one exists.

The most common real-world trigger for `L1ViewStale` is a stalled RPC gateway: the provider answers, but its safe-head response stops advancing (a degraded upstream node, a load-balancer routing to a lagging replica, or a temporary indexing pause). The sequencer can't distinguish "fresh answer from a stalled view" from "L1 itself is unhealthy" without a second source of truth, so it treats both the same way: refuse to commit to soft confirmations until the recorded safe block is fresh again.

**At startup**: the sequencer first inspects local terminal facts, then attempts the initial safe-head Sync, then inspects the persisted safe-block and progress timestamps. If the L1 timestamp is missing or older than `l1_read_stale_after_blocks * seconds_per_block`, `check_danger` returns `L1ViewStale` and startup retries. A baseline a full block-time or more ahead of `now` also yields `L1ViewStale`, but only after observed-safe checks have run. If those checks selected a repair, the repair completes and its mandatory next inspection applies the clock refusal. If the view is usable and fresh, observed-safe checks can route to recovery, and the batch-relative wall-clock estimate remains the final retry guard.

**At runtime**: the `DangerDetector` polls `Storage::check_danger` on its cadence. The input reader records both the observed safe block timestamp and the local time at which the safe head last advanced. If safe-head observations stop advancing, either the global safe block timestamp crosses the read-staleness threshold (`L1ViewStale`) or a specific unresolved batch crosses the batch-relative adjusted threshold (`EstimatedBatchInDanger`). A backward clock step of a full block-time or more against either persisted baseline also produces `L1ViewStale` — evaluated after the observed arms — and saturation must never reinterpret such a regression as zero elapsed time; sub-block steps are quantization noise for the block-granular estimate and are tolerated. The detector then exits with `RecoveryRequired`, the orchestrator respawns, and startup re-runs the same check. The batch submitter never observes danger; this responsibility lives entirely with the detector.

**Other workers during L1 outages**: the inclusion lane and API are purely local (SQLite) and continue operating. The input reader retries L1 polling with error logging. All L1-dependent workers log errors at the `error` level to alert operators.

The `seconds_per_block` parameter (default: 12 for Ethereum) is configurable via `CARTESI_SEQUENCER_SECONDS_PER_BLOCK`. The L1 read-staleness threshold is configurable via `CARTESI_SEQUENCER_L1_READ_STALE_AFTER_BLOCKS`; if unset, startup derives it before the write danger threshold. These estimates are conservative — they may cause earlier detection if blocks are slower than assumed. This is correct: better to crash early than to issue doomed soft confirmations.

## Dead Batches

After cascade invalidation, submitted Pending batches (those with `w_nonce` assigned) are **dead batches**. They are still in the L1 mempool, competing with their flush no-op transactions.

Two outcomes per dead batch, non-deterministic:

- **Dead batch beats no-op**: lands on L1, scheduler sees it, rejects it (stale by inclusion, or nonce-poisoned by a preceding stale/missing batch)
- **No-op beats dead batch**: dead batch killed forever, scheduler never sees it (the scheduler skips the gap)

A killed batch acts as **silent nonce poison**: the scheduler never sees it, so `schedulerExpected` stays stuck at its `batch_nonce`. All subsequent batches have wrong nonces.

Dead batches occupy `w_nonce` slots strictly below `walletNonce`. Recovery batches occupy `w_nonce` slots at or above `walletNonce`. **No overlap.** This is why no mutual exclusion is needed between dead batches and recovery batches -- they live in non-overlapping `w_nonce` ranges.

## Cockroach recovery (`setup --recovery`)

Everything above is **standard recovery**: the sequencer's own bookkeeping
(the batch tree, pending dumps) lets startup cascade a doomed suffix and
resume. The repair decision is automatic, not an operator-designed reconstruction:
recovery crosses a process boundary, and the next boot inspects fresh facts
through the reducer regardless of how the prior process died: an unclean
exit leaves no gate behind, and there is no operator acknowledgement step.
A terminal death best-effort records its cause in the `terminal_faults`
black box, which nothing reads for decisions.

**Cockroach recovery** is the catastrophe path — the local DB is lost or has diverged (`CanonicalDivergence`, [I15](../invariants.md)). There is no tree to cascade; the operator supplies a fresh or explicitly wiped data directory and rebuilds canonical logical state from a trusted checkpoint plus L1. It is an operator-driven, one-shot `setup` mode, not a runtime action. There is no automated DB replacement, clone detection, distributed fencing, or partial-fill resume state machine. The summary:

Given a trusted checkpoint machine `S` at block `B` (a finalized `dumps/<id>/` dir, carrying `N` = its resume nonce and `A` = its last-executed safe block), `setup --recovery --checkpoint-block B --checkpoint-dump-dir <dir>` runs **flush → fold → fill**:

1. **Flush** the wallet nonce (keyed — recovery, unlike plain `setup`, signs) so every previous-instance batch resolves at safe depth `≤ C`, the post-flush safe head. Re-sync `safe_inputs` through `C`.
2. **Fold** (the pure `sequencer-core` engine, shared with the on-chain scheduler so it is consistent by construction): seed the fridge from the `(A, B]` directs (drop batches — already in `S`), replay the `(B, C]` stream, drain the leftover fridge at `C`. Yields `(S', N')` = the advanced app state and the resume nonce.
3. **Fill** a consistent DB: the baseline transaction has already minted a UUIDv4 `EraId` and initialized `RecoveryGeneration = 0`, while leaving the rebuild's `base_executed_input_count` and `base_safe_input_index` NULL. Derive `K = S'.executed_input_count()`. **Anchor the batch tree at `N'`** ([I16](../invariants.md) — the root tip *is* `N'`, no sentinel batch); sequence the `≤ C` inputs so the replay cursor starts past them (they're already in `S'`, while `run`'s first on-chain batch re-drains them by `safe_block`). Capture that root's exclusive safe-input cursor as the durable drain floor, then bind it with `K` in the same transaction that registers `S'` as the initial finalized snapshot at `C`; setup completion requires both non-NULL bases and the snapshot. Later standard recovery uses `max(base_safe_input_index, max valid attribution + 1)`, so invalidating the root cannot re-sequence those inputs. Physical `l2_tx_index` includes unmapped cursor padding and is deliberately distinct from application-history base `K`; the first executable input above the floor is mapped at `K`. `run` boots from this state.

During recovery the gold frontier (`safe_accepted_batches`) population is **deferred** (`FrontierMode::DeferUntilAnchorSet`): the tree is empty until fill, so simulating acceptance against it would flag every L1 batch as foreign and freeze the frontier ([I15](../invariants.md)). It is populated on `run`'s first sync — once the anchor `N'` is set — so the folded `< N'` history is skipped as trusted collapsed history. `N` is **trusted checkpoint metadata**, not re-verified at recovery time: a wrong-low `N` surfaces at `run` via the content-identity check, but a wrong-high `N` does not — sound because a sequencer-produced finalized dump cannot carry a wrong `N` by construction (see [`cockroach.md`](cockroach.md#data-dictionary) for the full trust boundary). Recovery is a **strict one-shot**: it refuses (terminal) on a DB that is already set up. A retained incomplete DB reuses the still-unexposed era minted by its baseline transaction. Once matching root Tip plus the atomically bound finalized snapshot/`K` exist, that durable fill is authoritative and retry is a no-op; it does not compare stored `K` against a later fold at a newer `C`. A fail-loud partial fill instead requires the operator to wipe and retry, minting another unexposed era. This is not general resume machinery.

The detect-and-refuse gate is the *trigger*: a fresh `setup` that finds a previous instance's batches past the checkpoint refuses with exit `40` (`EXIT_SETUP_NEEDS_RECOVERY`), pointing the operator here.

## Canonical divergence (terminal, outranks every arm)

Independent of the staleness machinery, the input reader's acceptance
simulation cross-checks every at/above-anchor **accepted** landing against the
local valid closed batch at that nonce (the content-identity check:
`keccak256` of the landed wire bytes vs the hash stamped at seal). The complete
outcome set for that predicate is `Match`, `Foreign` (no local batch), or
`Mismatch` (different bytes). `Foreign`/`Mismatch` persist the
`canonical_divergence` marker in the same transaction as `safe_inputs`, the L1
safe head, and accepted-frontier projection, and freeze the acceptance
frontier immediately.

This automatic detection starts only when the landing reaches L1 safe and the
reader successfully ingests it. `check_danger` reports
`CanonicalDivergence` **ahead of every other arm**, so a respawn loop can never
route a diverged node into a provider call, recovery phase, or admission. Every
reducer iteration begins with local inspection; mutating phase transactions
reassert the marker's absence. The controller maps it to terminal `Refuse`.
At runtime, `DangerDetector` owns prompt process-wide reaction on its two-second
cadence. The inclusion lane's existing time-gated SQLite read independently
returns a typed divergence instead of a usable frontier, so a turn that
observes the marker closes intake and terminates before direct execution,
promotion, or the five-block frame-clock decision. This is opportunistic
refusal, not another polling schedule or reaction-time guarantee. A turn that
already read an open frontier may finish if the reader commits the marker
concurrently, and a user-op chunk committed before either runtime observation
may acknowledge; no cross-worker lock or per-chunk marker query is added.

The operator watchdog does not replace this check. The marker freezes finalized
promotion, so the offending landing normally leaves the watchdog's checkpoint
endpoint unchanged and its idle optimization skips replay/comparison. It is a
wire-identity predicate; the watchdog is the broader independent
application-state comparison once a newer finalized checkpoint exists.

The distinction from standard recovery is important. The danger classifier
and reducer exhaustively handle the modeled automatic-recovery states on this
page. The check is complete only for accepted-batch content identity. It is not an
independent oracle for checkpoint/application correctness, trusted collapsed
history below the anchor, the mirrored scheduler predicate, or arbitrary
direct/user execution bugs. In particular, the known wrong-high checkpoint
nonce case is outside its detection boundary (see
[`cockroach.md`](cockroach.md#data-dictionary)). Absence of the marker does not
prove general canonical agreement.

The remedy is **cockroach recovery (wipe + rebuild from L1), never the
standard recovery on this page**: the cascade reconciles the batch tree's
*shape* under the assumption that accepted nonce N is our batch N — a content
mismatch means canonical state contains executed effects with no reliable
local source, so rebuild-from-L1 is the only honest repair.

## Implementation Constraints

These constraints were discovered during TLA+ model checking and are required for correctness:

1. **`walletNonce` must NOT be reset during recovery.** Recovery batches must use `w_nonces` strictly past all dead batch slots. The flush consumes dead batch slots by advancing `nextL1Slot` up to `walletNonce`. Recovery starts fresh from there.
   **Mechanism:** `walletNonce` is realized durably as the `wallet_nonce_watermark` singleton — the highest wallet nonce ever broadcast. Every broadcaster (the batch poster and the flusher's no-ops alike) commits `watermark = max(watermark, n)` power-loss-durably (`synchronous=FULL`) **before** sending at nonce `n` (write-before-broadcast; a crash between commit and send only over-covers — one wasted no-op). The flush's completion condition is `pending <= safe && safe >= watermark + 1`, so it cannot declare victory while any slot we ever used is unresolved — restoring this constraint against the local pool's volatile memory. The watermark is never reset and never lowered.

2. **`SubmitBatch` must use `max(walletNonce, nextL1Slot)`.** Prevents assigning `w_nonce` values for slots L1 has already consumed.

3. **`SubmitBatch` must assign ALL pending batches at once, in spine-position order.** If batches are submitted individually, a flush-win can bump one batch's `w_nonce` past a later batch's, violating the spine ordering invariant.

4. **Wall-clock freshness when the L1 view stops advancing.** The input reader records the L1 safe block timestamp and the local last-safe-head-progress time. `Storage::check_danger` first refuses on an old or missing safe block timestamp; a clock a full block-time or more out of step with either persisted baseline also refuses, but only after the observed-safe checks (sub-block skew is tolerated as quantization noise). Only a usable clock reaches the unresolved-batch estimate (`elapsed / seconds_per_block`). Without these checks, an L1 outage or a large backward clock step can silently push batches past the danger zone while the DB-based safe-block number remains frozen.

5. **The accepted-frontier cache persists acceptances, not scan progress.** `safe_accepted_batches` stores the scheduler-accepted prefix and resumes from the latest accepted safe input. Rejected batch-submitter inputs after that frontier can be rescanned on later safe-head syncs until a later batch is accepted. This is a performance tradeoff, not a correctness bug: recovery batches can reuse a scheduler nonce after earlier rejected rows, so a separate persistent scan cursor would need careful nonce-reuse tests before being introduced.

## Formal Verification

The recovery design is verified with two complementary bounded TLA+ models. [`preemptive.tla`](preemptive.tla) owns slot/batch safety; [`admission.tla`](admission.tla) owns startup reduction and runtime admission. An alternative optimistic batch design is preserved in [`history/optimistic.tla`](history/optimistic.tla).

**Scope and limitations**: these are bounded safety models. They exhaustively check all reachable states within the configured bounds but do not prove liveness or model concrete timing margins. The admission model includes abstract owner loss/crash with fresh-attempt restart (there is no admission state machine to model); the slot model does not model crash/restart and relies on SQLite atomicity for its implementation mapping.

### `preemptive.tla` -- Slot-level safety under adversarial flush

Models the core slot-level mechanics of preemptive recovery. At every `w_nonce` slot, L1 non-deterministically includes the spine batch OR a flush no-op (killing the batch). This covers the case where the frontier batch itself is killed during flush. The model also treats the open Tip's `safe_block` as meaningful, so it can explicitly recover an aging Tip that has no L1 footprint yet.

The model is a **safety over-approximation for the actions it shares with the implementation**: it allows `AdvanceTip` and `SubmitBatch` to interleave freely with recovery, which the real protocol prevents (the sequencer goes offline). This makes the proof stronger -- if `ZombieSafety` holds under more interleavings, it holds under fewer. However, the over-approximation claim does **not** hold action-for-action — two implementation actions sit *outside* the model's transition set: (1) the model discards an aging Tip only at `MAX_WAIT_BLOCKS`, while the implementation invalidates at `danger_threshold` (= `MAX_WAIT − MARGIN`); (2) the model's `Resolve` has no case for a killed-Pending frontier (it relies on resubmission until the frontier is Silver), while guarded post-flush Cascade invalidates killed Pendings unconditionally. Their safety rests on the external arguments above. Sequential startup ordering is intentionally delegated to `admission.tla` rather than cross-producting this already-large slot model.

**Verified**: 157M states, 0 violations.

| Invariant | Meaning |
|-----------|---------|
| ZombieSafety | `schedulerExpected = CountGold(spine)` -- scheduler accepts exactly the Gold prefix |
| BatchNoncesContiguous | Batch nonces are 0..N-1 for non-Tip spine |
| InvalidOnlyOnGold | Dead branches only hang off Gold nodes |
| L1WNonceUnique | No two L1 entries share a `w_nonce` |
| L1BeforeCursor | All L1 entries have `w_nonce < nextL1Slot` |
| SchedulerBehindL1 | Scheduler cursor doesn't pass L1 cursor |
| DeadNotYetIncluded | Dead batches have `w_nonce >= nextL1Slot` |

### `admission.tla` -- Startup reduction and authority

Models local-first inspection, typed Retry/Refuse, InitialSync, EnsureTip,
RecoverTip, Flush/Sync/Cascade with ephemeral witnesses, Sync-discovered
divergence, mandatory reinspection after every completed phase,
crash-and-restart as a fresh attempt over surviving durable facts (nothing
durable gates the next boot; the terminal-fault black box is non-gating
telemetry outside the model — written at settlement, read only for a boot
warning — and there is no acknowledgement action), and atomic minting of the `RuntimeAdmission` witness from the
final clean decision. It abstracts away the batch spine and delegates every phase's
batch mechanics to `preemptive.tla`.

**Verified**: 860 generated states, 266 distinct states, depth 13, 0 violations.

Key invariants include capability soundness, reducer/phase control while an
attempt is begun, completed-phase reinspection, terminal dominance, Cascade
witness preconditions, and Retry never being interpreted as clean. Concrete SQLite
mutation guards and transaction atomicity are tested in Rust rather than
modeled as database actions here.

### Running the spec

```bash
tlc -workers auto -deadlock docs/recovery/admission.tla
tlc -workers auto -deadlock docs/recovery/preemptive.tla    # ~90s
just -f docs/recovery/justfile check-all
```

Bounds are in `admission.cfg` and `preemptive.cfg`. The `MaxWalletNonce` bound keeps the slot model finite (kill/resubmit cycles generate new `w_nonce` values). Increase bounds for higher confidence at the cost of longer runtime.
