# One recovery: when batches stop landing

**Status:** a plan; none of it is code yet. It deletes the
[standard recovery](../../recovery/README.md), the automatic one, and leaves
[cockroach recovery](../../recovery/cockroach.md) as the only recovery.

## The scenario

Batches stop landing on L1. The provider fails, the submitter's wallet runs
dry, fees outrun what the poster pays, a bug stops the poster, or the process
is down. It is not a blip: monitoring pages people after about two hours, and
they work on the cause. It ends one of two ways:

1. **Fixed inside the window.** The sequencer posts its backlog in order, the
   batches are accepted, and no client notices.
2. **Not fixed.** The oldest batch L1 has not accepted can no longer be
   accepted, so nothing after it can be either. The sequencer stops for good,
   and the operators run cockroach recovery.

Today there is a third ending. Before the window closes, the standard recovery
stops the sequencer, settles the submitter's wallet, cuts the history back to
the accepted batches, and resumes in the same database. This plan removes it:

- **It saves no confirmation.** It discards everything past the accepted
  batches, which is exactly what ending 2 loses.
- **It cannot finish before the people do.** To replace closed batches it must
  first land transactions, so with a dead provider or an empty wallet it waits
  for the same fix.
- **Nobody is asleep.** It fires hours into an incident that has already paged
  everyone.
- **Every client must build its mirror**: notice the cut, roll back,
  subscribe again. A client builds the cockroach playbook anyway.
- **It is the most complex part of the sequencer**: about 7,000 lines of Rust
  and SQL with their tests, two TLA+ models, 22 of the 53 devnet scenarios,
  an exit code, two operator flags, and a second identity (the recovery
  generation) on every client interface.

What it buys is unattended resumption in the same database, after an outage
or a stop of any length. Without it:

- **A sequencer stopped or blind for longer than the window needs cockroach
  recovery**, even when it had confirmed nothing. A running sequencer with no
  traffic is not affected: it seals its open batch by time, whatever it
  holds, and posts it.
- **Manual recoveries become more frequent.** Recoveries in total do not, and
  the process exits on estimates and stale views go away.

## The model

### The window

