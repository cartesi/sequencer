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
fold, so those two targets agree *by construction*. This prose is the intended
protocol contract; the reference implementation embodies it but remains code
that can contain bugs and require hardening.

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
the capability-gated `Application::apply_direct_input` hook, reached through
the shared `execute_direct_input` boundary (see the
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

An `AppError` from either execution path is fatal: there is no canonical
successor to the partially evaluated input, so the canonical harness,
inclusion lane, catch-up, and recovery fold all stop rather than continue.

This ordering — directs ≤ `S_K` then ops validated on top of them — is exactly
what the inclusion lane writes into frame K's wire content
([I2](../invariants.md#i2-drain-attribution-accumulated-directs-land-in-the-clock-advanced-frame)),
which is what keeps the off-chain prediction faithful.

### Sequencer frame-clock policy

The scheduler constrains frame clocks to be non-decreasing and no later than
the batch inclusion block; it does not require a frame for every L1 block.
During an admitted live run, the sequencer advances logical frame time when the
latest persisted safe head `H` is at least five blocks beyond the open frame
clock `S`. It then opens exactly one frame at `H`, drains the complete
newly-covered direct range, and uses `H` for subsequent user ops. An observation
jump from 100 to 132 therefore creates one frame at 132, not synthetic frames at
each missed five-block boundary. Intermediate empty frames would execute
nothing, so their omission is scheduler-equivalent. Bootstrap and recovery are
anchoring transitions rather than live clock ticks: they may open a fresh Tip
at their proven checkpoint/current safe head without applying the five-block
delta.

Direct-input presence is not another rotation condition: the five-block tick
may have an empty direct prefix, while directs observed below the threshold
wait for the next tick and retain their own inclusion-block clock when
executed. Batch closure is orthogonal and necessarily creates the successor
batch's first frame at the unchanged `safe_block`; equal adjacent frame clocks
are valid. Thus block distance is the only reason logical frame time advances,
not the only reason a frame row exists.

---

## The application-history offset

The feed offset is semantically
[`Application::executed_input_count()`](application-contract.md#4-canonical-history-cursor-executed_input_count).
It starts at zero and names the next application input to execute. An
application at count `X` is ready for history entry `X`; successfully executing
that entry advances the application to `X + 1`.

The scheduler determines which InputBox material becomes an executed
application input. Its count transitions are therefore:

| Scheduler event | Application execution | Count effect |
|---|---|---|
| Direct arrives and is appended to the fridge | none yet | unchanged |
| Covered or overdue direct is successfully executed | `execute_direct_input` returns `Ok` | exactly `+1` |
| User op passes signature, protocol, and app validation and is successfully executed | `execute_valid_user_op` returns `Ok` | exactly `+1` |
| User op has an unrecoverable signature or is rejected by protocol/app validation | none | unchanged |
| Batch envelope is decoded, accepted, skipped, or rejected | none for the envelope itself; contained executions are counted by the rows above | unchanged for the envelope |
| Empty batch | none | unchanged |
| Either execution method returns `AppError` | fatal invariant failure | no canonical successor is defined |

The censorship backstop still runs before classification, so a malformed,
stale, or otherwise rejected batch can indirectly advance the count by causing
overdue directs to execute first. The batch itself never contributes an entry.

This coordinate must agree across the canonical fold, the inclusion lane,
catch-up/replay, and recovery. Standard recovery may replace an invalidated
suffix at the same application offsets; cockroach recovery resumes from the
absolute count persisted in the recovered application state even when older
history is no longer locally available.

> **Cutover status:** the typed execution boundary, scheduler count
> transitions, and durable per-input mapping are landed. Physical
> `sequenced_l2_txs.offset` remains SQLite rowid and the existing WebSocket
> still exposes that cursor; changing the public protocol to canonical offsets
> and `HistoryVersion` remains Track 3 work.

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
- **The content-identity check is complete relative to #2, not an independent oracle for #1.** For
  every at/above-anchor landing that `scheduler_accepts` accepts, the frontier
  builder exhaustively finds a byte-identical valid local closed batch
  (`Match`), no local batch (`Foreign`), or different bytes (`Mismatch`); the
  latter two durably freeze the frontier. Because the check shares #2 and its
  structural omissions, it cannot prove that #1, application state, or trusted
  collapsed history is correct. A structurally malformed foreign landing may
  conservatively record divergence even if #1 would reject it; that false
  positive is accepted under the sequencer self-trust model. See I9/I15 for the
  runtime and recovery boundary.
- **The expected-nonce fold** is homed once, next to `scheduler_accepts`, as
  [`advance_expected_batch_nonce`](../../sequencer-core/src/protocol.rs). The
  submitter's `decide_submit_start` consumes it directly. The frontier builder
  `populate_safe_accepted_batches` keeps a deliberate inline copy of the same
  advance — its loop interleaves the advance with two storage-only side effects
  (the content-identity check and the `canonical_divergence` freeze) that
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
- [I2](../invariants.md#i2-drain-attribution-accumulated-directs-land-in-the-clock-advanced-frame)
  — drained directs land in the new frame, so the lane's wire content matches
  the fold's drain-before-ops order.
- [I3](../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine)
  — frame `safe_block`s are non-decreasing, the lane-side mirror of gate d's
  monotonicity rule.
- [I19](../invariants.md#i19-application-progress-advances-only-at-the-shared-execution-boundary)
  — every successful application input advances one typed count/clock pair;
  rejected inputs do not, and `AppError` is terminal.

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

---

## Reference-scheduler count discipline

Every canonical input crosses the same typed execution
boundary in the scheduler, live lane, catch-up, and recovery fold:

1. Successful directs and validated user ops return their pre-execution
   `ExecutedInputCount` and advance exactly once with checked arithmetic.
   Rejection, bad signatures, envelopes, empty batches, and stale/structural
   skips leave it unchanged.
2. `AppError` is fatal everywhere; there is no parallel scheduler-only
   transition after a possibly partial hook failure.
3. Distinct opaque capabilities let application hooks mutate application state
   without granting them authority to overwrite scheduler-owned progress. The
   shared boundary checks progress before/after both successful and failing
   hooks and asserts getter coherence after commit.
4. Tests pin direct/user advancement, every skip/reject family, pre-batch
   overdue-drain ordering, overflow, dump round-trips, nonzero recovery bases,
   and live/replay mapping agreement.

Additional scheduler changes may still be batched separately, but they must
preserve the transition table above and the shared boundary rather than
reintroducing a scheduler-local count.
