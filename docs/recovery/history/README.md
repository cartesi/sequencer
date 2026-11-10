# Recovery Design History

This directory preserves recovery designs that production does not use: the
**historical optimistic recovery design** and its counterexample, and two
superseded cockroach-recovery plans. None is a production procedure. The
[current recovery guide](../README.md) owns automatic recovery; the
[cockroach guide](../cockroach.md) owns manual rebuilding, which now
describes the implemented [sequencer recovery](../../plans/2026-11-sequencer-recovery/README.md).

## Superseded cockroach-recovery plans

Archived on 2026-11-05. Each opens with the commit it was checked against,
and its links point at the code and docs of that commit: some name headings
and files that later changes removed, so read them at that commit.

| Plan | What it proposed | Why it was superseded |
|---|---|---|
| [Shared recovery tool](2026-10-shared-recovery-tool.md) | A tool that rebuilds the scheduler's history from L1, starting from three values a machine reports, shared by the sequencer and its clients | Reading the whole scheduler state from the machine needs no replay from L1 and no shared tool |
| [Recovery from a canonical machine](2026-11-recovery-from-a-machine.md) | The rebuild folded into setup, from four values a machine reports | The owner's review led to exporting the whole scheduler state, fridge included |

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
