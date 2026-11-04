# Shared recovery tool

**Status:** proposed; nothing here is implemented. It started in the
2026-10-28 design session. On 2026-11-02 the owner settled the tool's
two-phase replay and the template rules it relies on;
[Decisions](#decisions) lists what stays open. Two simpler plans followed: a
[candidate](2026-11-recovery-from-a-machine.md), then
[sequencer recovery](2026-11-sequencer-recovery/README.md), which would replace
both. The design needs the
[next template's rules](#rules-for-the-next-template), so it serves only
deployments built from that template; [cockroach recovery](../recovery/cockroach.md)
and `setup --recovery` stay the procedure for today's. When this design lands,
its durable parts move there and this file keeps only remaining work. Its
claims were checked by reading the code at `3e92563`,
[PR #56](https://github.com/cartesi/sequencer/pull/56) at `ee5c72c`, Dave at
`dbcca7d4` and the emulator at v0.22.0-test3 (`7adfd492`, whose header and CLI
match v0.22.0), and by adversarial review; none was executed.

Cockroach recovery rebuilds a sequencer that is ready for new user ops after an
instance loses its database or its behavior cannot be trusted. The operator
triggers it by hand and keeps that instance down. The design takes the worst
case: the submitter key is ours, but the instance's database, snapshots and
receipts prove nothing. The canonical scheduler is the authority, bug for bug.
The rebuilt sequencer is assumed correct; if it is not, fix it and rebuild
again. Nothing has to be replayed from genesis: the tool starts from a
canonical machine and reads only the L1 blocks after that machine's clock.

What makes the tool *shared* is a client: Bart's DEX backend keeps a richer
state than the sequencer and must realign after a rebuild, from its own backup
and with its own code. Bart operates both; only their development is split.
Recovery runs rarely, so each step must be simple enough to explain, practice
and trust.

## The idea

Every executed L2 transaction carries a block: a direct its inclusion block, a
user op its frame's `safe_block`. Let `T` be the canonical sequence of executed
L2 transactions, in execution order. Each application state carries its
progress: its **effective count** `X`, how many transactions it has executed,
and its **effective block** `E`, the block of the latest one.

> A state on the canonical history with effective count `X` has executed
> exactly `T[0..X)`. Nothing but executed transactions changes an application,
> so the state is missing exactly `T[X..K)`, where `K` is the count at the stop
> block `C`.

Recovery hands each consumer its missing part, starting from its **base**: the
canonical state it starts from. The rebuilt sequencer's base is a canonical
machine at position `n_m` and count `X_m`, and it needs `T[X_m..K)`. A client's
base is a count `X_c` at which its state is canonical, reached from its own
backup, and it needs `T[X_c..K)`.

The hard part is computing `T[X_m..K)` without scanning from genesis. The
canonical machine makes it possible. Its application state is the sequencer's
starting state, and three numbers that its scheduler reports under the next
template let the tool replay the canonical scheduler from where the machine
stopped:

- its **position** `n_m`: how many InputBox inputs it has consumed;
- its **batch nonce** `N_m`: the nonce the next batch must carry;
- its **scheduler clock** `L_m`: the block its fridge has been drained through,
  the latest of every accepted frame's `safe_block` and every drained direct's
  block.

The tool reads the inputs of L1 blocks `(L_m, C]` once and splits them at the
machine's position:

1. **Below `n_m`** the machine has already consumed the inputs. The tool drops
   their batches without judging them and keeps their well-formed directs.
   Those are exactly the machine's *fridge*: the directs it admitted but has
   not executed yet.
2. **From `n_m` on**, it runs the canonical scheduler from the machine's own
   state: batch nonce `N_m`, that fridge and clock `L_m`.

The tool does not know which user ops the application will include, so it hands
the sequencer *candidates*. The sequencer executes them; the ones that execute
form `T[X_m..K)`, and each client executes its own part of that list.

## Terms

- **Template**: the initial machine whose root hash the on-chain application
  pins as `getTemplateHash()`. A deployed application keeps its template for
  life ([Cartesi Machine facts](../cartesi-machine.md#versions-are-all-or-nothing)),
  so a rule that changes the guest ships with a new template, which is to say
  a new deployment. A **machine** is any stored state of the canonical
  machine, such as a validator's at the end of an epoch or a watchdog
  checkpoint.
- **Effective count `X` and effective block `E`**: an application's progress,
  `executed_input_count` and `last_executed_safe_block` in code
  ([application contract](../protocol/application-contract.md#3-the-safe-block-clock--last_executed_safe_block)).
  Both follow executed transactions only: a skipped op, a rejected batch or a
  queued direct moves neither. The count numbers the feed's rows.
- **Position `n`**: how many InputBox inputs a state has consumed, which is
  also the index of the next input. It differs from the effective count `X`,
  because a batch envelope or a queued direct is an input but not a
  transaction. The watchdog's checkpoint names, `<block>-<input count>`, hold
  positions.
- **Scheduler state**: the batch nonce `N`, the fridge and, under the next
  template, the scheduler clock `L`. `L` and `E` stay separate on purpose. A
  frame raises `L` to its `safe_block` even when it runs nothing, while `E`
  moves only with executed transactions. The scheduler reads `L`, so it never
  depends on which ops the application included. Merged, `E` would move when
  nothing executed: two states at one count could then differ, and a consumer
  that sees only executed rows could not follow `E`.
- **`C`** is the stop block, a safe head fixed after the flush; **`N'`** and
  **`L'`** are the batch nonce and the scheduler clock at its end.
- **`P` and `B`**, as in the watchdog's latch record: `P` is its head, the last
  block the sequencer agreed with, or the uncompared bootstrap block before
  the first agreement, and `B` the block of the comparison that failed.
- **Conformance vectors**:
  [`consensus_vectors_v1.json`](../../tests/fixtures/consensus_vectors_v1.json),
  frozen cases (EIP-712 hashes, signatures, SSZ bytes, fee conversions, guest
  scenarios with their exact outputs) that every port of the scheduler must
  reproduce ([conformance](../protocol/scheduler-semantics.md#conformance)).

## The tool as a function

In ML notation:

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

(* The scheduler's state. It reads nothing from the application. *)
type sched = { nonce : nonce;           (* N: the next batch's nonce *)
               fridge : direct queue;   (* admitted directs not yet executed *)
               clock : block }          (* L *)

val step : sched -> raw_input -> sched * tx list

(* What the base machine reports through its scheduler query. *)
type base = { position : index; nonce : nonce; clock : block }  (* n, N, L *)

(* Candidates from the base's position through the end of stop, and the
   scheduler state there. *)
val tool : gateway -> base -> stop:block -> tx list * sched
```

In code:

```ocaml
let tool gateway m ~stop =
  let inputs = read gateway ~after:m.clock ~through:stop in
  let before, after = List.partition (fun i -> i.index < m.position) inputs in
  (* Phase 1: the fridge. Batches the machine consumed are not judged again. *)
  let fridge = List.filter (fun i -> is_direct i && well_formed i) before in
  (* Phase 2: the canonical scheduler, from the machine's own state. *)
  run_steps { nonce = m.nonce; fridge; clock = m.clock } after
```

**Why the split is exact.** Under the next template every scheduler state keeps
one invariant: *every admitted direct at or below `L` has executed, and every
one above `L` is still in the fridge.* Every drain empties the fridge's front
through some block, oldest first:

- a frame drains every direct up to its `safe_block`, which lies strictly below
  its batch's block, so all of them have arrived, and it raises `L` to that
  `safe_block` even when it drains nothing;
- the backstop drains every direct at least `MAX_WAIT_BLOCKS` old;
- an overflow drains whole blocks older than the current input's;

and the backstop and an overflow raise `L` to the last block they took. So `L`
stays below the current input's block. A newly admitted direct therefore lands
above `L`, and so does every input from `n_m` on: the scan `(L_m, C]` holds all
of phase 2. And the machine's fridge is exactly the well-formed directs above
`L_m` with index below `n_m`. Three template rules make this hold
([rules](#rules-for-the-next-template)). Without them a block can be split
between executed and queued directs, the scan could miss the rest of the
machine's last block, and the tool would need the fridge's first index as a
fourth value.

**Why batches before `n_m` are not judged again.** The machine already judged
them, and a second judgment can differ. A faulty submitter lands batch nonce 10
at block 200, with a frame at 190; the canonical scheduler expects 9 and
rejects it. Batch 9 lands at block 201, with its one frame at 185, and runs. So
a machine at the end of block 201 has `N_m = 10` and `L_m = 185`, and its scan
`(185, C]` includes block 200. A replay that judged the block-200 batch again
would find the expected nonce and a first frame at or above `L_m`, accept it,
run ops the canonical machine never ran, and then reject the real batch 10.

**Why each value is needed.**

- **The position.** The batch nonce cannot replace it, as the example shows,
  and neither can a block. A machine can stop inside a block: Dave's last
  finalized machine does when an input lands after the sealing transaction in
  the same block. Even at a block's end, the machine cannot know whether its
  last block held more inputs. Each input carries its index on L1, so the tool
  still queries by block range and splits by index.
- **The batch nonce**, for the nonce gate.
- **The clock**, for the floor gate. A batch whose frame at 100 runs ops and
  whose frame at 140 runs nothing leaves `L = 140` and `E = 100`. A later batch
  starting at 120 is rejected canonically; a replay floored at `E` would accept
  it.

**What the tool reads besides L1.** `step` reads no application state. It does
read what the template fixes: the sequencer address, `MAX_WAIT_BLOCKS`, the
fridge bound and the application's stateless rule for well-formed directs, so
for libdex the scheduler library embeds a port of Bart's rule. Signature
recovery is pure, so `step` does it. The user ops it emits are *candidates*:
the application that executes them decides which ones execute, from its
senders' nonces (and, without the
[authorization rule](#rules-for-the-next-template), their fee balances). The
canonical machine is `step` followed by the guest's application. That holds
while the guest never answers an input with `RX_REJECTED`, as the
[fatal conditions](../protocol/scheduler-semantics.md#fatal-conditions) require:
the host's revert would roll the scheduler state back with the application, a
[question for Bart](#questions-for-bart).

**Cost.** The scan reads blocks `(L_m, C]`. While well-formed directs keep
arriving, the backstop keeps `L` within `MAX_WAIT_BLOCKS` (about 4 hours) of the
latest input. After a quiet stretch `L_m` can trail further, but a quiet
stretch carries few inputs. For the default machine the range also spans
Dave's settlement lag, [still to be measured](#measure-before-choosing).

**Completeness.** A provider that drops a log would drop a direct from the
fridge or a batch from the replay. The tool reads the InputBox's input count at
blocks `L_m` and `C`, checks that the indices it reads run without gaps from
the first count to the second, and refuses a base whose position lies outside
those two counts. The count at `L_m` needs an RPC with state at that block, an
archive node for an old base, as the watchdog's `init` does
([commands](../watchdog/README.md#commands)).

## The consumers

```ocaml
(* Executes each tx through the shared execution boundary, as the canonical
   application does: a user op its state does not authorize is skipped.
   Returns the new state and the transactions it executed, with their counts. *)
val execute : state -> tx list -> state * (count * tx) list

let sequencer m ~stop =
  let candidates, sched = tool gateway (scheduler_query m) ~stop in
  let state, executed = execute (project m) candidates in    (* T[X_m..K) *)
  (import state sched, executed)

(* s: a client state restored to its base, a count between X_m and K. *)
let client s executed = fst (execute s (from_count (count s) executed))
```

Only the sequencer executes candidates, because only its state sits at the
machine's position. It holds the nonces (and, without the authorization rule,
the balances) that decided inclusion there. This yields `T[X_m..K)` with each
transaction's count, and the sequencer writes that list for the clients. A
client executes the part from its own base `X_c`, which needs
`X_m <= X_c <= K`: the machine must be no newer than any client's base.

Only the sequencer receives `sched`. It imports `N'` and `L'`, and its root
frame drains the queued directs ([below](#queued-directs-at-the-stop-block)). A
client receives those directs as the new era's first feed rows, like any other
row, and never holds scheduler state.

### Why a list and not a plugin

The alternative is a `RichClientApp` plugin that the tool runs to produce each
client's end state. The list is better for three reasons:

- **Data crosses the boundary, not code.** The tool depends only on the
  scheduler, which a template fixes; a client's projection, schema and storage
  change on their own schedule.
- **One execution boundary.** A client already executes L2 transactions
  through the shared execution boundary, and recovery reuses it. An entry
  carries its count (the feed's offset) and what the canonical application
  reads; it carries no batch nonce (a direct the backstop drains belongs to no
  batch) and no transaction hash. A plugin would be a second path that runs
  once in a deployment's life and rots.
- **History, not only state.** Bart's client keeps trades and transfers, and
  executing its part of the list replays that history, in canonical order.

### What the count is for

The tool positions the machine by its input position and never reads a count.
The consumers use the count three ways:

- **Positioning a client.** A client has no input position, only its count, so
  its part of the list is cut by count.
- **Joining the new era.** The era serves rows from `K`, which `/history`
  reports as `available_from`. After its part of the list, a client compares
  its count with `K` and subscribes there. The comparison must be explicit: the
  feed refuses a count below `K` but serves one above it.
- **Matching the machine.** The wallet's comparison bytes embed the count, so
  the sequencer's `K` must equal the machine's, and the projection from a
  machine must carry it.

## States at a base

Each consumer's state must sit exactly at its base: a state one transaction
short or past it would drop or repeat one.

### The sequencer's base

The sequencer's state is projected from a machine. The projection must carry
the effective count and block that the consumers' counts start from: the
wallet's `state` inspect report holds both, and libdex keeps its state as a
flat buffer in one of the machine's drives, which may or may not hold them
([application contract](../protocol/application-contract.md#7-canonical-recovery-integration)).
The three values come from the machine's `scheduler` inspect query, a rule for
the next template. Any canonical machine the operator trusts will do if it is
no newer than any client's base; the tool enforces no provenance. After a
divergence every client's base lies at or before the watchdog's head `P`,
which rules out a machine past it.

| Machine | Position | Trust |
|---|---|---|
| Dave's last finalized machine, the default | the current sealed epoch's `inputIndexLowerBound`, inside a block when an input lands after the sealing transaction in the same block; the scheduler query reports it too | its root is on chain |
| The watchdog's head | the input count in its name | the operator's watchdog replayed it from L1; before its first agreement, the operator trusted it at `init` |
| A `replay --out` machine | the input count at the end of the block it was replayed to | the operator's watchdog replayed it from L1 |
| `incident/canonical`, only for a deployment without clients | the input count at the end of the latched block `B`, past every client's base | the operator's watchdog replayed it from L1 |
| The template | zero, with nonce and clock zero; the tool then reads every input since the deployment block | its hash is on chain |

A client whose base is older than the default machine needs an older machine:
`replay --out` derives one from an archived checkpoint or the template.

A matching root authenticates a machine only from a source we trust, such as
our own Dave node: a stored machine's root comes from its stored hash tree, not
from its contents ([hashing](../cartesi-machine.md#hashing)). An upcoming
emulator release brings a mechanism to verify an untrusted load; until then,
`verify_hash_tree` is not that mechanism and is
[on the watchdog's to-do list](2026-09-watchdog-v2.md#remaining-work).

### The client's base

A client's base is a count `X_c` at which its state is canonical. A watchdog
agreement vouches for one, since the sequencer reports its count with each
comparison. It vouches for the state, not for the client's rows before it
([after an incident](#after-an-incident)). An accepted checkpoint alone does
not: accepted means only that our batch landed and passed a partial gate check,
which skips gate d
([I1](../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)),
so `/history` keeps advancing past a divergence that only the watchdog sees.
For the wallet the comparison pins the count, since its bytes embed it; for
libdex that depends on Bart's answer.

The sequencer's digest response already carries its count, but the watchdog
parses only the block and the digest, and deletes each checkpoint once a newer
one agrees. So it must append each agreement to a log kept outside its state
directory, which a re-`init` wipes: the agreement's block, the sequencer's
count, and the era and generation that count was read under, which the digest
response must add.

A client reads its base from that log, not from a sequencer, which may be gone
or wrong. It takes the newest agreement on its own history, normally the head.
An agreement is on the client's history when its count is at or below the
client's and either:

- its era is the client's and its generation is the client's or an earlier
  one, since standard recovery never cuts an accepted row; or
- its era is one the client left at a rebuild, and its count is at or below
  the base the client crossed each later rebuild from.

So a client that just realigned keeps the base it crossed from until its new
era logs an agreement, which is also the comparison that checks the rebuild.
Before a deployment's first agreement the log is empty, and a client's base is
count zero, its initial state.

A client that follows the optimistic feed is past its base by the time the
watchdog compares, and its database can hold terabytes. Three rules make any
count on its history restorable, with any backup:

1. **Every commit is a boundary and keeps its rows.** Apply each row, from the
   feed or from the executed list, or each run of rows, in one SQLite
   transaction that also writes the count, era and generation and stores the
   rows themselves. Any consistent copy of the database is then at a
   transaction boundary, says which, and holds every row before it.
2. **A cut discards later snapshots.** A standard recovery cuts the history at
   its preserved count
   ([checkpoint compatibility](../protocol/application-history.md#checkpoint-compatibility-after-standard-recovery));
   a rebuild cuts it at the client's base. The new history replaces the rows
   past the cut and reuses their counts. So once realigned, the client deletes
   every snapshot past the cut and stops snapshotting its old live database.
3. **Snapshot anywhere.** Take snapshots when convenient; none needs to line up
   with a checkpoint.

To reach `X_c`, restore the newest snapshot at or below `X_c`. Then replay the
rows from that snapshot up to `X_c` through the client's normal apply, and
check the count. The rows can come from the live database or from any newer
snapshot, since each holds every row before it. Never restore a snapshot past
`X_c`: its state includes rows past the base, which may not be canonical.
Replay is exact only if applying a row reads nothing but the database and the
row. A rebuilt sequencer serves only its own era, so the client keeps its old
live database until it has realigned.
[Notes on snapshotting](#notes-on-snapshotting-a-large-sqlite-database) cover
the storage side.

### After an incident

The sequencer may start from any canonical machine no newer than every
client's base. After a sequencer incident a client starts from the newest
logged agreement on its history, normally the watchdog's head. An agreement
vouches for the sequencer's state, not for the order of the rows before it:
rows that commute reach the same state in either order, so a matching state
does not validate the client's history
([reader state](../recovery/cockroach.md#application-specific-reader-state)).
If diagnosis finds the fault began before that agreement, the client starts
from an older logged agreement that predates it, and the sequencer then needs
a machine no newer than that, which `replay --out` derives.

- **The watchdog latched a `state_mismatch`.** Its head stays at `P`, which
  need not be the block before `B`. A tick compares only the newest accepted
  checkpoint, so the accepted checkpoints in `(P, B)` were never compared, and
  the compared state can first differ anywhere in `(P, B]`
  ([incident runbook](../watchdog/incident-runbook.md)). The client followed
  the faulty sequencer through all of them. The other latches call for no
  rebuild: a dead canonical machine ends the application
  ([preconditions](#preconditions-the-design-must-state)), and a regression is
  an operator error
  ([runbook](../watchdog/incident-runbook.md#inclusion_block_regressed)).
- **The sequencer lost its database, or its content-identity check tripped**
  ([I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen)).
  A tripped check answers the finalized routes with 503 and does not latch the
  watchdog, so its head stays at the last comparison before the trip.
- **The client has a bug.** This is the client's own repair, not a rebuild: it
  restores a snapshot older than the bug's first effect and replays its rows
  with the fixed code, across any rebuild, since the executed list's entries
  are rows in its database. It then deletes every snapshot newer than the one
  it restored, since they carry the bug's state.

## Queued directs at the stop block

At the end of `C` the canonical scheduler can still hold queued directs. By the
invariant above, they are exactly the well-formed directs in blocks `(L', C]`.

The rebuilt sequencer starts from the scheduler state at the end of `C`. It
keeps no fridge: its input table, which setup fills from the deployment block,
already holds every direct. Today the lane drains directs from the first input
after the later of a floor block and its latest frame's `safe_block`, and a
rebuild puts that floor at `C`. The change is that the floor becomes `L'`, in an
era field of its own: `base_safe_block` stays `C`, since it also serves as
`/history`'s `l1_stop_block` and the end of historical inputs. So the root
frame, at `C`, first executes the queued directs, as the first canonical frame
drains them, before any user op, and clients receive them as ordinary feed
rows. A cascade that invalidates every frame
([I16](../invariants.md#i16-the-batch-tree-has-exactly-one-valid-parentless-root-carrying-the-deployments-anchor-nonce))
opens a new root, which drains above `L'` again. Acceptance still scans only
blocks after `C` ([replay boundaries](../recovery/cockroach.md#replay-boundaries)).

Today's procedure instead executes the queued directs during recovery (the
terminal drain) and publishes the result, and each client repeats that drain
after restoring the scheduler's queue and nonce
([projection replay](../protocol/projection-replay.md)). That state is a
prediction: the canonical machine reaches it only inside the post-`C` input
that drains the last of the queued directs, often executing more there, so no
comparison can check it, a client that applied it must roll it back if a
second rebuild comes, and a rebuild that started from it would execute those
directs twice. Track 3 kept the terminal drain because replacing it would carry
pending work across the baseline
([scope boundaries](2026-07-track3-feed-replay-design.md#scope-boundaries));
that work crosses as one block, `L'`.

## The procedure

1. **Choose the bases.** Each client restores its state to the newest logged
   agreement on its history that precedes any fault the diagnosis found
   ([the client's base](#the-clients-base)). The sequencer then takes a
   canonical machine no newer than any of them: by default Dave's last
   finalized one, else the watchdog's head or a `replay --out` machine
   ([the sequencer's base](#the-sequencers-base)).
2. **Flush, then fix `C`.** Settle the old instance's transactions, then take
   a safe head observed after they settle as `C`. The flush is best effort: it
   replaces what the provider's pool holds for the submitter account and waits
   until that is safe. It cannot rely on the old watermark, which does not
   survive every event that calls for a rebuild
   ([flush boundary](../recovery/cockroach.md#flush-and-stopping-block)). As a
   standalone subcommand, it is the only step that needs the key. Refuse if a
   submitter input above `C` is already ingested.
3. **Run the tool.** Read the machine's position, batch nonce and clock with
   its scheduler query. The tool refuses a position below the InputBox count at
   `L_m` or past the end of `C`, reads blocks `(L_m, C]`, and writes the
   candidates and the scheduler state at `C`, the latter last as the commit
   point.
4. **Rebuild the sequencer.** Execute the candidates from the state projected
   from the machine, write the executed list for the clients, and import the
   result with `N'` and `L'` in one transaction. Then start the sequencer with
   `run` promptly. A late start is safe: once the root frame ages past the
   danger threshold, startup's
   [Tip repair](../recovery/README.md#open-tip-repair-without-flushing) reopens
   it at the current safe head, still draining above `L'`.
5. **Realign clients.** Each client:
   1. reads `/history` for the new era and generation;
   2. executes its part of the executed list, committing its entries as rows
      under that era and generation (rule 1);
   3. checks that its count equals `available_from`, and records with it the
      base it crossed the rebuild from;
   4. deletes every snapshot past its base (rule 2), then takes a snapshot;
   5. subscribes at its count with that era and generation.

The new era's first watchdog comparison checks the result a batch later: a
wrong list, `N'` or `L'`, or a guest compiled for another sequencer address,
shows up there as a divergence. Replaying the watchdog's head to `C` with
`replay`, which starts only at the end of a block, checks it before `run`, at
an emulated cost. The tool cannot see a halt, which reads application state; a
consumer that reaches the halting transaction fails there, as the canonical
machine did.

## Rules for the next template

The owner settled these on 2026-10-29 and 2026-11-02. Each changes the guest,
so each needs Bart's agreement and ships with the next template, ours and his
port's ([register](../review/register.md#canonical-rules-open-for-the-c-port)).
The tool relies on three of them: whole-block overflow, strict frame clocks and
the scheduler clock. They keep `L` below the current input's block, which makes
phase 1 exact and puts every input from the position on inside the scan. Phase
2 then replays whatever the template applies. The tool runs `step` without
application state; the only application code inside `step` is the fridge's
well-formedness rule, so that rule may read only the direct and fixed
configuration.

- **Fridge bound.** Only well-formed directs enter the fridge, under an
  application-defined rule that reads only the direct and fixed configuration,
  and a count bounds it
  ([register](../review/register.md#bound-the-canonical-direct-input-fridge)).
  A direct that is not well-formed is dropped without executing. An overflow
  drains whole blocks from the front, oldest first, until there is room. The
  bound must hold every direct one block can carry, about 1,500 to 2,000 at
  today's 60M gas limit, for the deployment's life. Then an overflow never
  needs the current input's block, and it drains at most one block more than
  it must. The sequencer cannot mirror an overflow, so the batches it already
  sealed diverge, and a manual cockroach recovery answers that. Making
  standard recovery repair one is deferred on complexity, since standard
  recovery is the hardest part of this software.
- **Strict frame clocks.** A frame's `safe_block` must lie strictly below its
  batch's inclusion block; gate d accepts equality today. A correct sequencer
  never posts equality, since its frames take the L1 safe head and its batch
  lands later. Equality lets a frame drain only part of its own block. The
  frozen conformance vectors post such a frame in 18 of their 19 guest
  scenarios, so this rule ships with v2 vectors.
- **The scheduler clock and floor gate.** The scheduler keeps `L`: the latest
  of every accepted frame's `safe_block` and every drained direct's block. A
  new gate, placed after gate e, rejects a batch whose first frame lies below
  `L`; empty batches stop at gate c and never reach it. It reads `L`, never `E`
  ([terms](#terms)). Applications may then read blocks as time: a direct's
  inclusion block and a user op's frame `safe_block` never go backwards along
  `T`. Raising `L` on a backstop drain changes no gate outcome, not even a
  report: the backstop drains only directs at least `MAX_WAIT_BLOCKS` old, so
  any batch whose first frame lies below one is stale and stops at gate e.
  Recovery still needs that raise: without it, the directs the backstop
  executed would sit above `L_m`, and the tool would queue and execute them
  again. On the sequencer side, the gate makes
  [I3](../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine)
  consensus-critical: every transition that opens a Tip needs an assert that it
  starts at or above the latest valid frame. `scheduler_accepts` leaves the
  floor out, as it leaves out gate d
  ([I1](../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)):
  those asserts keep the sequencer's own batches at or above it, and an
  overflow's raise, which rejects batches the lane already sealed, is the
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
- **A `scheduler` inspect query** returning the machine's position, batch
  nonce and clock: the tool's input. The position is the last input's index
  plus one, batches included, or zero for the template.

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
- **Consumers execute bit for bit.** The sequencer executes the candidates, and
  every client its part of the executed list, through the canonical validation
  and execution; for Bart's client that means embedding libdex exactly as the
  guest runs it.
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

The owner settled these on 2026-10-29 and 2026-11-02:

| Decision | Choice |
|---|---|
| Shape | From L1, the scheduler library and three values the base machine reports (position, batch nonce, clock): candidates from the machine's position and the scheduler state at `C`. No fold from genesis to reach the machine |
| Phases | Blocks `(L_m, C]` read once: below the machine's position, directs only, and batches never judged again; from it, the canonical scheduler |
| Clients | Each executes its part of the sequencer's executed list, from its own count, instead of a plugin per client; the machine is no newer than any client's base |
| Sequencer's state | Projected from a machine the operator trusts, by default Dave's last finalized one; no enforced provenance |
| Clocks | `E` (the application's, from executed transactions) and `L` (the scheduler's, from accepted frames and drains) stay separate |
| Queued directs at `C` | A direct floor at the block `L'`, replacing the terminal drain. It applies the tool's phase-1 rule (every queued direct lies above the clock), the lane's floor is already a block, and a database today's procedure rebuilt keeps its floor at its `C`. Settled on 2026-11-02, in place of the index `F` chosen on 2026-10-29 |
| Flush | Best effort, trusting the provider's pool view |
| Fencing | An operator requirement, in the threat model's [operator mistakes](../threat-model/README.md#operator-mistakes) |
| Next template | The fridge bound with whole-block overflow, strict frame clocks, the scheduler clock and floor gate, authorization deciding inclusion, direct context, and the `scheduler` query |
| Fridge overflow | A manual cockroach recovery; standard recovery does not repair it |

These stay open:

| Decision | Recommendation |
|---|---|
| Client's base | The newest agreement on its history, from a log in which the watchdog records each agreement with the sequencer's count, era and generation. The alternative, a hash chained over executed transactions that the guest keeps in its state, would check a client's whole history, order included, and drop the log and the cut rule, at the cost of a template change |
| Client backups | Every commit writes the count and keeps its rows, the executed list's included; a cut discards later snapshots; snapshots taken anywhere and replayed to the base |
| Executed list's delivery | A file the rebuild writes when it executes the candidates (step 4), before `run` starts. Serving it as the new era's first feed rows instead needs an immutable store for rows before `K` and a batch nonce for directs the backstop drained |
| Template tie | One template per release, enforced at setup and `run` |
| Shared gate function | Defer: it changes the gold frontier, the heart of standard recovery |
| `trait Scheduler` | Defer: for libdex deployments the scheduler library is ours, with libdex's direct rule ported, held to Bart's guest by the versioned gate vectors of the next template |
| Threat-model statement | Add that key loss or compromise means a new deployment until the forwarding contract |

## Questions for Bart

- Does libdex's drive hold only its flat buffer, or a padding tail too? Does
  that buffer hold the effective count and block, so that a state projected
  from a machine starts at the canonical count and a comparison pins it?
- Can your guest answer the `scheduler` query: its position, batch nonce and
  clock?
- Which of a direct's index, timestamp and prevRandao does libdex read?
- Is your guest's compiled sequencer address the pinned submitter? Can its
  harness or scheduler answer an input with `RX_REJECTED`?
- Does your scheduler reproduce the conformance vectors, and reject an
  out-of-range `fee_price` at gate d, as ours does since PR #56? Will it adopt
  strict frame clocks with the v2 vectors?
- Which directs are well-formed for libdex, and how large can one be? The
  fridge's count bound bounds memory only if each entry's size is bounded, and
  it must hold every direct one block can carry.
- Does libdex's validation refuse a user op for any reason besides its
  signature, nonce and fee cap? Under the authorization rule, every other
  reason becomes a business failure that still advances the nonce and charges
  the fee.
- Can your client follow [the backup rules](#the-clients-base): the count in
  every commit with its rows, and snapshots discarded past a cut? How large is
  its database, which SQLite version does it link, and does applying a row
  read anything besides the database and what a list entry carries, such as
  the row's batch nonce or a direct's transaction hash?
- What are libdex's per-op cycles and state size? Who fixes libdex during an
  incident?

## Measure before choosing

- The scan of blocks `(L_m, C]`, and signature recovery and execution over it,
  at the production deployment's Dave lag, which sets how long the default
  machine's list is.
- The guest's cost to drain a full fridge in one input, which the root frame
  after a rebuild and the first frame after an outage both do, and one
  overflow's drain of a full block.
- A client's replay, from its newest snapshot at or below its base to that
  base.
- The time to recover, end to end, in a drill.

## Notes on snapshotting a large SQLite database

From the SQLite, Litestream, OpenZFS and AWS documentation as of 2026-10-29:

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
  transactions, so pick one at or below the base and let the rows in the live
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
  below the client's base and every newer one on the current history. The
  watchdog's head stays put while it is latched, idle or stopped, so an age
  window, such as Litestream's default of 24 hours, can delete the only usable
  base. A bug in the client, or a sequencer fault diagnosed before the head,
  needs a snapshot older than it, which only a longer retention covers.

## Remaining work

- Settle the open decisions; the template rules wait on Bart's answers.
- Tool: the two-phase replay from the three values, from a `step` split out of
  `Scheduler<A>`, with the scan's completeness check; and the flush
  subcommand. Today's scheduler recovers every signature, hands an op to the
  application in two calls (`validate_user_op`, then `apply_valid_user_op`)
  and keeps no input index in the fridge, so an application that records what
  it is handed cannot produce the candidates.
- Guest, for the next template: the rules above, the `scheduler` query, and
  v2 conformance vectors.
- Sequencer: project a machine's state, execute the candidates, write the
  executed list, import `N'` and `L'`, and drain directs above `L'`. A database
  that today's procedure already rebuilt has executed its queued directs in
  the terminal drain, so its floor stays at its `C`. Draining above `L'`
  replaces the terminal drain, so rewrite what pins that drain today: the
  projection replay steps, the README's nonce note, the history guide's era
  baseline, snapshot lifecycle, invariants I6, I8, I11 and I20, and the
  rebuild drill's replica check. I1's documented omissions add the floor gate.
- Clients: the backup rules, and a driver that executes a part of the executed
  list through the shared execution boundary and commits its entries as rows.
- Watchdog: parse the count, era and generation from the digest response,
  which must add the latter two, and append each agreement to a log that
  survives a re-`init`; and `export-state`, which writes a machine's
  state-source bytes for the sequencer's projection, and the position, batch
  nonce and clock its `scheduler` query reports for the tool, and nothing
  application-specific.
- Dave's machine: read the last finalized root and the sealed epoch's
  `inputIndexLowerBound` from the consensus contract, which nothing in this
  repository reads yet, and check the index against the scheduler query's
  position.
- Drills that rebuild from a machine and realign a client, in the
  [incident runbook](../watchdog/incident-runbook.md#drills); until the tool
  lands, `setup --recovery` keeps its archive drill.
- Then the deletions the
  [tracks board](2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
  lists for the tool's landing, and `setup --recovery`, which the tool replaces.
