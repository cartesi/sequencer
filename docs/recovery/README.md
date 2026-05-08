# Batch Recovery

This document describes the recovery design for the sequencer: how the system detects that batches are failing to land on L1, and how it recovers to a consistent state. The design is verified with bounded TLA+ model checking ([`preemptive.tla`](preemptive.tla)).

See `AGENTS.md` "Batch Staleness and Recovery" for quick-reference tables and function names.

## Runtime lifecycle at a glance

The sequencer's recovery loop spans two process lifetimes:

1. **In-process detection.** The `DangerDetector` polls `Storage::check_danger` on a cadence. When any non-`Safe` status fires (`L1ViewStale`, `ClosedBatchInDanger`, `TipInDanger`, or `EstimatedBatchInDanger`), the runtime converts that into `RunError::DangerDetected` and the process exits with non-zero status.
2. **External respawn.** An orchestrator (systemd, k8s, …) restarts the process.
3. **Startup dispatch.** The fresh boot runs `run_preemptive_recovery` before any writers come online: sync L1, re-run `check_danger`, then `decide_startup_action` routes to one of `Proceed`, `RecoverTip`, `FlushAndCascade`, or `Refuse`. The chosen action runs as a single SQLite transaction.

The detector trip and the startup dispatch share the same `check_danger` function; the detector cares only that *some* arm fired, while the startup dispatch examines *which* arm fired to pick the right action.

Key abstractions, by responsibility:

- **`DangerDetector`** ([`recovery/detector.rs`](../../sequencer/src/recovery/detector.rs)): tiny background task that calls `Storage::check_danger` on a cadence. Never writes to the DB, never talks to L1. Exits with `DetectorExit::RecoveryRequired` when any non-`Safe` status fires. The runtime converts that into `RunError::DangerDetected` and the process exits. The dispatch difference between statuses only matters at the next startup, where `decide_startup_action` re-runs `check_danger` and routes accordingly.
- **`BatchSubmitter`** ([`l1/submitter/worker.rs`](../../sequencer/src/l1/submitter/worker.rs)): makes L1 progress only — never checks danger. Productive ticks re-enter immediately; idle/transient ticks sleep `idle_poll_interval`. A pure `decide_submit_start` function folds observed L1 nonces over the scheduler-accepted frontier.
- **`decide_startup_action`** ([`recovery/mod.rs`](../../sequencer/src/recovery/mod.rs)): pure function. Takes `danger` and returns `Proceed | RecoverTip { batch_index } | FlushAndCascade { batch_index } | Refuse(reason)`. L1 reachability is an execution concern: if the flush path cannot reach L1, startup fails and the orchestrator retries.
- **`MempoolFlusher`** ([`recovery/flusher.rs`](../../sequencer/src/recovery/flusher.rs)): submits no-op transactions to consume all pending wallet-nonce slots and waits for safe finality. Does **not** retry internally on provider errors — the orchestrator's respawn loop is the retry mechanism.
- **`ProtocolConfig`** ([`sequencer-core/src/protocol.rs`](../../sequencer-core/src/protocol.rs)): single source of truth for the scheduler-mirroring fields (`batch_submitter`, `max_wait_blocks`) plus the sequencer-local tuning knobs (`preemptive_margin_blocks`, `l1_read_stale_after_blocks`, `seconds_per_block`). Exposes `scheduler_accepts`, `is_scheduler_stale`, `danger_threshold`, and `l1_view_is_stale`.

All five pieces are replaceable at the abstraction boundary: the tick decision is a pure function; the storage surface returns structs, not ad-hoc tuples; the danger detector and submitter are independently testable.

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

The implementation can handle the nonce-0 case either by submitting a sentinel batch at first startup, or by special-casing the recovery code for the "no Gold ancestor" branch.

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

Query the latest confirmed `w_nonce` (N) and the pending `w_nonce` (M). Submit `M - N` no-op transactions (e.g., self-transfer of 0 ETH) at nonces N, N+1, ..., M-1. These compete with any batches in the mempool at the same slots.

Wait for all `M - N` slots to reach L1 safe finality.

### Step 4: Post-flush state

Every `w_nonce` slot from N to M-1 is now resolved:

