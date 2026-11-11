# Batch landing and outages

The sequencer serves an optimistic history: it confirms a user op at once and
posts it to L1 later, in a batch. That promise holds only while its batches
land inside the application's staleness window. This page owns what happens
when they do not: the rule that ends an era, what the sequencer does about it,
and what an operator does.

The sequencer repairs nothing by itself. Its one recovery is
[cockroach recovery](cockroach.md): an operator rebuilds it in a new data
directory from a trusted canonical machine and L1, which starts a new era.
[Why there is no automatic repair](#why-there-is-no-automatic-repair) gives
the reasons.

The [scheduler contract](../protocol/scheduler-semantics.md) owns the canonical
acceptance rules, and the [invariant register](../invariants.md) owns
cross-module enforcement.

## Accepted and unaccepted batches

The sequencer's batches form one list in creation order: an **accepted
prefix**, then the batches L1 has not accepted yet, ending at the one **open
batch** that still takes user ops.

**Accepted** means the sequencer read a safe L1 input, applied the scheduler's
acceptance rules to it, and matched the landed bytes to its own closed batch at
that nonce. The match is recorded on the batch as `accepted_input`
([I9](../invariants.md#i9-acceptance-identity-accepted-nonce-n-means-our-batch-n)).
It does not mean a canonical machine was seen executing it; the
[watchdog](../watchdog/README.md) checks that. A foreign or different payload
accepted at one of the sequencer's nonces records a **divergence**, which ends
the era as well
([I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen)).

Everything past the accepted prefix is a prediction. A user op in an unaccepted
batch has been confirmed to its sender and served on the feed, but L1 has not
agreed to it yet.

## The window

`max_wait_blocks` is a constant of the application's template
([scheduler limits](../protocol/application-contract.md#3-admission--policy-never-consensus)).
The scheduler uses it twice
([processing an input](../protocol/scheduler-semantics.md#processing-an-input)):

- **The backstop.** Before anything else, every input executes the directs
  that have been queued for `max_wait_blocks` or more.
- **Staleness.** A batch that lands `max_wait_blocks` or more after its first
  frame's safe block is skipped, and the batch nonce stays. Every later batch
  then fails the nonce gate.

The sequencer's lane predicts neither. It assumes its batches land fresh, and
until one lands stale the prediction holds: the backstop executes the directs
the next frame would have drained, in the same order.

## The void history

Let `H` be the safe head, with every input through `H` read and judged. Take
the oldest batch L1 has not accepted, closed or open, and let `s` be its first
frame's safe block. Any landing still to come is in block `H + 1` or later. So
once

```text
H + 1 - s >= max_wait_blocks
```

that batch is stale wherever it lands, its nonce is never consumed, and no
later batch can be accepted. Everything served past the accepted prefix is
**void**, and the era is over.

- **It is a proof from observed L1.** It uses no clock, no estimate and no
  margin, so it cannot fire early.
- **It never fires on a batch that could still land.** The sequencer's
  acceptance check is looser than the scheduler's
  ([I1](../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)),
  so a batch it counts as unaccepted is unaccepted on chain.
- **It stays true.** `H` only rises, `s` is fixed when the batch opens, and
  nothing repairs. So it is not stored: every start derives it again from the
  database, before L1 is reachable.
- **The open batch counts.** Its first frame is fixed when it opens, so it
  ages whether or not it holds a user op.

The trigger is not the backstop's drain. A drain alone does not contradict the
lane, and a history can be void with no direct queued at all.

```rust
/// The oldest batch L1 has not accepted, closed or open, at the safe head.
pub struct LandingLag {
    pub batch: BatchId,
    pub first_frame_safe_block: u64,
    pub safe_block: u64,
}

impl LandingLag {
    /// The batch's age at the earliest block it can still land in: `H + 1 - s`.
    pub fn blocks(&self) -> u64;
    /// No block after the safe head can accept this batch.
    pub fn is_void(&self, max_wait_blocks: u64) -> bool;
}
```

## What the sequencer does

A recorded divergence and a void history are the two **terminal facts**. While
either holds, `run` refuses, and neither ever stops holding.

**At start**, `run`:

1. checks the terminal facts on the database as it is, before any L1 call;
2. syncs L1 to the safe head;
3. checks the terminal facts again;
4. opens a batch if none is open, which is how the first `run` of an era gets
   its first batch;
5. launches its workers.

It repairs, flushes and replaces nothing. When the first sync fails because the
provider is down, `run` starts on its stored view, as a running sequencer
carries on through the same outage. The first `run` of an era is the
exception: it has no open batch yet, and it exits 20 rather than open one at a
safe head that may be hours old, since a batch's first frame is fixed when it
opens. Any other sync failure exits 20 or 30, by its class.

**While running**, one worker reads the terminal facts every two seconds and
stops the process on either. It does not write the database and does not call
L1. The lane does not read them, so a user op confirmed in the seconds between
the safe head showing a fact and the stop is lost with the rest.

**How it stops.** A start that finds a terminal fact exits 30 and names it. A
running process that finds one aborts (SIGABRT, 134), as it does for any
diagnosed terminal fault; a clean shutdown that finds one exits 30 instead of
0. A supervisor treats all three as terminal
([restart policy](../watchdog/operator-deployment.md#sequencer-restart-policy)).

**It reports two numbers**, with no threshold of its own:

| Number | Meaning |
|---|---|
| `landing_lag_blocks` | `H + 1 - s` for the oldest unaccepted batch: its age at the next block. The history is void when it reaches `max_wait_blocks` |
| `safe_head_age_seconds` | The wall clock less the safe block's L1 timestamp. The lag freezes while the provider is down; this number does not |

Both are in [`GET /healthz`](../../README.md#health-probes-internal-only),
which answers `null` for them until the worker's first reading, and in a
`landing lag` log line, written at start and every five minutes.

**What it does not do:**

- **It does not stop intake.** An operator who wants no new confirmations
  during an outage closes `POST /tx` at the reverse proxy.
- **It does not estimate.** A stale L1 view refuses nothing, at start or while
  running.
- **It does not stop posting.** The poster retries as always; a void batch
  wastes gas only until the next safe head shows it.

## The budget

From the moment batches stop landing, the operators have the window, less the
age of the oldest unaccepted batch at that moment, less the safe head's lag of
about 64 blocks. A stopped sequencer spends the same budget: its open batch
ages while it is down.

A running sequencer with no traffic stays inside it. The lane seals the open
batch after `max_batch_open_seconds`, whatever it holds, and the poster posts
it. `run` refuses a `max_batch_open_seconds` that leaves no room to land: the
window less a submission margin of 100 blocks
([`RunConfig`](../../sequencer/src/commands/config.rs)). The check assumes L1
blocks are no faster than `seconds_per_block` and does not bound inclusion
time.

## Scenarios

| Situation | The sequencer | Operators | Clients |
|---|---|---|---|
| Batches stop landing; fixed inside the window | Keeps serving and sealing; posts the backlog in order once it can | Paged by the landing lag; fix the cause | Nothing |
| A restart during that outage | Starts as always | Nothing more | Reconnect |
| Not fixed: the oldest unaccepted batch is void | Stops; every `run` refuses | Cockroach recovery | A new era |
| Blind past the window: the provider is down | Cannot see it; stops at the first sync that shows it | The same | The same |
| Stopped past the window, even with nothing confirmed | Refuses at the next start | The same | The same |
| A foreign batch at one of its nonces | Records the divergence and stops; every `run` refuses | Cockroach recovery | A new era |

Each row is a devnet drill
([`test_cases.rs`](../../tests/e2e/src/test_cases.rs)), run in CI by
`just test-rollups-e2e`:

| Situation | Drill |
|---|---|
| Fixed inside the window | `batches_stop_landing_inside_the_window_then_land_test`, and at a restart `dropped_submission_is_posted_again_after_restart_test` |
| A restart during the outage | `restart_after_a_short_outage_keeps_history_test`, `restart_inside_the_window_keeps_history_then_lands_test`, `warm_restart_with_l1_down_test` |
| Void while running | `void_history_while_running_stops_for_good_test`, through the rebuild to the new era |
| Blind past the window | `blind_past_the_window_stops_at_the_first_sync_that_shows_it_test`, and at start `blind_start_boots_on_the_stored_view_then_stops_at_the_first_sync_test` |
| Stopped past the window | `stopped_past_the_window_refuses_every_start_test`, and with nothing confirmed `stopped_past_the_window_with_nothing_confirmed_refuses_test` |
| A foreign batch at one of its nonces | `foreign_batch_at_our_nonce_stops_for_good_test` |
| The first `run` of an era with the provider down | `first_run_of_an_era_with_l1_down_retries_test` |

## When batches are not landing

1. **Watch the landing lag and the safe head's age.** Alert at a fraction of
   the window that leaves time to act.
2. **Find the cause**: the provider, the submitter's balance, fees
   ([fee policy](../l1-fee-policy.md)), L1 itself, or a crash loop.
3. **Keep `run` up.** It posts the backlog the moment it can. A stopped
   sequencer posts nothing, and its open batch keeps aging.
4. **To stop adding confirmations that may be lost**, close `POST /tx` at the
   proxy. The feed and the poster carry on.
5. **Do not flush the wallet.** `flush-mempool` runs only with `run` stopped,
   and it replaces the batch transactions waiting in the mempool with no-ops.
   `run` must then post those batches again, which spends the window.

## When the sequencer has stopped for good

The running process aborts, and every later start exits 30, naming the batch,
its first frame's safe block, the safe head and the window
([how it stops](#what-the-sequencer-does)). Restarting does not help, and
nothing in the old data directory needs repair: follow
[the operator's playbook](cockroach.md#the-operators-playbook) for a cockroach
recovery.

What is lost is what any cockroach recovery loses: everything served that had
not landed by the stop block. Those user ops are revoked, not cancelled; anyone
holding one can submit it again while its sender's nonce is unused.

Clients handle one event, the new era, by
[the client's playbook](cockroach.md#the-clients-playbook).

## Why there is no automatic repair

A repair in place would stop the sequencer short of the window, settle the
submitter's wallet, cut the history back to the accepted prefix and resume in
the same database. The sequencer does not do it, for these reasons:

- **It would save no confirmation.** It discards everything past the accepted
  prefix, which is exactly what a void history loses.
- **It could not finish before the people do.** To replace closed batches it
  must first land transactions, so with a dead provider or an empty wallet it
  waits for the same fix.
- **Nobody is asleep.** It would fire hours into an incident that has already
  paged everyone.
- **Every client would have to mirror it**: notice the cut, roll back,
  subscribe again. A client builds the cockroach playbook anyway.
- **It would be the most complex part of the sequencer**: replacing a batch
  reuses its nonce, so a transaction of the old batch that lands late executes
  against the new history unless every wallet slot is settled first. The
  [archive](history/README.md) keeps the models of that race and its
  counterexample.

What it would buy is unattended resumption in the same database, after an
outage or a stop of any length. The price of doing without it is that a
sequencer stopped or blind for longer than the window needs a cockroach
recovery, even when it had confirmed nothing.

Inside an era no batch nonce is reused, so that race cannot occur: a late
transaction of the sequencer's carries the bytes of a batch it still holds,
and it is accepted as that batch or skipped.

## Code map

| Concern | Owner |
|---|---|
| The landing lag and the terminal facts | [`storage/landing.rs`](../../sequencer/src/storage/landing.rs) |
| Startup: check, sync, check, open a batch | [`commands/run/`](../../sequencer/src/commands/run/) |
| The watching worker and the reading `/healthz` serves | [`landing.rs`](../../sequencer/src/landing.rs), [`egress/api/health.rs`](../../sequencer/src/egress/api/health.rs) |
| Acceptance, content identity and the divergence marker | [`storage/acceptance.rs`](../../sequencer/src/storage/acceptance.rs) |
| The staleness formula and the off-chain acceptance check | [`protocol.rs`](../../sequencer-core/src/protocol.rs) |
| The batch-open check | [`commands/config.rs`](../../sequencer/src/commands/config.rs) |
