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
triggers it by hand, keeps that instance down and supplies a machine it
trusts. The design takes the worst case: the submitter key is ours, but the
instance's database, snapshots and receipts prove nothing. The canonical
scheduler is the authority, bug for bug. The rebuilt sequencer is assumed
correct; if it is not, fix it and rebuild again. Recovery must not depend on
replaying the application from genesis. It runs rarely, so each of its steps
must be simple enough to explain, practice and trust; anything a first version
can do without waits for a need.

Here an *image* is the canonical machine's template: the initial machine,
guest program included, whose root hash the on-chain application pins as
`getTemplateHash()`. A *machine* is a stored state of it, such as a validator's
at the end of an epoch. Images can change, but only through a new deployment:
a deployed application keeps its image for life
([Cartesi Machine facts](../cartesi-machine.md#versions-are-all-or-nothing)).

As in the [data dictionary](../recovery/cockroach.md#data-dictionary), `C` is
the stop block, `N'` the next batch nonce at its end and `K` the
executed-input count of the rebuilt baseline, where clients subscribe. Here the
baseline is the canonical state at the end of `C`, before its queued directs
run ([below](#queued-directs-at-the-stop-block)), so `K` is today's less the
queue's length.

## The model

Three kinds of state fold two streams:

| State | Folds | Holds |
|---|---|---|
| `M`, the canonical machine | raw InputBox inputs | the application, the fridge and the next batch nonce |
| `S`, the sequencer's application | executed L2 transactions | the application only |
| `Cl`, a client's state | executed L2 transactions | its own projection, presumably with an `S` inside |

The scheduler maps the raw stream to the executed L2 stream, and it has two
parts with different inputs:

- The **sequencing automaton** (next batch nonce and fridge) runs
  [gates a–e](../protocol/scheduler-semantics.md#processing-an-input), the
  frame drains, the backstop and, once the fridge is bounded, its overflow
  drain. It reads no application state, so its state at any raw position is a
  function of the input prefix alone. No input is ever rolled back, as long as
  the guest halts on an application failure and never answers an input with
  `RX_REJECTED`, as the
  [fatal conditions](../protocol/scheduler-semantics.md#fatal-conditions)
  require of every port. Our guest does; Bart confirms libdex never reverts,
  and whether the rest of his guest can is a
  [question for him](#questions-for-bart). A halt still depends on application
  state, so anything derived without the application holds only while the
  machine is live.
- **Inclusion** decides which user ops execute. It reads application state:
  the sender's nonce and fee balance.

Two consequences shape the design:

- The nonce and the fridge never need to be trusted or exported. Running the
  scheduler itself over the input prefix derives both exactly, every gate and
  the fridge bound included, with an application that executes nothing: it
  only advances its progress once per executed input, as the scheduler
  asserts, and, once the fridge is bounded, keeps the rule for which directs
  are well-formed. That rule must be a pure function of the direct, or the
  fridge would depend on application state. `scheduler_accepts` cannot do
  this, because it omits gate d's rejections and keeps no fridge.
- `S` and `Cl` need not share a type. They need a shared *position* and the
  canonical executed segment between positions: the shared object is the
  history, not a state. Snapshot-then-subscribe synchronizes states, and a
  client whose state includes history needs history.

### Positions are indices and counts, never clocks

| Coordinate | Stream | Meaning |
|---|---|---|
| InputBox input index | raw | The only exact machine position; a Dave epoch boundary is one and can fall inside a block. |
| End of block `X` | raw | All inputs whose inclusion block is at most `X`: the only block-shaped position. |
| Executed-input count | L2 | A position within one history; counts derived from `M` are canonical. |
| App clock `A` | neither | The running maximum of executed clocks; it locates nothing. |

The app clock (`last_executed_safe_block`) is a clock, not a position
([history coordinates](../protocol/application-history.md#progress-ordering-and-acceptance)):
every op of a frame shares it, queued directs sit at or above it, and it can
lag the raw boundary without limit. Its only positional content concerns
directs in block `A`, which is how
[checkpoint eligibility](../recovery/cockroach.md#checkpoint-eligibility) fails.

"Finalized" has three meanings here: the Ethereum tag (the sequencer reads
`safe`), a Dave-settled epoch, and the sequencer's `/finalized_*` routes, which
serve its latest safe accepted checkpoint. Recovery needs only safe blocks to
be permanent ([threat model](../threat-model/README.md#l1-chain-semantics)).

## Resuming a machine

A rebuild resumes the canonical program from a machine and replays the raw
inputs after it. Two rules fix where it starts and where it stops:

1. **Resume at the machine's input index.** A cursor at the app clock re-feeds
   inputs the machine already consumed: queued directs are enqueued again and
   execute twice, and a batch first rejected for a future nonce can now match
   and execute, shifting `N'`. A block cursor drops or repeats the inputs of a
   split block. Take the index from the machine's source, such as the chain
   for Dave's machine or the name of a watchdog checkpoint; the input left in
   the machine's receive buffer can only cross-check it
   ([Cartesi Machine facts](../cartesi-machine.md#rollup-host-semantics)).
2. **Fix the stop block `C` after the flush.** `C` is a safe head observed
   after the flush settles the old instance's transactions; a stop block fixed
   earlier lets an old batch at the next nonce land after it.

## The input machine

The tool starts from a stored machine the operator trusts, at a known input
index. It enforces no provenance: choosing the machine is the operator's
responsibility, which also lets tests start anywhere.

| Machine | Position | Trust | Replay to `C` |
|---|---|---|---|
| Dave's last finalized machine, the default | `inputIndexLowerBound` of the current sealed epoch, possibly mid-block | its root is on chain | a week or more on mainnet |
| A watchdog checkpoint | the end of a block (`<block>-<count>`), possibly mid-epoch | the operator's watchdog replayed it from L1 | at most the incident window |
| The template | input index 0 | its hash is on chain | from genesis |

The default gives the operator and every client one machine, which each can
take from its own Dave node and check against L1. Dave's node, sling, keeps
only the newest finalized machine and serves it only as files
([Cartesi Machine facts](../cartesi-machine.md#rollup-host-semantics)). Read its
position at the same finalized block as the root, or take both from the newest
`EpochSealed`. That position can fall inside a block, so the replay starts at
an input index.

A matching root authenticates a machine only from a source we trust, such as
our own Dave node: a stored machine's root comes from its stored hash tree, not
from its contents ([hashing](../cartesi-machine.md#hashing)). v0.22.0's one
check of the contents, `verify_hash_tree`, catches a corrupt copy but not hash
files rewritten to match, and the emulator team asked that nothing call it;
PR #56 runs it in the watchdog's `init` and `replay` anyway, which is
[on the watchdog's to-do list](2026-09-watchdog-v2.md#remaining-work). An
upcoming emulator release brings a mechanism to verify an untrusted load,
which would let a machine from any source be checked against L1.

The import takes a native bundle at `C` carrying `K`, `N'` and `F`, the
InputBox index where the canonical fridge starts
([below](#queued-directs-at-the-stop-block)), whether the tool exported it
from a machine or a test built it. The sequencer's logical state is never
richer than the canonical one
([application contract](../protocol/application-contract.md#7-canonical-recovery-integration)),
so a machine can always produce the bundle. A sequencer archive is not such a
bundle: it stops at its own accepted block, below `C`, and nothing here folds
it forward. A rebuild after a lost database therefore starts from a machine
too, most cheaply the watchdog head.

## Queued directs at the stop block

At the end of `C` the canonical scheduler can still hold queued directs. The
first accepted frame at or above a direct's block drains it, or the backstop
does `MAX_WAIT_BLOCKS` later. The rebuilt sequencer must sequence each of them
exactly once. The fridge is always a suffix of the directs it admits (every
direct today, the well-formed ones once the fridge is bounded), since frame
drains, the backstop and the bound's overflow drain all pop only its front, so
one InputBox index `F` describes it: the oldest queued direct's, or the end of
`C` when nothing is queued. A block cannot, because after faulty sequencing a
frame whose `safe_block` equals its inclusion block drains only part of that
block.

The rebuilt sequencer therefore starts from the canonical state at the end of
`C`, with its direct floor at `F` instead of at block `C`. Its root frame, at
`C`, executes the admitted directs in `[F, end of C)` as ordinary rows before
any user op, as the first canonical frame drains them, and clients receive
them through the feed with normal offsets. The sequencer keeps no fridge of
its own: its pending directs are the admitted directs in the undrained index
range of its input table, and setup
already ingests every input from the deployment block, so `F` alone carries
the canonical fridge across the rebuild. The bundle also carries the queue's
length, which the import checks against its own count of admitted directs
from `F`. The change is small:
the era records `F`, and the lane drains directs from the later of `F` and the
first input after its latest frame's `safe_block`. A cascade that tears down
every frame
([I16](../invariants.md#i16-the-batch-tree-has-exactly-one-valid-parentless-root-carrying-the-deployments-anchor-nonce))
re-roots there and drains the queued directs again. Acceptance still scans
only blocks after `C`
([replay boundaries](../recovery/cockroach.md#replay-boundaries)).

Today's procedure instead executes the queued directs during recovery (the
terminal drain) and publishes the result, `S'`. `S'` is a prediction, not a
machine state: the canonical machine reaches it only inside the post-`C` input
that empties the fridge, often executing more there, so no comparison can
check it. A client that applied the drained tail must roll it back if a second
rebuild comes, and a rebuild that started from `S'` would execute those
directs twice. With the floor, the baseline is a state the canonical machine
actually reaches, and the root frame's drain is the ordinary drain
attribution
([I2](../invariants.md#i2-drain-attribution-accumulated-directs-land-in-the-clock-advanced-frame)),
an argument normal operation already relies on and an easier one to explain.
Track 3 kept the terminal drain because replacing it would carry pending work
across the baseline
([scope boundaries](2026-07-track3-feed-replay-design.md#scope-boundaries));
that work crosses as one index.

## The tool

The canonical machine produces the baseline: it is the state that machine
reaches at the end of `C`, bug for bug.

1. **Check the machine.** Compare its root with the chain's where it claims an
   on-chain one, and its position with the input in its receive buffer.
2. **Flush, then fix `C`.** Settle the old instance's transactions, then take
   a safe head observed after they settle as `C`. The flush is best effort: it
   replaces what the provider's pool holds for the submitter account and waits
   until that is safe. It cannot rely on the old watermark, which does not
   survive every event that calls for a rebuild, so a transaction alive only
   outside the provider's pool may still land and freeze the rebuilt
   instance, which then needs another rebuild. This is the only step that
   needs the key, and only `C` must wait for it, so most of step 3 can run
   first. Refuse if a submitter input above `C` is already ingested.
3. **Replay to `C`.** Run the machine to the end of `C` with the watchdog's
   engine, starting at its input index. A fixed point means the application
   is dead: stop.
4. **Export.** Write the machine's state-source bytes at `C` as a native
   bundle with its count `K`. That is a byte copy where the dump is exactly
   the comparison file, as for the wallet; any other application keeps its own
   exporter
   ([application contract](../protocol/application-contract.md#7-canonical-recovery-integration)).
   Take `N'` and `F` from the scheduler run over the input prefix with an
   application that executes nothing, and write the bundle's metadata last,
   as its commit point.
5. **Import and run.** Import in one transaction, with the root frame's
   leading direct range `[F, end of C)`, then start `run` promptly. A late
   start is safe: once the root frame ages past the danger threshold,
   startup's [Tip repair](../recovery/README.md#open-tip-repair-without-flushing)
   reopens it at the current safe head, still draining from `F`.

The new era's first watchdog comparison then checks the result a batch later:
a wrong `N'`, a wrong `F` or a guest compiled for another sequencer address
shows up there as a divergence.

The watchdog needs two changes for this: an `export-state` command that writes
a machine's state-source bytes with its block and input count, and nothing
application-specific; and `replay` that starts at an input index, checked
against the input in the machine's receive buffer, where today it takes a
block and the count from the RPC.

## Later, when a need appears

### Clients

Bart's client treats rebuilds as out of scope today, so the client side waits
until it does not. Then the tool also produces the canonical executed segment
from the input machine's count to `K`, and a client applies it from its own
checkpoint:

1. Restore the newest backup at a count within the segment whose logged entry
   hashes match it and whose embedded `S` digest equals the tool's at that
   count. With no such backup, rerun from an older machine, or from the
   template (a replay from genesis).
2. Apply the rest of the segment through the same path as feed entries.
3. Require count `K` and the baseline's digest, then subscribe to the new era
   at `K`; its first rows are the queued directs. Never subscribe across an
   era.

Only a native fold can produce that segment: the canonical machine tags no
output with its entry and exposes state only between raw inputs, while one
batch executes many entries. The fold starts from the input machine's state
and must reach an equal state at `C`. An entry's hash covers its L1-derived
fields and its offset, never its outcome or outputs, and one entry format
should serve both the segment and the new sync's feed. When replaying a week
from Dave's machine is too slow, the same fold also gives a fast path: replay
canonically from a recent watchdog checkpoint instead, and let the fold from
Dave's machine produce only the segment.

A client that does not trust the operator starts from a machine from its own
node, or, once the emulator verifies untrusted loads, from anyone's. For
realignment after an incident, the machine must sit at or before a backup the
client can restore, and so before the divergence. The watchdog's head stays at
its last agreed block while the latch holds, so it serves such a client;
`incident/canonical`, the canonical machine at the latched block, does not
([incident runbook](../watchdog/incident-runbook.md#what-you-have)).

State equality does not validate history. The wallet reaches byte-identical
state through distinct histories of equal length: two commuting transfers, or
two included business failures from different senders, each pair in either
order. Entry hashes validate a backup's history within the segment; history
before it rests on state equality alone.

### The scheduler boundary

A `trait Scheduler`, like `trait Application`, would let tools run Bart's
scheduler natively. Taken at the machine's own interface,
`M -> RawInput -> M`, it keeps a tool bug for bug with the machine: the
harness's input decoding (u64 narrowing, the per-input signing domain, the
compiled sequencer address), the scheduler and the engine. For libdex the
shape is one C++ library built for RISC-V and native. The first version needs
none of it: the canonical machine produces the baseline, and for our guest the
scheduler that derives `N'` and `F` is the guest's own `Scheduler<A>`. Build
the trait with Bart's library, when a native fold over libdex is needed.

Sharing one gate function between the guest and the gold frontier would
remove the self-trust exception for gate d's rejections, which PR #56's
fee-range reject extends. It is weakest after a rebuild, since a malformed
old-key batch that escapes the flush freezes a correct instance. But the gold
frontier is the heart of standard recovery, the hardest part of this
software, so it waits for a need.

## Canonical rules for the next image

Each rule changes the image, so it needs Bart's agreement and can ride only a
new image, ours and his port's
([register](../review/register.md#canonical-rules-open-for-the-c-port)). Only
the fridge bound is settled; the others are proposals, open or deferred as the
[decisions](#decisions) record. If
each sequencer release supports one image, which the template tie below would
enforce, no rule needs a switch per deployment.

- **The fridge bound**, settled with the owner: only well-formed directs enter
  the fridge, a count bounds it, and an overflow drains just enough of its
  front to make room, so the input that overflows it carries no large drain
  ([register](../review/register.md#bound-the-canonical-direct-input-fridge)).
  The bound is protocol: the scheduler run that derives `N'` and `F` applies
  it like every other rule. The playbook for an overflow is a manual cockroach
  recovery.
- **Monotone frame clocks**, the owner's proposal. Gate d orders frames only
  within a batch, so a later batch's user ops can run below an earlier
  input's clock. The scheduler would keep the last accepted frame's
  `safe_block`, `L`, and reject a batch whose first frame is below it; the
  gate must read `L`, not the app clock, which a frame that executes nothing
  leaves behind and a drain can push above `L`.
  A correct sequencer never builds such a batch
  ([I3](../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine)).
  It changes no recovery reasoning, though: its value is time the
  application can trust. Its cost is `L` in the scheduler state, the
  bundle, the era, the gold frontier, which must track it exactly, and an
  assert on every transition that opens a Tip. An overflow drain can still
  run a young direct before a later batch whose frames sit below it, so
  clocks are monotone only between overflows; making the drain raise `L`
  would let standard recovery repair an isolated overflow, and is deferred on
  complexity.
- **Under-funded user ops execute**, the owner's sanity rule. An op its sender
  cannot fully pay for executes, and the sequencer that batched it bears the
  fee it cannot collect; whether the sender then pays nothing or what its
  balance covers is still open. The wallet changes too: its apply returns an
  error on an unpayable fee, which validation keeps unreachable today and
  which would halt the machine under this rule. Inclusion then reads only the
  sender's nonce, which the
  [nonce rule](../protocol/application-contract.md#user-nonces) makes a
  function of earlier inclusions, so the executed stream becomes a function of
  L1 while the machine is live. Recovery does not need this; making a
  mismatched nonce an included no-op would go further, but nothing needs that
  either. The sequencer keeps its own admission check, so its batches are
  unchanged.
- **A `scheduler` inspect query** returning the machine's input count, the
  nonce and the fridge entries with their input indices. Read at `C`, it would
  give `N'` and `F` from the canonical machine itself, replacing the scheduler
  run from genesis and, on Bart's image, the reliance on our scheduler
  matching his. It changes the image, and images already deployed still need
  the run, so it waits until the run proves too slow or Bart's image needs
  it.

## Preconditions the design must state

- **The old instance stays down.** Recovery is manual, and the operator keeps
  the old instance stopped: recovery deliberately creates a second data
  directory pinned to the same submitter address, the old one can still run
  with the same key, and
  [the flush fences transactions, not processes](../recovery/cockroach.md#incident-playbook).
  No command that signs with the key may run against the old directory again
  ([restart policy](../watchdog/operator-deployment.md#sequencer-restart-policy)).
  Otherwise both instances post at nonce `N'` and later, either or both can
  freeze, and the old instance's sealed batches can still land. A sequencer
  address that is an L1 contract forwarding batches from a hot key, which a
  cold multisig can rotate (a separate design, out of scope), would fence at
  L1 instead: rotating the key retires the old instance.
- **Key loss or compromise means a new deployment** while the submitter is an
  account compiled into the image ([README](../../README.md#running)); the
  forwarding contract would make it a rotation.
- **A dead canonical machine is unrecoverable.** Cockroach recovery repairs
  only the sequencer's view. PR #56 makes gate d reject an out-of-range
  `fee_price`, so on images built from it that sequencer bug becomes a
  divergence recovery can repair. Images built before it halt on such a batch
  only when one of its ops reaches the wallet's fee conversion, which takes a
  recovering sender, a `max_fee` covering the fee and the sender's next nonce;
  otherwise they skip those ops. Any other
  [fatal condition](../protocol/scheduler-semantics.md#fatal-conditions) still
  ends the application.
- **The watchdog ties the sequencer to the deployed image.** A fix to code the
  image shares diverges the sequencer from it, which today only the watchdog's
  next comparison notices ([cockroach recovery](../recovery/cockroach.md)). A
  cheap earlier tie: setup would pin the application's `getTemplateHash()`, and
  `run` would refuse an image other than the one its release declares, which
  also makes one image per release safe. Tying the watchdog to the validators is
  [a later to-do](2026-09-watchdog-v2.md#remaining-work).
- **Foreclosure** blocks `InputBox.addInput` in the pinned contracts, and
  PR #56 documents stopping the sequencer for good; do not rebuild a
  foreclosed application.
- **Detection.** The watchdog's divergence latch is how the operator learns
  that the sequencer disagrees with the canonical machine, after a bug or a
  fridge overflow alike, and whether its `range` state source works with
  Bart's image is unverified
  ([state sources](../watchdog/README.md#state-sources)).
- **Users.** Revoked user ops are not cancelled
  ([cockroach recovery](../recovery/cockroach.md)); this is accepted, and
  expiry is future work. `GET /nonce` reads 0 for senders idle since the
  rebuild ([user nonces](../protocol/application-contract.md#user-nonces)), as
  after today's rebuild; the baseline nonces that
  [Track 6](2026-07-coordination-tracks.md#track-6--dump--application-api-redesign)
  proposes would make it exact, and the first version does without them.
- **Disaster recovery** (a lost database, no bug) is probably the commonest
  trigger. Every client then sees an era change, and the rebuild starts from a
  machine like any other, most cheaply the watchdog head.
- **The canonical prefix stays.** Batches the faulty sequencer got accepted
  stay canonical, and recovery does not undo them.
- **Practice.** The rebuild has no formal model, and both its drills restore
  an honest instance's own archive through `setup --recovery`, which this
  design replaces, after wiping its database
  ([incident runbook](../watchdog/incident-runbook.md#drills)): a disaster,
  not a bug. Starting from a machine is unpracticed, and a path run once in a
  deployment's life rots.

## Decisions

The owner settled these on 2026-10-29:

| Decision | Choice |
|---|---|
| Input | A machine the operator trusts, by default Dave's last finalized one checked against L1; no enforced provenance |
| Queued directs at `C` | An input-index direct floor, replacing the terminal drain |
| Flush | Best effort, trusting the provider's pool view, since the old watermark cannot be relied on |
| Fencing | An operator requirement, now in the threat model's [operator mistakes](../threat-model/README.md#operator-mistakes); a rotatable forwarding contract would fence at L1 |
| Fridge bound | Well-formed directs only, bounded by count, an overflow draining just enough to make room; the bound is protocol |
| Fridge overflow | A manual cockroach recovery; making the drain raise `L` is deferred on complexity |

These stay open:

| Decision | Recommendation |
|---|---|
| First version | The sequencer's baseline only: check, flush, replay, export, import; the client segment and its native fold wait for a client that handles rebuilds |
| Scheduler state | The scheduler run over the input prefix with an application that executes nothing, replacing the checkpoint's operator-trusted nonce and today's fridge seeding from `(A, B]` ([replay boundaries](../recovery/cockroach.md#replay-boundaries)) |
| Flush tool | A standalone subcommand for the best-effort flush, so that the rest of the tool needs no key |
| Template tie | One image per release: setup pins `getTemplateHash()`, and `run` refuses another |
| Frame clocks | Defer unless libdex reads a frame's `safe_block` as time |
| Under-funded ops | The owner's rule if wanted as application sanity; recovery does not need it |
| Shared gate function | Defer: it changes the gold frontier, the heart of standard recovery |
| `scheduler` query and `trait Scheduler` | Defer until a need appears |
| Threat-model statements | Add that key loss or compromise means a new deployment until the forwarding contract; the old-instance requirement and the Ethereum L1 scope are already there |

## Questions for Bart

- Is libdex's dump exactly its canonical range, so that a machine's state
  loads through `from_dump`? Do the comparison bytes embed the executed count?
- Does libdex read the block timestamp, prevRandao or input index when it
  executes a direct input? Does it read a frame's `safe_block` as time?
- Is your guest's compiled sequencer address the pinned submitter? Can its
  harness or scheduler answer an input with `RX_REJECTED`?
- Does your scheduler pass `consensus_vectors_v1`, and does it reject an
  out-of-range `fee_price` at gate d, as ours does since PR #56?
- Which directs are well-formed for libdex, and how large can a well-formed
  one be? The fridge's count bound bounds memory only if each entry's size is
  bounded too.
- Does libdex reject a user op for reasons beyond its nonce and fee balance,
  such as the C ABI's account binding?
- When will your client handle a rebuild instead of treating it as out of
  scope? It then needs the segment, and an entry hash per applied entry.
- What are libdex's per-op guest cycles and state size? Who fixes libdex
  during an incident?

## Measure before choosing

- The emulated replay time of an incident window and of a week, with guest
  cycles per wallet and per libdex batch (today one record exists: about
  50 ms of host overhead per input on v0.21, in
  [Cartesi Machine facts](../cartesi-machine.md#storage-sharing-and-snapshots)).
  If a week is too slow, the default input costs too much.
- The production deployment's Dave lag.
- A scheduler run from genesis with an application that executes nothing,
  which still recovers one signature per op, and setup's sync time from the
  deployment block.
- The guest's cost to drain a full fridge in one input, which the root frame
  after a rebuild and the first frame after an outage both do.
- The time to recover, end to end, in a rebuild drill.

## Remaining work

- Settle the open decisions; those that change the image wait on Bart's
  answers.
- Watchdog: `export-state`, and `replay` from an input index.
- Tool: the best-effort flush, the scheduler run for `N'` and `F`, the bundle,
  and the import with `F`. An existing rebuilt database takes `F` at the end
  of its `C`. The floor rewrites what pins the terminal drain today: the
  projection replay steps, the README's nonce note, the history guide's era
  baseline, snapshot lifecycle, invariants I6, I8, I11 and I20, and the
  rebuild drill's replica check.
- A drill that starts from a machine, in the
  [incident runbook](../watchdog/incident-runbook.md#drills); until the tool
  lands, `setup --recovery` keeps its archive drill.
- Then the deletions the
  [tracks board](2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
  lists for the tool's landing, and `setup --recovery`, which the tool and the
  import replace.