`max_wait_blocks`, written `W` here, is a constant of the application's
template. The [scheduler](../../protocol/scheduler-semantics.md#processing-an-input)
uses it twice:

- **The backstop.** Before anything else, every input executes the directs
  that have been queued for `W` blocks or more.
- **Staleness.** A batch that lands `W` blocks or more after its first frame's
  safe block is skipped, and the batch nonce stays
  ([gate e](../../protocol/scheduler-semantics.md#batch-validity)). Every
  later batch then fails the nonce gate.

The sequencer's lane predicts neither. It assumes its batches land fresh,
and until one lands stale the prediction holds: the backstop executes the
directs that the next frame would have drained, in the same order.

### When the history is void

Let `H` be the safe head, with every input through `H` already judged. Take
the oldest batch L1 has not accepted, closed or open, and let `s` be its first
frame's safe block. Any landing still to come is in block `H + 1` or later. So
once

```text
H + 1 - s >= W
```

that batch is stale wherever it lands, its nonce is never consumed, and no
later batch can be accepted. Everything the sequencer served past the accepted
batches is void. This is the fact the sequencer acts on:

- **It is a proof from observed L1.** No clock, no estimate, no margin.
- **It stays true.** `H` only rises, `s` is fixed when the batch opens, and
  nothing repairs. So it needs no stored flag: every start re-derives it from
  the database, before L1 is even reachable.
- **It never fires on a batch that could still land.** The sequencer's own
  acceptance check is looser than the scheduler's, so a batch it counts as
  unaccepted is unaccepted on chain.

The trigger is not the force drain. A drain alone does not contradict the
lane, and a history can be void with no direct queued at all.

### The budget

From the moment batches stop landing, the operators have the window, less the
age of the oldest unaccepted batch at that moment, less the safe head's lag of
about 64 blocks. With a 24-hour window and the default two-hour batch, that is
between about 21.5 and 23.5 hours. A stopped sequencer spends the same budget:
its open batch ages while it is down.

## Scenarios

| Situation | The sequencer | Operators | Clients |
|---|---|---|---|
| Batches stop landing; fixed inside the window | Keeps serving and sealing; posts the backlog in order once it can | Paged by the landing lag; fix the cause | Nothing |
| A restart during that outage | Starts as always; repairs nothing | Nothing more | Reconnect |
| Not fixed: the oldest unaccepted batch is void | Stops for good; every `run` refuses | Cockroach recovery | A new era |
| Blind past the window: the provider is down | Cannot see it; stops at the first sync that shows it | The same | The same |
| Stopped past the window, even with nothing confirmed | Refuses at the next start | The same | The same |
| A foreign batch at our nonce, a fridge overflow, a bug | As today: the divergence marker, or the watchdog | Cockroach recovery | A new era |

## What the sequencer does

**At start:** check the terminal facts on the database as it is, sync L1, check
them again, open a batch if none is open, run. The terminal facts are a
recorded divergence and a void history. Nothing is repaired, flushed or
replaced. When the first sync fails because the provider is down, `run` starts
on its stored view, as a running sequencer carries on through the same outage.

**While running:** the worker that polls for danger today keeps its place and
loses its thresholds. Each tick it reads the divergence marker and the oldest
unaccepted batch. It stops the process the way a divergence stops it today,
on either fact.

```rust
/// The oldest batch L1 has not accepted, closed or open, at the safe head.
pub struct LandingLag {
    pub batch: BatchId,
    pub first_frame_safe_block: u64,
    pub safe_block: u64,
}

impl LandingLag {
    pub fn blocks(&self) -> u64 {
        self.safe_block.saturating_sub(self.first_frame_safe_block)
    }

    /// No block after the safe head can accept this batch.
    pub fn is_void(&self, max_wait_blocks: u64) -> bool {
        let next_block = self.safe_block + 1;
        age_exceeds(next_block, self.first_frame_safe_block, max_wait_blocks)
    }
}
```

**Two numbers for the operator's monitoring**, with no threshold inside the
sequencer:

- the **landing lag**, `H - s` in blocks, beside the window;
- the **safe head's age** in seconds, because the lag freezes while the
  provider is down.

The sequencer has no metrics exporter, only logs and `/healthz`. The plan
logs both numbers and adds them to `/healthz`
([still to decide](#still-to-decide)).

**What it does not do:**

- **It does not stop intake.** An operator who wants no new confirmations
  during an outage closes `POST /tx` at the reverse proxy.
- **It does not estimate.** No danger margin, no wall-clock guess at L1's
  progress, no refusal on a stale view.
- **It does not stop posting.** The poster retries as it does today; a void
  batch wastes gas only until the next safe head shows it.

## Playbooks

### Batches are not landing

No document tells an operator this today.

1. **Watch the landing lag and the safe head's age.** Alert at a fraction of
   the window; the two-hour page is the operator's threshold, not the
   sequencer's.
2. **Find the cause**: the provider, the submitter's balance, fees
   ([fee policy](../../l1-fee-policy.md)), L1 itself, or a crash loop.
3. **Keep `run` up.** It posts the backlog the moment it can. A stopped
   sequencer posts nothing, and its open batch keeps aging.
4. **To stop adding confirmations that may be lost**, close `POST /tx` at the
   proxy. The feed and the poster carry on.
5. **Do not flush the wallet.** `flush-mempool` in a live era replaces batches
   that are waiting to land.

### The sequencer stopped for good

`run` exits with the terminal code and refuses every later start. The remedy
is [cockroach recovery](../../recovery/cockroach.md#the-operators-playbook),
with one step this plan adds in front of it:

- **When the old data directory survives, settle its wallet first**, with
  `flush-mempool` on that directory, before `setup` runs in the new one. The
  old directory holds the wallet-nonce watermark, which covers every slot the
  old instance ever broadcast at. Setup's own flush starts from an empty
  database and covers only what the provider's pool still remembers
  ([the flush and the stop block](../../recovery/cockroach.md#the-flush-and-the-stop-block)).
  An outage that ends here is the case most likely to leave batches in flight,
  and a late one that lands in the new era reads as a foreign batch and costs
  another recovery. After `setup`, the old directory is never signed for
  again, as today.

What is lost is what any cockroach recovery loses: everything served that had
not landed by the stop block. Those user ops are revoked, not cancelled.

### Clients

Within an era the history only appends. A state saved at any count from the
era's base to its head always resumes, and `/nonce` never goes down. The one
event a client handles is a new era, by
[the client's playbook](../../recovery/cockroach.md#the-clients-playbook).
There is no generation, no cut to detect and no compatibility query.

## The window belongs to the application

`MAX_WAIT_BLOCKS` is a constant of `sequencer-core` today, 1200 blocks. It
moves beside the template's other two scheduler constants, which the
application already returns from a static method:

```rust
pub struct SchedulerLimits {
    /// A direct queued this long is executed; a batch this old is skipped.
    pub max_wait_blocks: u64,
    pub fridge_bound: u64,
    pub max_direct_payload_bytes: u64,
}

pub trait Application {
    fn scheduler_limits() -> SchedulerLimits;
    // ...
}
```

- **Nothing in the scheduler changes.** `SchedulerConfig`, in Rust and in
  `scheduler.h`, already takes the window as a field; only
  `SchedulerConfig::for_application` reads the crate constant.
- **The C engine's limits record gains the field**, so libdex supplies its own
  window ([`application-engine.h`](../../../bindings/c-app-engine/include/application-engine.h)).
- **Setup pins it with the deployment identity**, as it pins the payload cap,
  and storage reads it from that row. A binary with another window would judge
  acceptance and the void history by the wrong rule, so `run` refuses it.
  The pin binds the binary to the data directory, not to the deployed
  template: a machine's scheduler export carries no constants.
- **The window is per application type, not per deployment.** The wallet's
  devnet and Sepolia builds are one type, so they share a value.

## What goes

| Area | What goes | About |
|---|---|---|
| Startup and runtime | The repair selection and its admission witness, the danger thresholds and wall-clock estimates, exit code 10, `--preemptive-margin-blocks`, `--l1-read-stale-after-blocks` | 2,000 lines |
| Storage and schema | The cascade and the Tip repair, `batches.invalidated_at_ms` and every "valid" filter and view, `history_generation_cuts`, the safe head's sync stamp | 3,700 lines |
| The client interfaces | `RecoveryGeneration`, `STALE_GENERATION`, `from_generation` and `compatibility` on `/history`, `X-Recovery-Generation`, the generation in the WS claim, the SDK's handling | 1,100 lines |
| The workers | The lane's recovery test and the reader's progress stamp | 300 lines |
| Devnet scenarios | 13 of 53 deleted, 9 rewritten | 116 of 512 s |
| Models | `preemptive.tla`, `admission.tla`, the archived `optimistic.tla` | 3 models |

Three simplifications follow from "no batch is ever replaced":

- **The batches are an append-only list.** "Valid" stops being a concept, the
  nonce is the era's anchor plus the batch's position, and one unique index
  replaces the partial one.
- **A history's identity is its era.** `HistoryVersion` goes; a claim is an
  era id and a count.
- **Startup has one path.** No repair is selected, so nothing has to be
  rechecked between preparing and launching.

## What stays, and why

- **The flusher and the wallet-nonce watermark.** Setup flushes to pin its
  stop block, and `flush-mempool` settles a wallet before a rebuild. The
  flusher moves from `recovery/` to `l1/`.
- **The content-identity check and the divergence marker.** They are
  independent of the standard recovery. With no nonce reused inside an era, a
  mismatch now means a leaked key, a transaction from an older era, or a bug.
- **Opening a batch at start.** Setup opens none, so the first `run` of an era
  opens its first batch.
- **The accepted checkpoint.** The comparison endpoints and snapshot
  collection still need the snapshot of the latest accepted batch; it stops
  being a rollback point.
- **The batch-open check.** `run` still refuses a `max_batch_open_seconds`
  that leaves no room to land inside the window. It matters more now: a batch
  that cannot land in time ends the era where it used to be repaired.
- **`--seconds-per-block`**, for the flush, the poster's confirmation timeout
  and that check.

## Order of work

Each step is its own commit, compiles and passes on its own, and carries its
docs. A step that touches a guest crate moves the template hash, so the images
are rebuilt before the machine and devnet suites run.

1. **The window moves to the application.** No behaviour changes: the wallet
   keeps 1200. Setup pins it; the C engine's record gains it.
2. **The sequencer stops for good on a void history.** Startup, the watching
   worker and the two numbers; the repair, its flags and exit code 10 go. The
   devnet scenarios are deleted or rewritten here, and the new drills land.
3. **The batches become an append-only list.** The schema and storage lose
   invalidation.
4. **A history's identity is its era.** The generation leaves the wire, the
   SDK, the OpenAPI document and the database.
5. **The docs and models that remain.** [Automatic recovery](../../recovery/README.md)
   becomes the page for batch landing, the void history and the outage
   playbook, at the same path. The invariant register, the threat model and
   `AGENTS.md` follow. The three TLA+ models go.

## Tests and drills

Each row of the [scenarios](#scenarios) is proven on the devnet at least once:

| Scenario | Drill |
|---|---|
| Fixed inside the window, live | **New.** Hold submissions back, advance L1 to just inside the window, release; the nonces are accepted in order and an old claim still subscribes |
| A restart during the outage | Kept: the pre-danger outage and the warm restart with L1 down |
| Void while running | Rewritten from the aging open batch: the process exits terminal, a second start refuses, then a rebuild and a new era |
| Blind past the window | Rewritten from the two provider outages: live, and at start |
| Stopped past the window | Rewritten from the stale-batches restart, and once with nothing confirmed |
| A foreign batch at our nonce | **New.** Post one from another key; the start refuses and the marker is set |

The harness needs four things it lacks: a way to hold submissions back while
L1 advances, a `run` that returns its exit status, a reader for the recorded
terminal fault, and a raw InputBox input. Mining the window is one call and
takes milliseconds, so the wallet's window does not need to shrink for tests.

## Still to decide

- **Where the two numbers are read.** Logs alone, or `/healthz` too. The plan
  proposes both; `/healthz` is the only place a probe can read them without
  parsing logs, and it is an interface change.
- **The wallet's window.** At 1200 blocks with the default two-hour batch, a
  quiet wallet deployment has under two hours to land each batch, and a
  posting stall past that now ends the era. Moving the wallet to 7200 means
  regenerating the guest scenarios of the consensus vectors, which fix block
  numbers around 1200. The plan proposes it as its own commit after step 1.
- **The C engine ABI's version.** Version 2 exists only on this branch. A bump
  to 3 costs a line and refuses an engine built against today's header, which
  would otherwise report a window of zero.
- **The TLA+ models.** Inside an era no nonce is reused, so the race they
  guard cannot occur there, and neither model has eras. The plan proposes
  deleting all three and keeping the old counterexample, in prose, as the
  reason a nonce is never reused in place.
- **The batch's key.** A batch nonce now names one batch for the life of an
  era, so `BatchId` and the nonce are two names for one thing. Making the
  nonce the key touches every table that points at a batch; decide after
  step 3.
- **Settling the old wallet before a rebuild.** The step
  [above](#the-sequencer-stopped-for-good) reverses today's rule that the old
  directory is never signed for again, for the time before `setup`. It needs a
  drill before it enters the cockroach playbook.
