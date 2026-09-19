# Recovery Design History

This directory preserves the **historical optimistic recovery design** and its
counterexample. It is not the production recovery procedure. The
[current recovery guide](../README.md) owns automatic recovery; the
[cockroach guide](../cockroach.md) owns manual rebuilding.

## The optimistic alternative

The sequencer would keep accepting user operations and building batches while
recovery ran concurrently. The retained [model](optimistic.tla) permits a
cascade only when the first unresolved batch is **Silver** (included in a safe
L1 block) and stale by its **inclusion block**. Recovery replaces the suffix and
resets the next wallet nonce to the next unconsumed L1 slot. Submitted batches
from the invalidated suffix may remain in the network as zombies competing with
new recovery batches.

The recorded bounded check reported 194M states with no invariant violations
after the Silver-only fix. This is model evidence, not a proof of production
recovery or its completion time. Bounds are in [`optimistic.cfg`](optimistic.cfg).

## The counterexample: invalidating before slot resolution

The rejected variant allowed an unresolved frontier to be invalidated based on
its current age, before its L1 outcome was settled. The danger was **cascading
and reusing wallet-nonce slots**, not detecting danger early.

Take `MAX_WAIT_BLOCKS = 2` and three original batches:

```text
batch nonce    0   1   2
safe_block     0   0   1
wallet nonce   0   1   2
```

Assume batch 0 is already accepted. At `currentSafeBlock = 2`, batch 1 is old
enough to be stale if included now, while batch 2 is still fresh. If recovery
invalidates batches 1 and 2 while they are pending, it can submit a fresh
replacement batch 1 at wallet nonce 1.

At L1 slot 1, the original and replacement compete:

- **Original wins:** the scheduler sees the stale batch and leaves its expected
  batch nonce at 1. The original batch 2 then fails the nonce check.
- **Replacement wins:** the original batch 1 cannot land. The fresh replacement
  advances the scheduler's expected nonce to 2. If the original batch 2 lands
  in block 2, its age is `2 - 1 < 2` and its nonce matches: the scheduler accepts
  data the sequencer already invalidated.

Wallet-nonce mutual exclusion removed the stale batch that the nonce-poisoning
argument depended on. The retained optimistic model's `Resolve` therefore
requires a Silver frontier that is stale by inclusion: that original batch is
already on safe L1 and cannot be displaced by a replacement.

## Why production uses preemptive recovery

Production closes intake and performs recovery offline. For closed-batch
recovery, the flush consumes every covered wallet-nonce slot at safe depth,
**whether the original transaction or a no-op wins**, then re-syncs the accepted
prefix before cascading. It does not require the original frontier batch to
become Silver. An unsubmitted open Tip has no wallet slot to settle.

This gives recovery a sequential procedure and stops new soft confirmations
while submission uncertainty is being resolved. The optimistic alternative
keeps serving through that interval and may add confirmations that a later
cascade revokes.

The tradeoff is downtime. Flush completion requires L1 progress; fee headroom
does not guarantee replacement or establish a deadline. The
[current recovery guide](../README.md) owns the safety conditions, and the
[L1 fee policy](../../l1-fee-policy.md) owns the accepted liveness limits.

## Running the historical model

```bash
tlc -workers auto -deadlock docs/recovery/history/optimistic.tla
```
