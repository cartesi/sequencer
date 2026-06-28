# Scheduler acceptance semantics

The canonical ordering rule. The on-chain scheduler reads a stream of L1 safe
inputs and folds them into application state in one deterministic order. Every
other component that *predicts* that order — the gold frontier, the inclusion
lane, the recovery fold — must reproduce this algorithm exactly. Disagreement is
silent scheduler/sequencer divergence, the most severe failure in the system
([I1](../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)).

This document **owns** that algorithm. [`AGENTS.md`](../../AGENTS.md) §"Sequencer /
Scheduler Duality" is the map; this is the detail. The reference implementation
is [`Scheduler<A>`](../../sequencer-core/src/scheduler/mod.rs) — the same source
compiled into the on-chain canonical machine and run bare-metal by the recovery
fold, so the two targets agree *by construction*.

---

## The one stream, the one classification

The scheduler consumes `InputAdded` events from the InputBox in L1 order. Each
event carries an authenticated `msg_sender`. Classification is **by sender
address, never by a tag byte** ([`process_input`](../../sequencer-core/src/scheduler/mod.rs)):

| Sender | Treated as | Effect |
|---|---|---|
| `== sequencer_address` | a **batch** (SSZ-encoded `Batch`) | folded through the acceptance algorithm below |
| anything else | a **direct input** (deposit) | appended to the *fridge* (the direct-input FIFO), drained later |

The payload is opaque to classification; app-specific decoding happens inside
`Application::execute_direct_input` (see the
[application contract](application-contract.md)).

---

## The algorithm (`process_input`)

Every input — batch or direct — first runs the **censorship backstop**, then is
classified.

```
process_input(input):
  1. force_execute_overdue(input.inclusion_block)   ── backstop, runs for EVERY input
  2. if input.sender != sequencer_address:
         fridge.push_back(input)                    ── DirectEnqueued
     else:
         process_batch_payload(input)               ── the acceptance algorithm
```

### Step 1 — Force-execute overdue directs (the backstop)

Before *anything* else, drain every fridge direct that has gone **overdue**:
`current_block − direct.inclusion_block ≥ MAX_WAIT_BLOCKS`
([`force_execute_overdue`](../../sequencer-core/src/scheduler/mod.rs)). This is
the censorship-resistance guarantee: a direct cannot be held in the fridge
forever by a sequencer that freezes `safe_block` or stops submitting batches —
once it ages out, it executes regardless of any frame. It runs on every input
tick (even a malformed batch advances `current_block` and can trip the
backstop). Drained directs execute in FIFO (ascending `inclusion_block`) order.

### Step 2 — Batch acceptance (`process_batch_payload`)

A batch from the sequencer address runs this gauntlet, in order. The first
failing gate decides the outcome; only `BatchExecuted` (including the empty-batch
case) **consumes the nonce**.

| # | Gate | On failure | Nonce? |
|---|---|---|---|
| a | SSZ-decode the payload as `Batch` | `BatchRejected(DecodeFailed)` | not consumed |
| b | `batch.nonce == next_expected_batch_nonce` | `BatchRejected(WrongNonce)` | not consumed |
| c | *empty frames?* → accept as a no-op | `BatchExecuted` | **consumed** |
| d | structural frame check (below) | `BatchRejected(SafeBlockAboveInclusionBlock \| NonMonotonicSafeBlocks)` | not consumed |
| e | staleness: `inclusion_block − frames[0].safe_block ≥ MAX_WAIT_BLOCKS` | `BatchSkippedStale` | **not consumed** |
| f | execute every frame (below) | `BatchExecuted` | **consumed** |

**Gate d — structural frame check**
([`batch_reject_reason_for_block`](../../sequencer-core/src/scheduler/mod.rs)):

- every frame's `safe_block ≤ inclusion_block` (a frame cannot claim to have
  accounted for L1 state it was included before) — else
  `SafeBlockAboveInclusionBlock`;
- frame `safe_block`s are **non-decreasing** across the batch — else
  `NonMonotonicSafeBlocks`.

**Gate e — staleness** is measured against the **first** frame only
(`has_elapsed_since`, mirrored off-chain by
[`ProtocolTiming::is_scheduler_stale`](../../sequencer-core/src/protocol.rs)). A
stale batch is a **true no-op in nonce space**: the expected nonce does *not*
advance, so the next batch — including a freshly-rebuilt one reusing the same
nonce after recovery — is what the scheduler accepts next. This is the
mechanism behind cascading invalidation (AGENTS.md §"Cascading invalidation").

### Step 2f — Frame execution order (drain-before-ops)

For each frame, in order ([`process_batch_payload`](../../sequencer-core/src/scheduler/mod.rs)):

1. **Drain covered directs first**: pop every fridge direct with
   `inclusion_block ≤ frame.safe_block` and execute it
   ([`drain_directs_safe_at`](../../sequencer-core/src/scheduler/mod.rs)). The
   `safe_block` is the sequencer's commitment that it has accounted for all
   directs up to that block; the scheduler enforces the drain regardless.
