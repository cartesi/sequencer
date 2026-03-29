# Recovery Design History

This directory preserves the optimistic recovery design -- an alternative to the preemptive approach documented in the parent [`README.md`](../README.md). Both designs are sound. We preferred preemptive for its operational properties.

## The Optimistic Design

In the optimistic design, the sequencer keeps accepting user operations and building batches while recovery plays out in the background. If a batch goes stale, the system detects it when the batch becomes Silver (safe on L1), cascade-invalidates, and submits recovery batches -- all while the sequencer continues serving soft confirmations.

The TLA+ spec [`optimistic.tla`](optimistic.tla) models this design with a scheduler, wallet nonces, zombie batches (invalidated batches still in the L1 mempool), and adversarial L1 inclusion. At each `w_nonce` slot where a zombie and a recovery batch compete, L1 non-deterministically picks one (wallet-nonce mutual exclusion).

**Verified**: 194M states, 0 violations (after the Silver-only fix below).

## The Silver-Only Constraint

Both designs share a critical constraint: **recovery must wait for the frontier batch to be Silver before cascade-invalidating.**

This constraint was discovered through the optimistic model. The original design allowed staleness detection on Pending or Bronze batches (a "short-circuit" for faster recovery). TLA+ found a counterexample:

Three batches with `MAX_WAIT_BLOCKS = 2`:

```
batch  bn=0  bn=1  bn=2
sb     0     0     1
wn     0     1     2
```

With `currentSafeBlock = 2`, `bn=1` is stale by current block, `bn=2` is fresh. If we cascade from `bn=1`, both become zombies. Recovery creates a new `bn=1` at `wn=1`.

At L1 slot 1, zombie `bn=1` and recovery `bn=1` compete (same `w_nonce`):

- **Zombie wins**: scheduler sees it, stale, skip. Nonce poisoned. Safe.
- **Recovery wins**: zombie `bn=1` dies (never reaches L1). Recovery accepted. `schedulerExpected` advances to 2. Zombie `bn=2(wn=2)` is fresh (`inclusion_block - safe_block = 1 < 2`), matches expected nonce -> **accepted**. The scheduler executes invalidated batch data.

The two protection layers (wallet-nonce mutual exclusion and nonce poisoning) undercut each other: mutual exclusion kills the batch that nonce poisoning needs.

The fix: only detect staleness when the frontier is Silver (safe on L1, immutable). The scheduler is guaranteed to see it before any recovery batch.

## Why We Chose Preemptive

Both designs are sound once Silver-only detection is enforced. The difference is operational:

**Both designs wait.** Any recovery design must wait for the frontier to become Silver before cascading. In the optimistic design, the sequencer keeps issuing soft confirmations during this wait -- confirmations that will be invalidated when the cascade fires. In the preemptive design, the sequencer goes offline before the cascade, so no doomed soft confirmations are issued.

**Preemptive is simpler to reason about.** The optimistic design has concurrent actors: the batch submitter, the inclusion lane, L1 mempool competition, and recovery all interleave. The preemptive design is sequential: stop, flush, recover, resume. Each step has clear preconditions and postconditions.

**Preemptive eliminates mempool races.** The flush resolves all `w_nonce` slot uncertainty before recovery runs. Recovery operates on fully-finalized L1 state. No zombie mutual exclusion needed.

**The cost is downtime.** Preemptive recovery takes the sequencer offline for the duration of the flush + safe finality wait (~15-20 minutes on Ethereum). For a rare event (a batch approaching the 4-hour staleness deadline), this is acceptable.

## Running the Spec

```bash
tlc -workers auto -deadlock docs/recovery/history/optimistic.tla    # ~3min
```

Bounds are in `optimistic.cfg`.