- **Batch won**: the batch is on L1 and safe (Silver or Gold)
- **No-op won**: the batch is dead forever, its slot consumed

There are no more mempool entries. All uncertainty is resolved.

**Flush safety does not depend on eviction.** A no-op may fail to evict a still-pending batch tx (e.g. our local node rejects the replacement under EIP-1559's ≥10% bump rule). That's fine: the outer `flush_and_wait` loop is unbounded — it keeps running until `pending ≤ safe`, and *eventual* inclusion of either the original batch tx or the no-op resolves the slot. Safety holds regardless of which lands; eviction is only an operational efficiency concern.

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

Cascading from the first non-gold catches all three. **No per-batch age check is needed for the cascade pivot itself** — every closed batch past gold is doomed by construction.

#### Path A — `recover_post_flush(danger_threshold)` (called from FlushAndCascade)

After step 3 (flush) and step 4 (re-sync), the gold frontier is fresh. Run the atomic recovery transaction:

1. **Find the cascade pivot.** First try the closed pivot: first valid closed batch with `nonce >= frontier_nonce`. By the contiguity invariant, this batch's nonce is exactly `frontier_nonce`. If one exists, cascade from it.
2. **No closed pivot? Check the Tip.** When all closed batches landed fresh and were accepted (the "everything worked" aftermath), there's no closed pivot — but the Tip can still be in the danger zone. When the lane rotates without a safe-block advance between frames (e.g. immediately after init, both frames share the bootstrap `safe_block`), `S_tip = S_closed`. The closed batch can become gold by inclusion-staleness while the Tip's age — measured against `current_safe_block` after the flush wait — has crossed the danger zone. Pure monotonicity (`S_tip ≥ S_closed`) doesn't rule this out: equality is allowed. So fall through to `find_tip_batch_in_danger(danger_threshold)`. If the Tip's age clears `danger_threshold`, cascade it.
3. **Cascade-invalidate the suffix**: set `invalidated_at_ms` on every valid batch with `batch_index >= pivot.batch_index`. This catches all non-gold batches in cases (2)/(3) above, and the Tip alone in the no-pivot-but-Tip-aging case.
4. **Open recovery batch**: parent is the last valid ancestor (`MAX(batch_index) FROM valid_batches` after the cascade). Nonce is structurally `parent.nonce + 1`, which equals `frontier_nonce` — the scheduler's `expected_nonce`. Re-drain direct inputs from the invalidated batches via the `MAX(safe_input_index) + 1` query over `valid_sequenced_l2_txs`.

**Threshold = `danger_threshold`, not `MAX_WAIT_BLOCKS`**. We're already committed to recovery; the Tip is past gold; if it's also past the threshold that would have triggered recovery had it been a closed batch, cascade it. Otherwise the next danger detector tick after resume would re-trip on the Tip's eventual close + submission anyway (the closed batch would inherit its first frame's safe_block).

#### Path B — `recover_aging_tip(danger_threshold)` (called from RecoverTip)

The `RecoverTip` action is dispatched when `check_danger` returns `TipInDanger(idx)`: no closed batch is past the gold frontier in the danger zone, but the open Tip's first frame has aged past `danger_threshold`. **No flush ran** — the Tip has no L1 footprint, so there's nothing to flush.

Closed batches past gold (if any) are still in their natural lifecycle — pending in the mempool, recently included, awaiting safe finality. Cascading them would prematurely abort their progression. We act only on the Tip:

1. Run `find_tip_batch_in_danger(danger_threshold)`. If `Some(tip_index)`, cascade-invalidate from there (which only touches the Tip — no closed batches have `batch_index >= tip_index`).
2. Open a fresh recovery batch.
3. If no Tip in danger and no Tip exists at all (torn-state crash recovery), open a Tip anyway.

The same function is called defensively from the `Proceed` path. Under that dispatch the Tip shouldn't be in danger (the danger check would have surfaced `RecoverTip`), but the threshold check inside `recover_aging_tip` is cheap and guards against a stale `check_danger` reading.

#### Why `danger_threshold`, not `MAX_WAIT_BLOCKS`, for the Tip threshold

