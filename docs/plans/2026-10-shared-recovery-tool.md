# Shared recovery tool

**Status:** proposed design from the 2026-10-28 design session, revised on
2026-10-29 with the owner's answers; nothing here is implemented.
[Cockroach recovery](../recovery/cockroach.md) owns the current procedure. When
this design lands, its durable parts move there and this file keeps only
remaining work. Its claims were checked by reading the code at `3e92563`,
[PR #56](https://github.com/cartesi/sequencer/pull/56) at `ee5c72c`, Dave at
`dbcca7d4` and the emulator at v0.22.0-test3 (`7adfd492`, whose header and CLI
match v0.22.0); none was executed.

Cockroach recovery rebuilds a sequencer that is ready for new user ops after
an instance loses its database or its behavior cannot be trusted. The operator
triggers it by hand and keeps that instance down. The design takes the worst
case: the submitter key is ours, but the instance's database, snapshots and
receipts prove nothing. The canonical scheduler is the authority, bug for bug.
The rebuilt sequencer is assumed correct; if it is not, fix it and rebuild
again. Recovery must not depend on replaying the application from genesis.

Today's `setup --recovery` already rebuilds the sequencer alone. What makes
this tool *shared* is a client: Bart's DEX backend keeps a richer state than
the sequencer and must realign after a rebuild, from its own backup and with
its own code. The operator of the sequencer and of that client is the same
(Bart); only their development is split. Recovery runs rarely, so each of its
steps must be simple enough to explain, practice and trust.

## Terms

- **Template**: the initial machine whose root hash the on-chain application
  pins as `getTemplateHash()`. A deployed application keeps its template for
  life ([Cartesi Machine facts](../cartesi-machine.md#versions-are-all-or-nothing)),
  so a rule that changes the guest ships with a new template, which is to say
  a new deployment. A **machine** is any stored state of the canonical
  machine, such as a validator's at the end of an epoch or a watchdog
  checkpoint.
- **A base** is an InputBox position: the state after every input below an
  index and none from it. Each consumer has its own, and every source of state
  names its own: a watchdog checkpoint by its `<block>-<input count>` name,
  Dave's last finalized machine by the current sealed epoch's
  `inputIndexLowerBound`, the template by zero. Positions are never the app
  clock `last_executed_safe_block`, which every op of a frame shares and which
  can lag the raw boundary without limit
  ([history coordinates](../protocol/application-history.md#progress-ordering-and-acceptance)).
- **Counts.** The *executed-input count* numbers the feed's rows (`X`, `K`); an
  *InputBox count* numbers raw inputs and gives bases. They differ: a batch
  envelope or a queued direct is an input but no row.
- **`P` and `B`**, as in the watchdog's latch record: `P` is its head, the last
  block the sequencer agreed with, or the uncompared bootstrap block before
  the first agreement, and `B` the block of the comparison that failed.
- **`C`** is the stop block, a safe head fixed after the flush; **`N'`** is the
  next batch nonce at its end; **`F`** is the InputBox index of the oldest
  direct still queued there, or the end of `C` when none is; and **`L`** is the
  frame-clock floor, once its gate lands: the last accepted frame's
  `safe_block`, raised by a fridge overflow to the youngest drained direct's
  block.
- **Conformance vectors**:
  [`consensus_vectors_v1.json`](../../tests/fixtures/consensus_vectors_v1.json),
  frozen cases (EIP-712 hashes, signatures, SSZ bytes, fee conversions, guest
  scenarios with their exact outputs) that every port of the scheduler must
  reproduce ([conformance](../protocol/scheduler-semantics.md#conformance)).
  They pin no gate edge cases yet; versioned gate vectors for the next
  template's rules ship with it.

## The tool as a function

The scheduler splits into an automaton that reads no application state and
the application it feeds. The tool needs only the automaton. In ML notation:

```ocaml
(* One InputBox input. Its chain id and application address, the signature
   domain, are deployment constants. *)
type raw_input = { index : index; block : block; timestamp : time;
                   prev_randao : word256; sender : address; payload : bytes }

(* What the scheduler hands the application, in execution order. A direct is
   an input from any sender but the sequencer, passed on whole. *)
type direct = raw_input
type tx =
  | User_op of { sender : address; nonce : nonce; max_fee : fee; fee : fee;
                 payload : bytes; safe_block : block }
  | Direct of direct

(* The automaton: what the scheduler keeps besides the application. *)
type sched = { batch_nonce : nonce;     (* the next batch nonce *)
               fridge : direct queue;   (* admitted directs not yet executed *)
               floor : block }          (* L *)

val step : sched -> raw_input -> sched * tx list

(* The tool. Both fold step over every input since the deployment block;
   segment keeps what the inputs from index base to the end of stop emit. *)
val segment : base:index -> stop:block -> tx list
val sched : stop:block -> sched   (* at the end of stop *)
```

`step` accepts or rejects batches; queues, drains and overflows the fridge;
and emits, in execution order, the directs it drains and the user ops of the
frames it runs whose signature recovers a sender and whose `max_fee` covers
the frame fee. It reads no application state. It does read what the template
fixes: the sequencer address, `MAX_WAIT_BLOCKS`, the fridge bound and the
application's stateless rule for well-formed directs, so for libdex the
scheduler library embeds a port of Bart's rule. Signature recovery is pure,
and the user ops it emits are *candidates*: whether each one executes, which
reads its sender's nonce and, on today's template, its fee balance, is left to
the application that receives the segment. The canonical machine is `step`
followed by the guest's application, and the sequencer and every client are
other applications over the same entries. That holds while the guest never
answers an input with `RX_REJECTED`, as the
[fatal conditions](../protocol/scheduler-semantics.md#fatal-conditions)
require: the host's revert would roll the batch nonce and the fridge back with
the application, a [question for Bart](#questions-for-bart).

So the tool needs an L1 connection and the scheduler library for the
deployment's template, nothing else: no machine, no nonces, no application
state, and no application code beyond the template's rule for well-formed
directs. It reads every input since the deployment block, because the batch
nonce and the fridge at a base depend on all of them; before the earliest base
the fold need only decode batches and check gates, recovering no signature.
`segment ~base` keeps exactly the entries emitted by the inputs from index
`base` on. A direct that such an input drains is kept even if it arrived
earlier, and a user op belongs to its batch's input, never to its frame's
`safe_block`. One fold serves every base: the tool computes the entries from
the earliest base and slices them per consumer, as `/historical-l1-inputs`
slices by `next_input_index`. Each consumer receives its own list and never
applies that rule itself. No base lies past the end of `C`.

The consumers:

```ocaml
(* Runs each tx through the shared execution boundary, as the canonical
   application does: a user op its state does not authorize is skipped. *)
val run : state -> tx list -> state

(* m: a canonical machine that has consumed the inputs below index n *)
let sequencer m n c =
  import (run (project m) (segment ~base:n ~stop:c)) (sched ~stop:c)

(* s: a client state restored to the count X that the watchdog logged with
   an agreement at input count n *)
let client s n c = run s (segment ~base:n ~stop:c)
```

A segment is exact only for a consumer that sits exactly at its base: each
holds the nonces (and, on today's template, the balances) that decide
inclusion, as the canonical machine did there. Only the sequencer receives
`sched`. It imports `N'`, `F` and `L`, and its root frame drains the queued
directs ([below](#queued-directs-at-the-stop-block)); a client receives those
directs as the new era's first feed rows, like any other row, and never holds
scheduler state. Today's client procedure instead restores scheduler
continuation metadata and repeats the terminal drain
([projection replay](../protocol/projection-replay.md)).

### Why the tool hands out a segment

The alternative is a `RichClientApp` plugin that the tool runs to produce each
client's end state. The segment is better for three reasons:

- **Data crosses the boundary, not code.** The tool depends only on the
  scheduler, which a template fixes; a client's projection, schema and storage
  change on their own schedule. A plugin would tie every tool release to every
  client's code and effects.
- **One execution boundary.** A client already runs L2 transactions through
  the shared execution boundary, and recovery reuses it; only the driver
  differs. An entry carries what the canonical application reads, so no
  offset, no batch nonce (a direct the backstop drains belongs to no batch)
  and no transaction hash, and a user op the client's state rejects is
  skipped. A plugin would be a second path that runs once in a deployment's
  life and rots.
- **History, not only state.** Bart's client keeps trades and transfers, and
  running the segment from its base replays that history, in canonical
  order.

### What the count is for

The tool positions everything by InputBox index and block and never reads the
executed-input count, the application's count of executed directs and
included user ops, which numbers the feed's rows. The consumers use it three
ways:

- **Locating a client's base.** Feed rows do not carry the InputBox index of
  the input that emitted them: a direct row's index and block are its own
  arrival's, and a row's batch nonce is the sequencer's own attribution, which
  no comparison checks. So a watchdog agreement, an InputBox count paired with
  the count `X` that the sequencer reported there, is the map from a base to a
  position in the client's history that a comparison vouches for
  ([the client's base](#the-clients-base)).
- **Joining the new era.** The era serves rows from `K`, the sequencer's count
  at the end of `C`, which `/history` reports as `available_from`. After its
  segment, a client compares its count with `K` and subscribes there. The
  comparison must be explicit: the feed refuses a count below `K` but serves
  one above it. Equal totals are a weak check: a client whose state holds a
  soft user op past `X` that never landed skips any canonical op with the same
  sender and nonce, so the extra row and the skip cancel and it still ends at
  `K`. Restoring exactly `X` ([below](#the-clients-base)) rules this out.
- **Matching the machine.** The wallet's comparison bytes embed the count, so
  the sequencer's `K` must equal the machine's, and the projection from a
  machine must carry it.

The watchdog's checkpoint names, `<block>-<input count>`, and its latch
record's `agreed.input_count` hold InputBox counts: bases, not executed counts.

## States at a base

Each consumer's state must sit exactly at its base: a state that has consumed
one input fewer or more would drop or repeat entries. The bases may differ, and
each consumer gets its own segment.

### The sequencer's base

The sequencer's state is projected from a machine at its base. The
projection must carry the executed-input count and clock that the consumers'
offsets start from: the wallet's `state` inspect report holds both, and libdex
keeps its state as a flat buffer in one of the machine's drives, which may or
may not hold them
([application contract](../protocol/application-contract.md#7-canonical-recovery-integration)).
Any canonical machine the operator trusts will do, even one past a
divergence, since it was computed from L1 and not from the sequencer's
history; the tool enforces no provenance.

| Machine | Base | Trust |
|---|---|---|
| Dave's last finalized machine, the default | the current sealed epoch's `inputIndexLowerBound`, read on chain with its root and checked against its receive buffer; it can lie inside a block | its root is on chain |
| The watchdog's head | the input count in its name, at the end of a block | the operator's watchdog replayed it from L1; before its first agreement, the operator trusted it at `init` |
| `incident/canonical` after a `state_mismatch`, or a `replay --out` machine | the InputBox count at the end of the latched block `B`, or of the block it was replayed to | the operator's watchdog replayed it from L1 |
| The template | zero | its hash is on chain |

A matching root authenticates a machine only from a source we trust, such as
our own Dave node: a stored machine's root comes from its stored hash tree, not
from its contents ([hashing](../cartesi-machine.md#hashing)). An upcoming
emulator release brings a mechanism to verify an untrusted load; until then,
`verify_hash_tree` is not that mechanism and is
[on the watchdog's to-do list](2026-09-watchdog-v2.md#remaining-work).

### The client's base

A client's base must be a point where the watchdog compared the sequencer:
the sequencer's accepted checkpoint there gives the count `X` that locates
the base in the client's history, and the comparison vouches for the state.
An accepted checkpoint alone means only that our batch landed and passed a
partial gate check, which skips gate d
([I1](../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)),
so `/history` keeps advancing past a divergence that only the watchdog sees.
Once the fridge bound lands, a comparison also keeps the base at the end of
its block, provided the compared bytes embed the count: an overflow later in
the accepted block drains directs that the accepted checkpoint lacks
([snapshot lifecycle](../snapshots/lifecycle.md#acceptance-and-comparison)),
each of which advances the count even when it changes nothing else, and the
watchdog compares at the block's end. The wallet's bytes embed the count, so
its comparison pins `X` and the whole block; for libdex both depend on Bart's
answer.

The sequencer's digest response already carries `X`, but the watchdog parses
only the block and the digest, and deletes each checkpoint once a newer one
agrees. So it must append each agreement's block, InputBox count, `X`, and the
era and generation `X` was read under to a log, whose last line is its head;
the digest response adds the era and generation. A client then takes the
newest logged agreement on its own history, normally the head, without asking
a sequencer that may be gone or wrong: one at or below its count, from its own
era and its own or an earlier generation, since standard recovery never cuts
an accepted row. A newer generation's agreement can lie past a cut the client
missed while offline, which nobody can measure once the sequencer is gone.
Before the first agreement the log is empty and the head is the bootstrap,
which nobody compared. A client's base is then count zero: its initial state,
which needs no comparison, so its segment replays its whole history. That
window closes at the first tick that agrees with a checkpoint the sequencer
accepted after the bootstrap block, normally one batch after each `init`.

A client that follows the optimistic feed is past `X` by the time the
watchdog compares. Snapshotting at every batch end would cover every `X`, but
few backups can snapshot that often, so three rules make restorable, with any
backup, any `X` on the client's history:

1. **Every commit is a boundary and keeps its rows.** Apply each feed row, or
   each run of rows, in one SQLite transaction that also writes the count, era
   and generation and stores the rows themselves. Any consistent copy of the
   database is then at an input boundary, says which, and holds every row
   before it.
2. **A cut discards later snapshots.** When a standard recovery cuts the
   history at its preserved count
   ([checkpoint compatibility](../protocol/application-history.md#checkpoint-compatibility-after-standard-recovery)),
   or a rebuild at the client's base, the client, once realigned, deletes
   every snapshot past the cut and stops snapshotting its old live database:
   the new line replaces those rows and reuses their counts. After a rebuild
   it does this between its commit at `K` and its snapshot there. Every
   remaining snapshot then lies on the current history.
3. **Snapshot anywhere.** Take snapshots when convenient; none needs to line up
   with a checkpoint.

To reach `X`, restore the newest snapshot whose count is at most `X`, replay
the rows after it, read from the live database or a newer snapshot, to exactly
`X` through the client's normal apply, and check the count. A snapshot past
`X` is never used, which removes the trap of applying soft rows twice. Replay
is exact only if applying a row reads nothing but the database and the row. A
rebuilt sequencer serves only its own era, so the client keeps its old live
database until it has realigned: with the newer snapshots, it is the only copy
of the old era's rows up to `X`. After a rebuild the client snapshots at `K`,
since the rows before it reached it through a segment, and keeps that segment
with its base.

Notes on snapshotting a large SQLite database, from the SQLite, Litestream,
OpenZFS and AWS documentation as of 2026-10-29:

- Volume or filesystem snapshots (ZFS, LVM thin, EBS) are the cheap choice at
  terabyte scale: atomic and incremental. A ZFS snapshot holds every completed
  write, so SQLite recovers it to its last committed transaction. An EBS
  snapshot holds only what reached the volume, as a power cut would: with
  `synchronous` NORMAL or FULL that only moves its count back, but with OFF it
  can be corrupt. The database and its `-wal` file must be in the same
  snapshot. Restore by cloning a snapshot, never by rolling back the live
  volume, whose rows the replay reads.
- Litestream suits gigabytes rather than terabytes: by default it uploads a
  full snapshot every 24 hours and keeps 24 hours. It restores only to the
  end of one of its own files (`-txid`, `-timestamp`), which can batch many
  transactions, so pick one at or below `X` and let the rows in the live
  database cover the rest. It deletes only by age, so it cannot discard the
  restore points past a cut (rule 2): at a cut, replicate to a new path, and
  restore from the old path only at or below the cut.
- The backup API and `VACUUM INTO` copy every page on every run. On a busy
  database a copy either restarts after each write from another connection or
  holds one read transaction, which stops WAL checkpoints until it ends.
- Every SQLite engine that opens a WAL database needs 3.51.3 or later (or the
  3.44.6 or 3.50.7 backport) once two connections, in threads or processes,
  can write or checkpoint at once: earlier versions can corrupt it. Litestream
  checkpoints with its own embedded engine, fixed from v0.5.17. WAL mode fixes
  the page size, so choose it at creation.
- Retention is by position, not age: always keep the newest snapshot at or
  below the client's base, the `X` of the newest logged agreement on its
  history, and every newer one on the current history. The head stays put
  while the watchdog is latched, idle or stopped, so an age window, such as
  Litestream's default of 24 hours, can delete the only usable base. A bug in
  the client, or a sequencer fault diagnosed before the head, needs a snapshot
  older than it, which only a longer retention covers.

### After an incident

The sequencer may start from any canonical machine. After a sequencer
incident a client starts from the newest logged agreement on its history,
normally the watchdog's head. An agreement vouches for the sequencer's state,
not for the order of the rows before it: rows that commute reach the same
state in either order, so a matching state does not validate the client's
history
([reader state](../recovery/cockroach.md#application-specific-reader-state)).
If diagnosis finds the fault began before that agreement, the client starts
from an older logged agreement that predates it.

- **The watchdog latched a `state_mismatch`.** Its head stays at `P`, which
  need not be the block before `B`. A tick compares only the
  newest accepted checkpoint, so the accepted checkpoints in `(P, B)` were
  never compared, and the compared state can first differ anywhere in
  `(P, B]` ([incident runbook](../watchdog/incident-runbook.md)). The client
  followed the faulty sequencer through all of them. The other latches call
  for no rebuild: a dead canonical machine ends the application
  ([preconditions](#preconditions-the-design-must-state)), and a regression is
  an operator error
  ([runbook](../watchdog/incident-runbook.md#inclusion_block_regressed)).
- **The sequencer lost its database, or its content-identity check tripped**
  ([I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen)).
  A tripped check answers the finalized routes with 503 and does not latch the
  watchdog, so its head stays at the last comparison before the trip.
- **The client has a bug.** This is the client's own repair, not a rebuild: it
  restores a snapshot older than the bug's first effect and replays its rows
  with the fixed code. To cross a rebuild, it replays to that rebuild's base
  and runs the segment it kept from it. It then deletes every snapshot newer
  than the one it restored, since they carry the bug's state, and snapshots
  again at each `K` it crossed.

## Queued directs at the stop block

At the end of `C` the canonical scheduler can still hold queued directs. The
fridge is always a suffix of the directs it admits, since frame drains, the
backstop and the bound's overflow drain all pop only its front, so `F`
describes it. A block cannot, because after faulty sequencing a frame whose
`safe_block` equals its inclusion block drains only part of that block.

The rebuilt sequencer therefore starts from its state at the end of `C`, with
its direct floor at `F` instead of at block `C`. Its root frame, at `C`,
executes the admitted directs in `[F, end of C)` as ordinary rows before any
user op, as the first canonical frame drains them, and clients receive them
through the feed with normal offsets. The sequencer keeps no fridge of its
own: its pending directs are the admitted directs in the undrained index range
of its input table, which setup already fills from the deployment block. The
change is small: the era records `F`, and the lane drains directs from the
later of `F` and the first input after its latest frame's `safe_block`. A
cascade that tears down every frame
([I16](../invariants.md#i16-the-batch-tree-has-exactly-one-valid-parentless-root-carrying-the-deployments-anchor-nonce))
re-roots there and drains them again. Acceptance still scans only blocks
after `C` ([replay boundaries](../recovery/cockroach.md#replay-boundaries)).

Today's procedure instead executes the queued directs during recovery (the
terminal drain) and publishes the result. That state is a prediction: the
canonical machine reaches it only inside the post-`C` input that empties the
fridge, often executing more there, so no comparison can check it, a client
that applied it must roll it back if a second rebuild comes, and a rebuild
that started from it would execute those directs twice. Track 3 kept the
terminal drain because replacing it would carry pending work across the
baseline
([scope boundaries](2026-07-track3-feed-replay-design.md#scope-boundaries));
that work crosses as one index.

## The procedure

1. **Choose the bases.** Each client restores its state to the newest logged
   agreement on its history that precedes any fault the diagnosis found
   ([the client's base](#the-clients-base)).
   The sequencer takes a canonical machine: by default Dave's last finalized
   one, or the watchdog's head, `incident/canonical` at `B` or a `replay --out`
   machine ([the sequencer's base](#the-sequencers-base)). Each base is an
   InputBox count.
2. **Flush, then fix `C`.** Settle the old instance's transactions, then take
   a safe head observed after they settle as `C`. The flush is best effort: it
   replaces what the provider's pool holds for the submitter account and waits
   until that is safe. It cannot rely on the old watermark, which does not
   survive every event that calls for a rebuild
   ([flush boundary](../recovery/cockroach.md#flush-and-stopping-block)). As a
   standalone subcommand, it is the only step that needs the key. Refuse if a
   submitter input above `C` is already ingested.
3. **Run the tool** for `C` and each base, from L1 alone. It refuses a base
   past the end of `C`, writes one segment per base, and writes `sched` at `C`
   last, as the commit point.
4. **Rebuild the sequencer.** Run its segment from its state projected from
   the machine, and import the result with `N'`, `F` and `L` in one
   transaction. Start `run` promptly. A late start is safe: once the root
   frame ages past the danger threshold, startup's
   [Tip repair](../recovery/README.md#open-tip-repair-without-flushing)
   reopens it at the current safe head, still draining from `F`.
5. **Realign clients.** Each client runs its segment from its restored state,
   reads `/history`, checks that its count equals `available_from`, records
   the returned era and generation with that count in one commit, deletes
   every snapshot past its base, takes a snapshot, keeps its segment, and
   subscribes with that claim.

The new era's first watchdog comparison checks the result a batch later: a
wrong segment, `N'` or `F`, or a guest compiled for another sequencer address,
shows up there as a divergence. Replaying the watchdog's head to `C` with
`replay`, which starts only at the end of a block, checks it before `run`, at
an emulated cost. The tool cannot
see a halt, which reads application state; a consumer that reaches the halting
transaction fails there, as the canonical machine did.

## Rules for the next template

The owner settled these on 2026-10-29. Each changes the guest, so each needs
Bart's agreement and ships with the next template, ours and his port's
([register](../review/register.md#canonical-rules-open-for-the-c-port)). The
tool needs none of them: `step` applies whatever the template applies, the
`L` gate and the fridge bound included. Only the fridge bound puts application
code into `step`, its well-formedness rule, which is why that rule may read
only the direct and fixed configuration.

- **Fridge bound.** Only well-formed directs enter the fridge, under an
  application-defined rule that reads only the direct and fixed configuration;
  a count bounds it; and an overflow drains just enough of its front to make
  room, raising `L` to the youngest drained direct's block
  ([register](../review/register.md#bound-the-canonical-direct-input-fridge)).
  A direct that is not well-formed is dropped without executing. An overflow
  is answered by a manual cockroach recovery: making standard recovery repair
  one is deferred on complexity, since standard recovery is the hardest part
  of this software.
- **Monotone frame clocks.** Applications may read blocks as time: a direct's
  inclusion block and a user op's frame `safe_block`. Gate d orders frames only
  within a batch, so a later batch could run below an earlier input's clock.
  The scheduler keeps `L` and rejects a batch whose first frame is below it;
  the gate reads `L`, not the app clock, which a frame that executes nothing
  leaves behind and a drain can push above `L`. Between overflows a correct
  sequencer never builds such a batch
  ([I3](../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine)),
  but the gate makes I3 consensus-critical: every transition that opens a Tip
  needs an assert that it starts at or above the latest valid frame, and
  `scheduler_accepts` must track `L` from the frames it accepts, starting after
  a rebuild from the imported `L`. It keeps no fridge, so it misses an
  overflow's raise of `L`, which rejects batches the lane already sealed: the
  divergence a manual rebuild answers.
- **Authorization decides inclusion.** A user op is included exactly when its
  sender authorized it here: its signature recovers the sender, its nonce is
  the sender's next and has a successor, and its `max_fee` covers the frame
  fee. An op that fails any of these has no effect on anyone. An included op
  always advances its sender's nonce and pays the fee, capped at the balance
  (a saturating subtraction); anything else that stops it is a business
  failure, which still advances the nonce and charges the fee. The wallet
  changes too: its apply returns an error on an unpayable fee, which would
  halt the machine under this rule. The sequencer keeps checking balances at
  admission, so its batches are unchanged.
- **Direct inputs carry their context.** An application may read a direct's
  input index, block timestamp and prevRandao, never a batch's. The fridge and
  the `Direct` the scheduler applies carry them. The sequencer's input table
  and the feed's direct rows already carry the index and timestamp, and both
  must add prevRandao, or a client could not apply such a direct.
- **A `scheduler` inspect query** returning the machine's InputBox count, the
  batch nonce, `F` and `L`. It cross-checks `sched` and the number of inputs
  the tool's fold read from L1 against the machine at the same block. One
  inspect report holds at most 2 MiB.

Shared scheduler code serves every deployment; if each sequencer release
supports one template, which the proposed template tie below would enforce,
no rule needs a switch per deployment.

## Preconditions the design must state

- **The old instance stays down.** Recovery deliberately creates a second data
  directory pinned to the same submitter address, the old one can still run
  with the same key, and
  [the flush fences transactions, not processes](../recovery/cockroach.md#incident-playbook).
  No command that signs with the key may run against the old directory again
  ([restart policy](../watchdog/operator-deployment.md#sequencer-restart-policy)).
  A sequencer address that is an L1 contract forwarding batches from a hot key
  that a cold multisig rotates (a separate design, out of scope) would fence
  at L1 instead.
- **Key loss or compromise means a new deployment** while the submitter is an
  account compiled into the template ([README](../../README.md#running)).
- **A dead canonical machine is unrecoverable.** Cockroach recovery repairs
  only the sequencer's view. PR #56 makes gate d reject an out-of-range
  `fee_price`, so on templates built from it that sequencer bug becomes a
  divergence recovery can repair; any other
  [fatal condition](../protocol/scheduler-semantics.md#fatal-conditions) still
  ends the application.
- **Consumers execute bit for bit.** The sequencer and every client run the
  segment through the canonical validation and execution; for Bart's client
  that means embedding libdex exactly as the guest runs it.
- **The template tie.** A fix to code the template shares diverges the
  sequencer from it, which today only the watchdog's next comparison notices
  ([cockroach recovery](../recovery/cockroach.md)). Setup would pin the
  application's `getTemplateHash()`, and `run` would refuse a template other
  than the one its release declares. Tying the watchdog to the validators is
  [a later to-do](2026-09-watchdog-v2.md#remaining-work).
- **Detection.** The watchdog's divergence latch tells the operator that the
  sequencer disagrees with the canonical machine, after a bug or a fridge
  overflow alike. Whether its `range` state source works with Bart's template
  is unverified ([state sources](../watchdog/README.md#state-sources)).
- **Users.** Revoked user ops are not cancelled
  ([cockroach recovery](../recovery/cockroach.md)); this is accepted, and
  expiry is future work.
- **The canonical prefix stays.** Batches the faulty sequencer got accepted
  stay canonical, and recovery does not undo them.
- **Practice.** Both of today's rebuild drills restore an honest instance's
  own archive through `setup --recovery`
  ([incident runbook](../watchdog/incident-runbook.md#drills)): a disaster,
  not a bug. Rebuilding from a machine, and realigning a client, are
  unpracticed, and a path run once in a deployment's life rots.

## Decisions

The owner settled these on 2026-10-29:

| Decision | Choice |
|---|---|
| Shape | From L1 and the scheduler library alone: a segment per consumer, from its own base, and `sched` at `C` for the sequencer only |
| Purpose | Segments that the sequencer and every client run, instead of a plugin per client |
| Sequencer's state | Projected from a machine the operator trusts, by default Dave's last finalized one; no enforced provenance |
| Bases | Each consumer has its own; the tool slices one fold per base |
| Queued directs at `C` | An input-index direct floor, replacing the terminal drain |
| Flush | Best effort, trusting the provider's pool view |
| Fencing | An operator requirement, in the threat model's [operator mistakes](../threat-model/README.md#operator-mistakes) |
| Next template | The fridge bound, monotone frame clocks, authorization deciding inclusion, direct context, and the `scheduler` query |
| Fridge overflow | A manual cockroach recovery; standard recovery does not repair it |

These stay open:

| Decision | Recommendation |
|---|---|
| Positions | InputBox counts, which every source of state names, instead of block ends, so Dave's machine needs no replay to the end of its block |
| Client's base | The newest agreement on its history, from a log in which the watchdog records each agreement with the sequencer's count `X`, era and generation |
| Client backups | Every commit writes the count and keeps its rows; a cut discards later snapshots; snapshots taken anywhere and replayed to `X` |
| Template tie | One template per release, enforced at setup and `run` |
| Shared gate function | Defer: it changes the gold frontier, the heart of standard recovery |
| `trait Scheduler` | Defer: for libdex deployments the scheduler library is ours, with libdex's direct rule ported, held to Bart's guest by the versioned gate vectors of the next template |
| Threat-model statement | Add that key loss or compromise means a new deployment until the forwarding contract |

## Questions for Bart

- Does libdex's drive hold only its flat buffer, or a padding tail too? Does
  that buffer hold the executed-input count and clock, so that a state
  projected from a machine starts at the canonical count and a comparison pins
  it?
- Which of a direct's index, timestamp and prevRandao does libdex read?
- Is your guest's compiled sequencer address the pinned submitter? Can its
  harness or scheduler answer an input with `RX_REJECTED`?
- Does your scheduler reproduce the conformance vectors, and reject an
  out-of-range `fee_price` at gate d, as ours does since PR #56?
- Which directs are well-formed for libdex, and how large can one be? The
  fridge's count bound bounds memory only if each entry's size is bounded.
- Does libdex's validation refuse a user op for any reason besides its
  signature, nonce and fee cap? Under the authorization rule, every other
  reason becomes a business failure that still advances the nonce and charges
  the fee.
- Can your client follow [the backup rules](#the-clients-base): the count in
  every commit with its rows, and snapshots discarded past a cut? How large is
  its database, which SQLite version does it link, and does applying a row
  read anything besides the database and what a segment entry carries, such
  as the row's batch nonce or a direct's transaction hash?
- What are libdex's per-op cycles and state size? Who fixes libdex during an
  incident?

## Measure before choosing

- The fold of the automaton from the deployment block: an L1 scan of every
  input, which need recover no signature before the earliest base.
- Signature recovery and execution from a realistic earliest base to `C`.
- The production deployment's Dave lag, which sets how long the default
  machine's segment is.
- The guest's cost to drain a full fridge in one input, which the root frame
  after a rebuild and the first frame after an outage both do.
- A client's replay, from its newest snapshot at or below the head's `X` to
  that `X`.
- The time to recover, end to end, in a drill.

## Remaining work

- Settle the open decisions; the template rules wait on Bart's answers.
- Tool: `segment` per base from one fold, and `sched`, from a `step` split
  out of `Scheduler<A>`; and the flush subcommand. Today's scheduler recovers
  every signature, hands an op to the application in two calls
  (`validate_user_op`, then `apply_valid_user_op`) and keeps no input index in
  the fridge, so an application that records what it is handed cannot produce
  the entries.
- Sequencer: project a machine's state, run its segment, and import with `F`
  and `L`. An existing rebuilt database takes `F` at the end of its `C`. The
  floor rewrites what pins the terminal drain today: the projection replay
  steps, the README's nonce note, the history guide's era baseline, snapshot
  lifecycle, invariants I6, I8, I11 and I20, and the rebuild drill's replica
  check.
- Clients: the backup rules, and a segment driver that runs each entry
  through the shared execution boundary and skips the user ops it rejects.
- Watchdog: parse the executed-input count, era and generation from the
  digest response, which must add the latter two, and append each agreement's
  block, InputBox count and those to a log; and `export-state`, which writes a
  machine's state-source bytes and nothing application-specific, for the
  sequencer's projection.
- Dave's machine: read the last finalized root and the sealed epoch's
  `inputIndexLowerBound` from the consensus contract, which nothing in this
  repository reads yet, and check the index against the machine's receive
  buffer.
- Drills that rebuild from a machine and realign a client, in the
  [incident runbook](../watchdog/incident-runbook.md#drills); until the tool
  lands, `setup --recovery` keeps its archive drill.
- Then the deletions the
  [tracks board](2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
  lists for the tool's landing, and `setup --recovery`, which the tool replaces.