2. **Then execute the frame's user ops**, each through the single entry point
   [`validate_and_execute_user_op`](../../sequencer-core/src/application/mod.rs)
   with `(frame.fee_price, frame.safe_block)`. A user op is silently skipped
   (no state change, no output) when its signature is unrecoverable, when app
   `validate_user_op` rejects it, or when the protocol `max_fee ≥ fee_price`
   guard fails — the fold is a pure deterministic function and emits no
   diagnostics at the library seam.

This ordering — directs ≤ `S_K` then ops validated on top of them — is exactly
what the inclusion lane writes into frame K's wire content
([I2](../invariants.md#i2-drain-attribution-drained-directs-land-in-the-new-frame)),
which is what keeps the off-chain prediction faithful.

---

## The three implementations (and why they agree)

I1 names three places this algorithm lives. They are not three rewrites; two are
*derived* from the first.

| # | Implementation | Role | Source |
|---|---|---|---|
| 1 | **Canonical fold** `Scheduler<A>` | on-chain authority + bare-metal recovery fold | [`scheduler/mod.rs`](../../sequencer-core/src/scheduler/mod.rs) |
| 2 | **Off-chain predicate** `scheduler_accepts` + `advance_expected_batch_nonce` | builds the gold frontier (`safe_accepted_batches`) | [`protocol.rs`](../../sequencer-core/src/protocol.rs) |
| 3 | **Inclusion lane live prediction** | drains + executes ahead of L1 to issue soft confirmations | [`ingress/inclusion_lane/`](../../sequencer/src/ingress/inclusion_lane/) |

- **#1 is the authority.** It is the literal scheduler source; the recovery
  engine [`fold_replay`](../../sequencer-core/src/scheduler/fold.rs) *drives* it
  (seeds the fridge, replays the stream), so recovery is consistent with L1 by
  construction rather than by a parallel re-implementation. It is not a fourth
  copy.
- **#2 is a predicate, not the full fold.** `scheduler_accepts` answers only
  "would the scheduler accept this batch into the frontier?" — it checks sender,
  decode, staleness, and nonce, and **deliberately omits gate d** (the
  structural frame checks). That omission is sound by *self-trust*: the frontier
  simulator runs over the sequencer's **own** batch submissions, which are
  well-formed by construction; a structurally-malformed batch would be a
  self-bug (a fault state to crash on), not an adversarial input to predict.
  This document is the cross-reference home for the omission — the asymmetry is
  intentional, not a missing check.
- **The expected-nonce fold** is homed once, next to `scheduler_accepts`, as
  [`advance_expected_batch_nonce`](../../sequencer-core/src/protocol.rs). The
  submitter's `decide_submit_start` consumes it directly. The frontier builder
  `populate_safe_accepted_batches` keeps a deliberate inline copy of the same
  advance — its loop interleaves the advance with two storage-only side effects
  (the R2 content-identity check and the `canonical_divergence` freeze) that
  cannot move below the protocol layer; sharing the fold there would force a
  callback contract. The duplication is intentional and documented at the call
  site.

**Why the agreement is load-bearing:** the gold frontier (#2) is what
recovery's cascade pivots on and what promotion trusts; the lane (#3) is what
users see as soft confirmations. If any of the three computes a different
accept/reject/order than the canonical fold (#1), the sequencer will have
promised users a future the scheduler will not produce. No mechanism enforces
the agreement — only review and the duality tests. **Change one, check all.**

---

## Invariants this rests on

- [I1](../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)
  — the three implementations agree (this document is its prose).
- [I2](../invariants.md#i2-drain-attribution-drained-directs-land-in-the-new-frame)
  — drained directs land in the new frame, so the lane's wire content matches
  the fold's drain-before-ops order.
- [I3](../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine)
  — frame `safe_block`s are non-decreasing, the lane-side mirror of gate d's
  monotonicity rule.

## Test-pinned properties

The duality's load-bearing edge cases each have at least one test
([`scheduler/mod.rs`](../../sequencer-core/src/scheduler/mod.rs) tests,
[`protocol.rs`](../../sequencer-core/src/protocol.rs) tests):

- **Empty batches are valid no-ops that consume the nonce** — no first frame to
  measure staleness against (`empty_batches_are_valid_noops`,
  `scheduler_accepts_empty_frames_batch_regardless_of_age`).
- **Stale batches do not consume the nonce** and the next batch reuses it
  (`stale_batch_is_skipped_without_consuming_nonce`).
- **Drain uses an inclusive `inclusion_block ≤ safe_block` rule**
  (`frame_drain_uses_consistent_inclusive_safe_block_rule`).
- **The backstop drains all overdue directs before the batch**
  (`pre_batch_backstop_executes_overdue_directs_before_user_ops`,
  `backstop_drains_all_overdue_directs`).
- **Staleness boundary is `≥ MAX_WAIT_BLOCKS`** — accepted at `MAX_WAIT − 1`,
  skipped at `MAX_WAIT` (`scheduler_accepts_boundary_just_below_stale`,
  `is_scheduler_stale_reports_true_at_and_past_threshold`).
- **Structural rejects don't consume the nonce**
  (`non_monotonic_safe_blocks_invalidate_batch`,
  `frame_safe_block_above_inclusion_block_invalidates_batch`,
  `wrong_batch_nonce_is_rejected_without_consuming_nonce`).