The Tip threshold is a **policy choice**, not a mathematical staleness bound. A Tip whose first frame is at age `danger_threshold` could in principle still close, submit, and land fresh by inclusion-staleness — `inclusion_block - first_frame` would be roughly `danger_threshold + (rotation + submit latency)`, which (with a reasonable margin) is still below `MAX_WAIT_BLOCKS`.

We invalidate at `danger_threshold` because:

1. **Pre-confirmation honesty.** Once the Tip's age crosses the danger zone, the system has decided this generation is operationally suspect. Continuing to issue soft confirmations against it is dishonest to users.
2. **Avoid retrip risk.** The runtime danger detector also fires on `DangerStatus::TipInDanger`. Without invalidating at startup, we'd resume operation, the detector would re-trip on the next tick, and we'd cycle. Cascading at startup converges in one cycle.
3. **Symmetry with the closed-batch trigger.** The closed-batch detector trips at `danger_threshold`. Using the same threshold for the Tip preserves the framing: "danger zone = committed to recovery."

### Step 6: Resume

Restart the batch submitter and user-op acceptance. The sequencer is back online.

### Why post-flush cascade is unconditional (and not threshold-based)

An earlier design considered using `MAX_WAIT_BLOCKS` as the cascade threshold even in the post-flush path: only invalidate the frontier if its `current_safe_block - first_frame.safe_block ≥ MAX_WAIT`. The intuition was to preserve soft confirmations when re-submission could still land fresh.

**This doesn't hold up.** Walk through the boundary case:

1. Frontier batch has `current_staleness ∈ [danger_threshold, MAX_WAIT)`. Detector trips, flush runs.
2. `recover_post_flush` (with hypothetical threshold) sees age below MAX_WAIT, declines to cascade. Resume.
3. Submitter wakes up, resubmits the Pending frontier (and any non-gold closed batches) at fresh wallet-nonce slots. They enter the mempool.
4. Detector polls again. Frontier age has barely moved (or not at all — safe head advances at ~1 block per 12s); still above `danger_threshold`. Detector trips again.
5. Recovery 2 starts. Flush submits no-ops at the slots the submitter just used for resubs. Bumped fees on no-ops typically out-bid resubs. Resubs killed.
6. Goto step 2. Loop converges only when `current_staleness` finally crosses `MAX_WAIT_BLOCKS` and the threshold check fires.

