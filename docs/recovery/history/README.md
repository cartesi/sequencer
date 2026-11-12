# Recovery Design History

This directory preserves recovery designs the sequencer does not implement:
the **optimistic recovery design** and its counterexample, the **preemptive
recovery** with its two models, and two superseded cockroach-recovery plans.
The sequencer's one recovery is [cockroach recovery](../cockroach.md);
[batch landing and outages](../README.md) owns what happens when batches stop
landing and [why there is no automatic repair](../README.md#why-there-is-no-automatic-repair).

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
after the Silver-only fix. This is model evidence, not a proof of that
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

## The preemptive recovery and its models

The preemptive design repaired the history in place. A detector stopped the
process at a threshold before staleness; the next start flushed every wallet
slot the closed unaccepted batches had used, so that the original or a no-op
landed at safe depth in each, then re-synced the accepted prefix, invalidated
the suffix after it, and opened a fresh batch at the first unaccepted nonce.
It did not require the original frontier batch to be on safe L1, and it closed
intake while it ran. The sequencer does not implement it.

Two bounded TLA+ models checked it. They were not a refinement proof of the
Rust code, a proof of their composition, or a liveness guarantee.

- [`preemptive.tla`](preemptive.tla) ([bounds](preemptive.cfg)) models
  safe-block advancement, wallet-slot competition between batches and no-ops,
  scheduler acceptance and branch invalidation. Its `Inv` checks
  `ZombieSafety` (`schedulerExpected = CountGold(spine)`) with batch-nonce
  contiguity, invalid-branch ancestry, wallet-slot uniqueness and cursor
  bounds.
- [`admission.tla`](admission.tla) ([bounds](admission.cfg)) models the
  startup controller: the local terminal gate, the initial-sync fallback, at
  most one repair, the flush observation and mandatory sync, the guarded
  cascade, and admission before any worker exists. Its invariants cover
  terminal dominance, repair preconditions, a caught-up post-flush view, and
  admission soundness.

Both models exist because replacing a batch reuses its batch nonce, so a late
transaction of the replaced batch can race the replacement. Inside an era no
batch nonce is reused: a late transaction of the sequencer's carries the bytes
of a batch it still holds, and it is accepted as that batch or skipped. The
race these models guard against cannot occur there.

## Running the models

```bash
just -f docs/recovery/history/justfile check-all
```
