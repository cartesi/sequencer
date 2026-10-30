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
- **A base** is the end of an L1 block: the state after every input of that
  block and of the blocks before it, and of none after. Positions are blocks
  like this, never the app clock `last_executed_safe_block`, which every op of
  a frame shares and which can lag the raw boundary without limit
  ([history coordinates](../protocol/application-history.md#progress-ordering-and-acceptance)).
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
the application it feeds. The tool needs only the automaton:

```haskell
-- One InputBox input (an EvmAdvance). Its chain id and application address,
-- the signature domain, are deployment constants.
data RawInput = RawInput
  { index :: Index, block :: Block, timestamp :: Time, prevRandao :: Word256
  , sender :: Address, payload :: Bytes }

-- What the scheduler hands the application, in execution order.
data Direct = Direct
  { sender :: Address, index :: Index, block :: Block
  , timestamp :: Time, prevRandao :: Word256, payload :: Bytes }
data L2Tx
  = UserOp { sender :: Address, nonce :: Nonce, maxFee :: Fee, fee :: Fee
           , payload :: Bytes, safeBlock :: Block }
  | DirectTx Direct

-- The sequencing automaton: what the scheduler keeps besides the application.
data Sched = Sched
  { batchNonce :: Nonce          -- next batch nonce
  , fridge     :: Queue Direct   -- admitted directs not yet executed
  , floor      :: Block          -- L
  }

step :: Sched -> RawInput -> (Sched, [L2Tx])

-- The stream, grouped by the input whose processing emitted each entry.
segment :: Block -> Block -> ([(Block, [L2Tx])], Sched)
segment a b = (zip (map block ins) txs, sched_b)
  where sched_a = foldl (\s i -> fst (step s i)) initial (inputsThrough a)
        ins = inputsIn a b
        (sched_b, txs) = mapAccumL step sched_a ins
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
the application that receives the stream. The canonical machine is `step`
followed by the guest's application, and the sequencer and every client are
other applications over the same stream. That holds while the guest never
answers an input with `RX_REJECTED`, as the
[fatal conditions](../protocol/scheduler-semantics.md#fatal-conditions)
require: the host's revert would roll the batch nonce and the fridge back with
the application, a [question for Bart](#questions-for-bart).

So `segment a b` needs an L1 connection and the scheduler library for the
deployment's template, nothing else: no machine, no nonces, no application
state and no application code. It reads every input since the deployment
block, because the batch nonce and the fridge at `a` depend on all of them;
before `a` the fold need only decode batches and check gates, recovering no
signature. Its output is the L2 stream of the inputs in `(a, b]`, grouped by
the block of the input whose processing emitted each entry (for a drained
direct, the input that drained it, not its own block), and `Sched` at the end
of `b`, which holds `N'`, the fridge (so `F`) and `L`.

The stream is fed to two states at base `a`:

```haskell
sequencer = run (project m_a) txs   -- m_a: a machine at base a
client    = run backup_a txs        -- a backup at base a
-- run executes each tx through the shared execution boundary, as the
-- canonical application does: a user op its state does not include is skipped
```

A candidate stream is exact only for a consumer that sits exactly at its
start, and both do: each holds the nonces (and, on today's template, the
balances) that decide inclusion, as the canonical machine did at `a`. Neither
needs an InputBox index or an executed count to find its start: the base
block fixes it. Each computes its own offsets while it runs, and the two must
reach the same count, a cheap cross-check. A consumer at a later base `a'`
skips the groups whose block is at most `a'`, by that tag, never by a direct's
`block` or an op's `safeBlock`. The sequencer then imports
its state with `N'`, `F` and `L`, and its root frame drains the queued directs
([below](#queued-directs-at-the-stop-block)); a client subscribes to the new
era at the sequencer's count and receives those directs as the era's first
feed rows.

### Why the tool hands out a segment

The alternative is a `RichClientApp` plugin that the tool runs to produce each
client's end state. The segment is better for four reasons:

- **Data crosses the boundary, not code.** The tool depends only on the
  scheduler, which a template fixes; a client's projection, schema and storage
  change on their own schedule. A plugin would tie every tool release to every
  client's code and effects.
- **One execution boundary.** A client already runs L2 transactions through
  the shared execution boundary, and recovery reuses it; only the driver
  differs, since segment entries carry no offsets and a user op the client's
  state rejects is skipped. A plugin would be a second path that runs once in
  a deployment's life and rots.
- **History, not only state.** Bart's client keeps trades and transfers, and
  running the segment from its base replays that history, in canonical
  order.
- **Checkable and free of interpretation.** Every consumer is a fold over the
  segment, the sequencer included, and their counts must agree; a plugin's end
  state could only be trusted.

## States at a base

Both states must sit exactly at base `a`: a state that has consumed half of
block `a`, or one input past it, would drop or repeat inputs. This is where
the whole-block property becomes load-bearing.

The sequencer's state is projected from a machine at its base. The
projection must carry the executed-input count and clock that the consumers'
offsets start from: the wallet's `state` inspect report holds both, and libdex
keeps its state as a flat buffer in one of the machine's drives, which may or
may not hold them
([application contract](../protocol/application-contract.md#7-canonical-recovery-integration)).
Any canonical machine the operator trusts will do, even one past a
divergence; the tool enforces no provenance.

| Machine | Base | Trust |
|---|---|---|
| Dave's last finalized machine, the default | its epoch can end inside a block: replay the rest of that block from its input index, read on chain with its root and checked against its receive buffer | its root is on chain |
| A watchdog checkpoint | the end of a block (`<block>-<count>`) by construction | the operator's watchdog replayed it from L1 |
| The template | before the first input | its hash is on chain |

A matching root authenticates a machine only from a source we trust, such as
our own Dave node: a stored machine's root comes from its stored hash tree, not
from its contents ([hashing](../cartesi-machine.md#hashing)). An upcoming
emulator release brings a mechanism to verify an untrusted load; until then,
`verify_hash_tree` is not that mechanism and is
[on the watchdog's to-do list](2026-09-watchdog-v2.md#remaining-work).

The client's state comes from its backups. A backup sits at base `B` only if
it holds exactly the entries below the executed-input count `X` of an accepted
checkpoint at block `B`, which is a whole block by construction
([snapshot lifecycle](../snapshots/lifecycle.md#acceptance-and-comparison)),
and it is labelled with `B` and `X`. A client that follows the optimistic feed
is already past `X` when it learns of the acceptance, so it snapshots at batch
boundaries and keeps the snapshot whose count a checkpoint later accepts. A
later snapshot labelled `B` would apply its soft entries again; worse, a soft
op that never landed has already used its sender's nonce, so the client skips
the canonical op with that nonce, and the counts still agree. After an
incident the base must predate the divergence, at or before the watchdog's
last agreed block: a client that followed the divergent sequencer holds no
canonical backup past it.

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

1. **Choose the bases.** Take a machine the operator trusts at the end of a
   block; any canonical machine will do, even one past the divergence. Take
   each client's newest backup at a base; after an incident it must be at or
   before the agreed head in the watchdog's latch record. `a` is the earliest
   of these bases, and each consumer skips the groups tagged with blocks up to
   its own.
2. **Flush, then fix `C`.** Settle the old instance's transactions, then take
   a safe head observed after they settle as `C`. The flush is best effort: it
   replaces what the provider's pool holds for the submitter account and waits
   until that is safe. It cannot rely on the old watermark, which does not
   survive every event that calls for a rebuild
   ([flush boundary](../recovery/cockroach.md#flush-and-stopping-block)). As a
   standalone subcommand, it is the only step that needs the key. Refuse if a
   submitter input above `C` is already ingested.
3. **Run `segment a C`**, from L1 alone, and write the stream and `Sched` at
   `C`, the latter last as the commit point.
4. **Rebuild the sequencer.** Run the stream from its state projected from the
   machine, skipping the groups up to the machine's base, and import the result
   with `N'`, `F` and `L` in one transaction. Start `run` promptly. A late
   start is safe: once the root frame ages past the danger threshold, startup's
   [Tip repair](../recovery/README.md#open-tip-repair-without-flushing)
   reopens it at the current safe head, still draining from `F`.
5. **Realign clients.** Each client runs the stream from its backup, skipping
   the groups up to its backup's base, checks that it reaches the
   sequencer's count, and subscribes to the new era there.

The new era's first watchdog comparison checks the result a batch later: a
wrong stream, `N'` or `F`, or a guest compiled for another sequencer address,
shows up there as a divergence. Replaying the machine to `C` with the
watchdog's engine checks it before `run`, at an emulated cost. The tool cannot
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
- **A `scheduler` inspect query** returning the machine's input count, the
  batch nonce, `F` and `L`. It cross-checks `segment`'s fold from L1 against
  the machine. One inspect report holds at most 2 MiB.

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
  stream through the canonical validation and execution; for Bart's client
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
| Shape | `segment a b`, from L1 and the scheduler library alone, fed to the sequencer's and the client's states at base `a` |
| Purpose | A stream that the sequencer and every client run, instead of a plugin per client |
| Sequencer state at `a` | Projected from a machine the operator trusts, by default Dave's last finalized one; no enforced provenance |
| Queued directs at `C` | An input-index direct floor, replacing the terminal drain |
| Flush | Best effort, trusting the provider's pool view |
| Fencing | An operator requirement, in the threat model's [operator mistakes](../threat-model/README.md#operator-mistakes) |
| Next template | The fridge bound, monotone frame clocks, authorization deciding inclusion, direct context, and the `scheduler` query |
| Fridge overflow | A manual cockroach recovery; standard recovery does not repair it |

These stay open:

| Decision | Recommendation |
|---|---|
| Client backups | Taken at exactly an accepted checkpoint's executed-input count, and labelled with its block and that count |
| Template tie | One template per release, enforced at setup and `run` |
| Shared gate function | Defer: it changes the gold frontier, the heart of standard recovery |
| `trait Scheduler` | Defer: for libdex deployments the scheduler library is ours, with libdex's direct rule ported, held to Bart's guest by the versioned gate vectors of the next template |
| Threat-model statement | Add that key loss or compromise means a new deployment until the forwarding contract |

## Questions for Bart

- Does libdex's drive hold only its flat buffer, or a padding tail too? Does
  that buffer hold the executed-input count and clock, so that a state
  projected from a machine starts at the canonical count?
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
- Will your client keep backups at exactly an accepted checkpoint's
  executed-input count, labelled with its block and count, and how long will
  it keep them?
- What are libdex's per-op cycles and state size? Who fixes libdex during an
  incident?

## Measure before choosing

- The fold of the automaton from the deployment block: an L1 scan of every
  input, which need recover no signature before `a`.
- Signature recovery and execution over a realistic `(a, C]`.
- The production deployment's Dave lag, and replaying the rest of a block from
  Dave's machine.
- The guest's cost to drain a full fridge in one input, which the root frame
  after a rebuild and the first frame after an outage both do.
- The time to recover, end to end, in a drill.

## Remaining work

- Settle the open decisions; the template rules wait on Bart's answers.
- Tool: `segment`, from a `step` split out of `Scheduler<A>`, and the flush
  subcommand. Today's scheduler recovers every signature, hands an op to the
  application in two calls (`validate_user_op`, then `apply_valid_user_op`)
  and keeps no input index in the fridge, so an application that records what
  it is handed cannot produce the stream.
- Sequencer: project a machine's state, run the stream, and import with `F`
  and `L`. An existing rebuilt database takes `F` at the end of its `C`. The
  floor rewrites what pins the terminal drain today: the projection replay
  steps, the README's nonce note, the history guide's era baseline, snapshot
  lifecycle, invariants I6, I8, I11 and I20, and the rebuild drill's replica
  check.
- Clients: backups at an accepted checkpoint's count, labelled with its block
  and count, and a segment driver that runs each entry through the shared
  execution boundary and skips the user ops it rejects.
- Watchdog: `export-state`, and `replay` from an input index, checked against
  the machine's receive buffer, to the end of its block, for Dave's machine.
- Drills that rebuild from a machine and realign a client, in the
  [incident runbook](../watchdog/incident-runbook.md#drills); until the tool
  lands, `setup --recovery` keeps its archive drill.
- Then the deletions the
  [tracks board](2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
  lists for the tool's landing, and `setup --recovery`, which the tool replaces.