Each loop iteration burns gas (no-ops + doomed resubs), takes ~12 minutes (the flush's safe-finality wait), and the soft confirmations are rolled back at the end anyway. Cascading on first non-gold converges in **one cycle** with predictable cost.

### Startup behavior summary

The startup flow is dispatched by `decide_startup_action(danger)`:

| `check_danger` result | Action | Recovery primitive | Why this dispatch |
|---|---|---|---|
| `Safe` | `Proceed` | `recover_aging_tip(danger_threshold)` (defensive — typically a no-op) | Nothing crossed danger; observed Tip check is a cheap belt-and-suspenders. |
| `L1ViewStale` | `Refuse(L1ViewStale)` | — | The L1 view is too old to support honest recovery or new soft confirmations. |
| `TipInDanger(N)` | `RecoverTip { N }` | `recover_aging_tip(danger_threshold)` (no flush — Tip has no L1 slot) | Tip has no L1 footprint; cascade and reopen directly. |
| `ClosedBatchInDanger(N)` | `FlushAndCascade { N }` | flush + `recover_post_flush(danger_threshold)` | Closed batch has L1 transactions whose fate must be resolved before cascading. |
| `EstimatedBatchInDanger(N)` | `Refuse(EstimatedBatchInDanger { N })` | — | Observed safe-state did not cross danger; only batch-relative wall-clock extrapolation did, and we don't recover from estimated state. |

**L1 view freshness gates recovery.** `check_danger` first checks the L1 safe block timestamp against `l1_read_stale_after_blocks`. If the timestamp is missing or too old, startup refuses: the sequencer has no trustworthy L1 view from which to recover or issue new soft confirmations. With a fresh L1 view, observed-safe checks decide concrete recovery: `ClosedBatchInDanger` runs flush + cascade, while `TipInDanger` invalidates the open Tip directly. `EstimatedBatchInDanger` is the final batch-relative wall-clock fallback: observed safe-state has not crossed the threshold, but elapsed time since the last safe-head advance says the batch consumed its remaining runway, so startup refuses instead of recovering from estimated state.

The Refuse variants block boot and surface to the operator. Every non-Refuse action ends with an atomic SQLite transaction: `Proceed` normally just ensures the Tip exists, `RecoverTip` invalidates the aging Tip and opens a fresh one, and `FlushAndCascade` cascades the post-flush non-gold suffix and opens a fresh Tip when needed.

**What TLA+ proves here**: the model still abstracts away the full startup cutover/flush decision. It proves ZombieSafety once wallet-nonce slots resolve, and separately models direct recovery of an aging open Tip. The claim that past `MAX_WAIT`, closed-batch staleness self-resolves is external reasoning from L1 monotonicity. The post-flush "cascade everything past gold" choice is also external reasoning (the "everything past gold is doomed" mental model above).

### Startup observability

Startup recovery logs the decision and outcome with stable structured fields:

- `danger_status` — `safe`, `l1_view_stale`, `closed_batch_in_danger`, `tip_in_danger`, or `estimated_batch_in_danger`.
- `danger_batch_index` — set for batch-specific danger statuses.
- `startup_action` — `proceed`, `recover_tip`, `flush_and_cascade`, or `refuse`.
- `refuse_reason` — present on refusal.
- `l1_reachable`, `danger_threshold`, `max_wait_blocks`, and `l1_read_stale_after_blocks` on the decision log.
- `invalidated_count` on the completion log, plus `batches` when any batch was invalidated.

The orchestrator should still be the source of restart-loop policy and alert routing, but it should not need to parse free-form messages to distinguish "refused because the L1 view is stale" from "recovering a Tip" or "running flush + cascade."

### L1 view freshness

The safety policy does not branch directly on a provider-reachability boolean. Reachability is an execution concern: startup tries to sync the safe head, and if `FlushAndCascade` later cannot reach L1, the flusher errors and the orchestrator retries. The decision primitive is the freshness of the L1 view recorded in SQLite.

The most common real-world trigger for `L1ViewStale` is a stalled RPC gateway: the provider answers, but its safe-head response stops advancing (a degraded upstream node, a load-balancer routing to a lagging replica, or a temporary indexing pause). The sequencer can't distinguish "fresh answer from a stalled view" from "L1 itself is unhealthy" without a second source of truth, so it treats both the same way: refuse to commit to soft confirmations until the recorded safe block is fresh again.

**At startup**: the sequencer attempts to sync the safe head from L1. Whether that succeeds or fails, it then checks the persisted safe block timestamp. If the timestamp is missing or older than `l1_read_stale_after_blocks * seconds_per_block`, `check_danger` returns `L1ViewStale` and startup refuses. If the view is fresh, observed-safe checks can route to recovery, and the batch-relative wall-clock estimate remains as a final refusal guard for unresolved batches whose safe-block age has effectively crossed the danger threshold.

**At runtime**: the `DangerDetector` polls `Storage::check_danger` on its cadence. The input reader records both the observed safe block timestamp and the local time at which the safe head last advanced. If safe-head observations stop advancing, either the global safe block timestamp crosses the read-staleness threshold (`L1ViewStale`) or a specific unresolved batch crosses the batch-relative adjusted threshold (`EstimatedBatchInDanger`). The detector then exits with `RecoveryRequired`, the orchestrator respawns, and startup re-runs the same check. The batch submitter never observes danger; this responsibility lives entirely with the detector.

**Other workers during L1 outages**: the inclusion lane and API are purely local (SQLite) and continue operating. The input reader retries L1 polling with error logging. All L1-dependent workers log errors at the `error` level to alert operators.

The `seconds_per_block` parameter (default: 12 for Ethereum) is configurable via `SEQ_SECONDS_PER_BLOCK`. The L1 read-staleness threshold is configurable via `SEQ_L1_READ_STALE_AFTER_BLOCKS`; if unset, startup derives it before the write danger threshold. These estimates are conservative — they may cause earlier detection if blocks are slower than assumed. This is correct: better to crash early than to issue doomed soft confirmations.

## Dead Batches

After cascade invalidation, submitted Pending batches (those with `w_nonce` assigned) are **dead batches**. They are still in the L1 mempool, competing with their flush no-op transactions.

Two outcomes per dead batch, non-deterministic:

- **Dead batch beats no-op**: lands on L1, scheduler sees it, rejects it (stale by inclusion, or nonce-poisoned by a preceding stale/missing batch)
- **No-op beats dead batch**: dead batch killed forever, scheduler never sees it (the scheduler skips the gap)

A killed batch acts as **silent nonce poison**: the scheduler never sees it, so `schedulerExpected` stays stuck at its `batch_nonce`. All subsequent batches have wrong nonces.

Dead batches occupy `w_nonce` slots strictly below `walletNonce`. Recovery batches occupy `w_nonce` slots at or above `walletNonce`. **No overlap.** This is why no mutual exclusion is needed between dead batches and recovery batches -- they live in non-overlapping `w_nonce` ranges.

## Implementation Constraints

These constraints were discovered during TLA+ model checking and are required for correctness:

1. **`walletNonce` must NOT be reset during recovery.** Recovery batches must use `w_nonces` strictly past all dead batch slots. The flush consumes dead batch slots by advancing `nextL1Slot` up to `walletNonce`. Recovery starts fresh from there.

2. **`SubmitBatch` must use `max(walletNonce, nextL1Slot)`.** Prevents assigning `w_nonce` values for slots L1 has already consumed.

3. **`SubmitBatch` must assign ALL pending batches at once, in spine-position order.** If batches are submitted individually, a flush-win can bump one batch's `w_nonce` past a later batch's, violating the spine ordering invariant.

4. **Wall-clock freshness when the L1 view stops advancing.** The input reader records the L1 safe block timestamp and the local last-safe-head-progress time. `Storage::check_danger` first refuses on an old or missing safe block timestamp, then uses the local progress timestamp to estimate unresolved-batch age (`elapsed / seconds_per_block`). Without these checks, an L1 outage can silently push batches past the danger zone while the DB-based safe-block number remains frozen.

5. **The accepted-frontier cache persists acceptances, not scan progress.** `safe_accepted_batches` stores the scheduler-accepted prefix and resumes from the latest accepted safe input. Rejected batch-submitter inputs after that frontier can be rescanned on later safe-head syncs until a later batch is accepted. This is a performance tradeoff, not a correctness bug: recovery batches can reuse a scheduler nonce after earlier rejected rows, so a separate persistent scan cursor would need careful nonce-reuse tests before being introduced.

## Formal Verification

The recovery design is verified with bounded TLA+ model checking. The canonical spec is [`preemptive.tla`](preemptive.tla). An alternative optimistic design is preserved in [`history/optimistic.tla`](history/optimistic.tla).

**Scope and limitations**: these are bounded safety models. They exhaustively check all reachable states within the configured bounds, but do not prove liveness (eventual progress), do not model the danger threshold trigger or timing margins, and do not model crash/restart (the implementation relies on SQLite atomic transactions for crash safety).

### `preemptive.tla` -- Slot-level safety under adversarial flush

Models the core slot-level mechanics of preemptive recovery. At every `w_nonce` slot, L1 non-deterministically includes the spine batch OR a flush no-op (killing the batch). This covers the case where the frontier batch itself is killed during flush. The model also treats the open Tip's `safe_block` as meaningful, so it can explicitly recover an aging Tip that has no L1 footprint yet.

The model is a **safety over-approximation**: it allows `AdvanceTip` and `SubmitBatch` to interleave freely with recovery, which the real protocol prevents (the sequencer goes offline). This makes the proof stronger -- if `ZombieSafety` holds under more interleavings, it holds under fewer. However, the model does not verify the full sequential protocol phases (cutover, flush, wait, recover, resume) described above; in particular, the startup decision of whether a closed unresolved batch must flush before recovery remains an external argument layered on top of the slot-level proof.

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

### Running the spec

```bash
tlc -workers auto -deadlock docs/recovery/preemptive.tla    # ~90s
```

Bounds are in `preemptive.cfg`. The `MaxWalletNonce` bound keeps the state space finite (kill/resubmit cycles generate new `w_nonce` values). Increase bounds for higher confidence at the cost of longer runtime.
