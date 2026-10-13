# Batch-scoped replay protection (deferred)

**Status:** deferred design, not implemented. The current rule is the
per-sender nonce in the [application contract](../protocol/application-contract.md#user-nonces).
Revisit when replay state must become prunable, or when the fee moves from the
frame to the batch.

## Why

The nonce rule makes replay protection depend on all history. Every account's
expected nonce lives in application state forever, so state cannot be pruned.
That state is the "historical fold" each implementation must carry.

Moving the fee to the batch, signed by the user, hits a race. A user who reads
the current fee, signs, and submits can find the batch already sealed. The
batch-scoped design fixes that race too.

## Design

**Signed coordinates.** The user signs `(b, i)` in place of a nonce:

- `b` is the scheduler batch nonce the op targets: the canonical batch sequence
  number, not the sequencer's local `batch_index`.
- `i` orders the sender's ops within `b`.

**Look-back.** The sequencer adds an unsigned field `k` (2 bits, so `k` ∈ 0..=3)
when it builds the batch. Executing batch `c`, the scheduler recovers
`b = c - k` and verifies the signature over it. `b` itself never goes on the
wire. A user who signed for a batch that sealed meanwhile is still included,
in a later batch with `k ≥ 1`.

**Acceptance.** Per sender, keep the last consumed `(b, i)`. Accept an op only
if its `(b, i)` is lexicographically greater, then store it.

- `i` need not equal the op's position or start at 0, so a rejected op does not
  break the ones after it.
- Ops of one sender targeting the same `b` may split across batches `b..b+3`;
  order is still enforced.

**Pruning.** Drop a sender's entry once its stored `b < c - 3`. This is sound:
any op still in the window has `b ≥ c - 3`, so it would pass the comparison
against the dropped entry anyway. State is bounded by the senders active in the
last four batches, not by every account ever seen.

**Why the state is needed.** Without look-back (`k = 0` always), uniqueness of
`i` within batch `b` is checkable inside the batch, and the rule is stateless.
With look-back, a check confined to the including batch is replayable by a
Byzantine sequencer:

```
user signs X with (b=10, i=0)
batch 10: X with k=0 → recovers b=10, accepted
batch 11: X with k=1 → recovers b=10, accepted again under a within-batch check
```

The per-sender `(b, i)` entry rejects the second inclusion.

## Fee at the batch

- Each batch carries one fee, fixed when the batch opens and published before
  users sign. Signing `b` commits the user to `fee(b)`; no `max_fee` is signed.
- An op included through look-back pays `fee(b)`, not `fee(c)`. The scheduler
  therefore keeps the fees of the last four batches, a bounded addition to its
  state.
- Users minimize cost by targeting the cheapest batch in the window, so a fee
  increase fully applies only after `look-back` batches. The 10× fee headroom
  over DA cost absorbs that delay.

## Wire and DA

- Per op, the 4-byte nonce and 2-byte `max_fee` are replaced by a small `i`
  plus 2 bits of `k`. The fee appears once per batch, not per frame.
- The distribution of `i` is skewed toward 0, which leaves room for later
  compression.

## Recovery and invalidation

A stale batch does not consume its nonce, so a cascade rolls the open batch
back and replacements reuse the nonces. An op from an invalidated batch never
executed, but its signature stays executable while canonical `c <= b + 3`,
including when its `b` is above the rolled-back open batch. That breaks
re-signing, which today's nonce makes safe:

```
user signs X with (15, 0); batches 10..15 are invalidated; open batch is 10
user re-signs X' with (10, 0); it executes and stores (10, 0)
canonical c reaches 15: X arrives with k=0, (15, 0) > (10, 0), accepted again
```

The coordinates name a nonce, not a batch's content, so a lower re-signed copy
does not exclude the original. Adopting the design requires binding ops to
their branch; [open questions](#open-questions) lists tentative fixes.

The fee commitment has the same gap: a replacement batch at a reused nonce
`b` opens with its own fee, so an op signed against the original `fee(b)` can
be charged a fee it never saw.

The window state (per-sender `(b, i)`, recent fees, and any anchors) is
canonical state. Checkpoints and the cockroach recovery tool must carry it.

## What changes together

This is one coordinated contract change, agreed with the engine owner:

- the EIP-712 `UserOp` type;
- `WireUserOp`, and `Frame`/`Batch` (the fee moves to the batch);
- scheduler acceptance, the application contract's nonce rule, and the C ABI's
  validation inputs;
- `GET /nonce` and `GET /fee` (they become the open batch's nonce and fee, plus
  the sender's next `i`), and the SDK.

## Open questions

- **Binding ops to their branch (required before adoption).** Tentative fixes,
  not yet analyzed in depth. Most promising: each op also signs an anchor, the hash of the canonical batch at nonce `b - 1`, which the
  scheduler keeps for the window. A rebuilt `b - 1` then invalidates every
  signature made against the old branch, as a recent block hash does elsewhere,
  and re-signing becomes safe. Alternatives: clients and `GET /nonce` never
  quote coordinates below a sender's outstanding signed op until `c > b + 3`;
  or the sequencer re-includes invalidated ops itself and users never re-sign.
- **Signing the fee.** Sign `fee(b)` without putting it on the wire: the
  scheduler fills it from its window, so a replacement batch with another fee
  makes the op recover a different address and skip, instead of overcharging.
  Or state that a replacement may charge its own fee, within policy.
- **Window width in time.** Four batches shrink to seconds under load, when
  batches seal on size. Size `k` against a minimum batch lifetime, or bound the
  sealing cadence.
- **Owner of the window state.** Replay protection is protocol-level. The
  scheduler could own it at the shared execution boundary, which would remove
  nonce logic from every application.
- **Width and encoding of `i`,** and its exhaustion rule.

## Why deferred

The per-sender nonce works, and the engine owner no longer needs to prune
replay state. The change spans signing, wire format, scheduler, ABI and API at
once, so it waits for a concrete trigger.
