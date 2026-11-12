# Recovery from a canonical machine

**Status:** archived on 2026-11-05, never implemented. A candidate of
2026-11-03 that simplified the [shared recovery tool](2026-10-shared-recovery-tool.md);
the owner's review of it led to
[sequencer recovery](../../plans/2026-11-sequencer-recovery/README.md), which
supersedes both. Its claims were checked by reading the code at `4e7b34b`
(main with PR #56), Dave at `dbcca7d4` and the emulator at v0.22.0-test3, and by
adversarial review; none was executed.

Cockroach recovery rebuilds a sequencer after an instance loses its database
or its behavior cannot be trusted. The operator triggers it by hand and keeps
that instance down. The design takes the worst case: the submitter key is
ours, but the instance's database, snapshots and receipts prove nothing. The
canonical scheduler is the authority, bug for bug. A client, Bart's DEX
backend, keeps a richer state than the sequencer and must realign after a
rebuild, from its own backup and with its own code. Bart operates both; only
their development is split. Recovery runs rarely, so each step must be simple
enough to explain, practice and trust.

## The idea

Let `T` be the canonical sequence of executed L2 transactions, in execution
order. Each application state carries its **count** `X`: how many of them it
has executed.

> A state on the canonical history with count `X` has executed exactly
> `T[0..X)`. Nothing but executed transactions changes an application, so the
> state is missing exactly `T[X..K)`, where `K` is the count at the stop block
> `C`.

Recovery picks one **base** for every consumer: a canonical machine, at
position `n_m` and count `X_m`. The rebuilt sequencer projects its state from
that machine. Each client restores its own state to `X_m` from its backup and
checks it against the machine. Then every consumer needs the same thing,
`T[X_m..K)`.

The rebuild starts from the machine, not from genesis. Under the next template
the machine's scheduler reports four numbers:

- its **position** `n_m`: how many InputBox inputs it has consumed;
- its **batch nonce** `N_m`: the nonce the next batch must carry;
- its **fridge start** `F_m`: the index of the oldest direct still in its
  fridge, or `n_m` when the fridge is empty;
- its **scheduler clock** `L_m`: the floor its next batch must not start
  below.

The rebuild reads its input table from index `F_m` and splits it at `n_m`:

1. **Phase 1**, inputs from `F_m` below `n_m`: the machine already consumed
   them. Keep the well-formed directs, which are exactly the machine's fridge,
   and drop the batches without judging them.
2. **Phase 2**, inputs from `n_m` through `C`: run the scheduler and the
   application from the machine's state, with batch nonce `N_m`, that fridge
   and clock `L_m`.

Phase 2 records every transaction that executes. That record, the **executed
list** `T[X_m..K)`, is all the sequencer hands the clients. What is shared is a
list of transactions, not a tool.

## Terms

- **Template**: the initial machine whose root hash the on-chain application
  pins as `getTemplateHash()`. A deployed application keeps its template for
  life ([Cartesi Machine facts](../../cartesi-machine.md#versions-are-all-or-nothing)),
  so a rule that changes the guest ships with a new template, which is to say
  a new deployment. A **machine** is any stored state of the canonical
  machine.
- **Count `X`**: how many transactions a state has executed:
  `executed_input_count` in code, though it counts transactions, not inputs.
  A skipped op, a rejected batch, a dropped direct or a queued direct does not
  move it. It numbers the feed's rows.
- **Position `n`**: how many InputBox inputs a state has consumed, which is
  also the index of the next input. It differs from the count, because a batch
  envelope or a queued direct is an input but not a transaction.
- **Scheduler state**: the batch nonce `N`; the fridge, the admitted directs
  not yet executed, oldest first, each with its input index; and, under the
  next template, the scheduler clock `L`: the last accepted frame's
  `safe_block`, raised by an overflow to the youngest drained direct's block.
  Under the next template the application keeps no clock of its own
  ([progress is the count alone](#rules-for-the-next-template)). A
  transaction's block reaches it as an argument: a direct's inclusion block, a
  user op's frame `safe_block`.
- **Comparison digest**: the SHA-256 of an application's comparison bytes, the
  state the watchdog already compares
  ([application contract §6](../../protocol/application-contract.md#6-checkpoint-lifecycle)).
- **`C`** is the stop block, a safe head fixed after the flush. **`N'`**,
  **`F'`** and **`L'`** are the batch nonce, fridge start and scheduler clock at
  its end; `F'` is the first input after `C` when nothing is queued.
- **`P` and `B`**, as in the watchdog's latch record: `P` is its head, the last
  block the sequencer agreed with, or the trusted bootstrap block before the
  first agreement, and `B` the block of the comparison that failed.

## The rebuild

```ocaml
(* What the base machine reports through its scheduler query. *)
type base = { position : index; nonce : nonce;
              fridge_start : index; clock : block }   (* n, N, F, L *)

(* One row of the executed list: a transaction and its count. *)
type row = { count : count; tx : tx }

(* The states at the end of stop, and the executed list T[X_m..K). *)
val rebuild : machine -> stop:block -> state * sched * row list

let rebuild m ~stop =
  let b = scheduler_query m and app = project m in
  (* Phase 1: the fridge. Batches the machine consumed are not judged again. *)
  let fridge = input_table ~from:b.fridge_start ~below:b.position
               |> List.filter (fun i -> is_direct i && App.well_formed app i) in
  (* Phase 2: the scheduler and the application, from the machine's state. *)
  let s = Scheduler.resume app ~nonce:b.nonce ~fridge ~clock:b.clock in
  Scheduler.run_recording s (input_table ~from:b.position ~through:stop)
```

**Why phase 1 is exactly the fridge.** Only well-formed directs enter the
fridge, and every drain takes its front: a frame, the backstop and an overflow
all execute the oldest queued directs first. So the queued directs are always
every admitted direct from the oldest queued one on, and the machine's fridge
is exactly the well-formed directs with index from `F_m` below `n_m`.

**Why batches below `n_m` are not judged again.** The machine already judged
them, and a second judgment can differ. A faulty submitter lands batch nonce 10
at block 200; the canonical scheduler expects 9 and rejects it. Batch 9 lands
at block 201 and runs, so a machine at the end of block 201 has `N_m = 10`. A
rebuild that judged the block-200 batch again would find the expected nonce,
accept it, run ops the canonical machine never ran, and then reject the real
batch 10.

**Why each value is needed.**

- **The position.** The batch nonce cannot replace it, as the example shows,
  and neither can a block. A machine can stop inside a block: Dave's last
  finalized machine does when an input lands after the sealing transaction in
  the same block. Even at a block's end, the machine cannot know whether its
  last block held more inputs.
- **The batch nonce**, for the nonce gate.
- **The fridge start.** Among the directs below the position, some have
  executed and some still wait, and nothing else tells them apart: an
  overflow, for one, drains part of a block.
- **The clock**, for the floor gate. A batch whose frame at 100 runs ops and
  whose frame at 140 runs nothing leaves `L = 140`. A later batch starting at
  120 is rejected canonically; a rebuild that floored at the block of the
  latest executed transaction, 100, would accept it.

**Where the inputs come from.** The rebuild runs in the fresh data directory.
Setup fills its input table from the deployment block to a safe head at or
past `C`, and the L1 reader proves the table complete
([I6](../../invariants.md#i6-the-frame-clock-accounts-for-a-complete-l1-interval)):
indices contiguous from zero, and the InputBox count checked at the safe head
it commits. So the rebuild reads nothing from L1 itself and needs no archive
node, and the sequencer's lane later drains the queued directs from the same
table. The rebuild refuses a base that does not fit the table: `F_m` must not
exceed `n_m`, `n_m` must not exceed the inputs through `C`, and input `F_m`,
when it is below `n_m`, must be a well-formed direct.

**What a row holds.** A row is a feed row without `batch_nonce`: a direct the
backstop drained belongs to no batch, and every row is already settled. A
direct's row carries its input index, timestamp and transaction hash. Today a
direct enters the scheduler and its fridge with only its sender, block and
payload, so each direct carries its input index through the fridge, and the
rebuild reads the timestamp and hash from the input table by that index.

**What it costs.** Phase 1 reads the inputs from `F_m` below `n_m`, batches
included; the backstop keeps them within `MAX_WAIT_BLOCKS` of the machine's
last input. It only checks well-formedness, so it recovers no signatures and
executes nothing. Phase 2 recovers signatures and executes from the position
to `C`, so the cost is the base's age at `C`.

**How close today's code is.** [`fold_replay`](../../../sequencer-core/src/scheduler/fold.rs)
already seeds a fridge from a range with batches dropped unjudged, then
replays the rest from a batch nonce. The rebuild changes its seed range to
`F_m` up to `n_m`, adds the clock to `Scheduler::resume_at`, carries each
direct's index through the fridge, drops the terminal drain, and records each
executed transaction with its count, where the scheduler already sees an
included op and every drained direct.

**Our scheduler and the guest's.** The canonical machine is the guest's
scheduler followed by its application. The rebuild runs our scheduler with the
same application. It reproduces the machine while our scheduler agrees with
the guest's on every gate and drain, which the template's conformance vectors
test, and while the guest never answers an input with `RX_REJECTED`, as the
[fatal conditions](../../protocol/scheduler-semantics.md#fatal-conditions)
require. A rebuild that reaches a halting transaction fails there, as the
canonical machine did.

## One base for every consumer

The base is one canonical machine. It must predate any fault the diagnosis
found. Its count `X_m` must be at or below every client's count, because a
client holds rows only up to its own count and the old instance's feed is gone.

| Machine | Trust |
|---|---|
| The watchdog's head | the operator's watchdog replayed it from L1 and agreed with the sequencer there; before the first agreement, it is the bootstrap machine the operator trusted at `init` |
| Dave's last finalized machine | its root is on chain |
| The template | its hash is on chain; its scheduler state is zero, so the rebuild replays every input since the deployment block |

For a base at any other block, the watchdog's `replay --out` derives a machine
at the end of that block, starting from the template or from an archived
checkpoint whose block is trusted. Every machine reports its position, batch
nonce, fridge start and clock through its `scheduler` query. Which machine is
the default is [open](#decisions-for-the-owner).

A matching root authenticates a machine only from a source we trust, such as
our own Dave node: a stored machine's root comes from its stored hash tree, not
from its contents ([hashing](../../cartesi-machine.md#hashing)). An upcoming
emulator release brings a mechanism to verify an untrusted load.

**The sequencer** projects its state from the machine. The projection must
carry the count; for libdex, whose state is a flat buffer in one of the
machine's drives, that is a question for Bart.

**Each client** restores its state to `X_m`. It takes its newest snapshot at
or below `X_m`, replays its own rows up to `X_m` through its normal apply, and
checks the count. Then it computes its comparison digest and compares it with
the machine's.

- **A match** shows that the client's application state is canonical at
  `X_m`, whatever path brought it there. It needs no watchdog agreement.
- **A mismatch** has one of two causes. Either the client is wrong (a bug, a
  wrong restore or engine skew), and it fixes that itself and checks again; an
  older machine cannot fix it. Or its history left the canonical one before
  `X_m`; then the operator takes an older machine, and every consumer starts
  there.

A match says nothing about the client's own records before `X_m`: which rows
it holds, their order, or what it derived from them. Different rows can reach
the same state
([reader state](../cockroach.md#application-specific-reader-state)).
Those records are canonical only if the feed the client followed was. That is
why the base must predate any fault the diagnosis found. After a
`state_mismatch` latch that means at or before `P`, unless diagnosis places the
fault later: a tick compares only the newest accepted checkpoint, so the
compared state can first differ anywhere in `(P, B]`
([incident runbook](../../watchdog/incident-runbook.md)).

The check needs the client to produce the comparison bytes exactly as the
canonical machine holds them. For libdex that is its state `M` as the guest's
drive holds it, zero tail included: a question for Bart. Because the check
tests the client's own execution, it also catches a wrong restore, engine skew
and any client bug that changes the application state. It cannot see a bug
confined to the client's own records.

## Clients apply the list

A client applies the executed list as rows, through the feed's apply:

```ocaml
(* Every row executed canonically, so a refusal means a wrong base, list or
   engine: realignment stops at that row. *)
let client s rows =
  List.fold_left (fun s r ->
      assert (r.count = count s);
      apply_or_stop s r.tx)   (* a user op must carry its sender's next nonce *)
    s rows
```

The rebuild writes the list as a file, durably, before it publishes the era.
The file opens with a header: the new era, `X_m`, the machine's comparison
digest, `K` and the baseline's comparison digest. The operator keeps it until
every client has realigned. After the list, the client's count must equal the
new era's `available_from`, `K`, and its digest the baseline's. It subscribes
there and receives the directs still queued at `C` as the era's first feed
rows, like any other row. A client never holds scheduler state.

## Client backups

The plan asks two rules of Bart's client, and leaves the rest of its storage to
it:

1. **Every commit is a boundary and keeps its rows.** Apply each row, from the
   feed or from the executed list, or each run of rows, in one SQLite
   transaction that also writes the count, era and generation and stores the
   rows themselves. Any consistent copy of the database, taken at any time, is
   then at a transaction boundary, says which, and holds every row before it.
2. **A cut discards later snapshots.** A standard recovery cuts the history at
   its preserved count
   ([checkpoint compatibility](../../protocol/application-history.md#checkpoint-compatibility-after-standard-recovery));
   a rebuild cuts it at `X_m`. The new history replaces the rows past the cut
   and reuses their counts. So once realigned, the client deletes every
   snapshot past the cut and stops snapshotting its old live database.

To reach `X_m`, restore the newest snapshot at or below it, replay the rows
after it from the live database or a newer snapshot, and check the count and
the digest. Never restore a snapshot past `X_m`: its state includes rows past
the base. Replay is exact only if applying a row reads nothing but the database
and the row.

Retention sets recovery time, not correctness. A pruned snapshot costs a longer
replay from an older one, never a wrong base, because the digest check catches
a wrong state. Keep recent snapshots and a thinning tail that outlasts the
oldest base an operator would pick: Dave's settlement lag plus the time to
respond to an incident. Once an incident opens, stop pruning until every client
has realigned.

## The procedure

1. **Choose the base and check the clients.** Pick a canonical machine that
   predates any fault the diagnosis found and whose count is at or below every
   client's. Read its scheduler values, count and comparison digest. Each
   client restores a copy of its state to `X_m` and checks the digest. A client
   that mismatches first checks its restore and its code; pick an older
   machine only if its history left the canonical one before `X_m`. Nothing is
   committed yet.
2. **Rebuild** with one new command, which replaces `setup --recovery`. It
   needs the submitter key. In order, it:
   1. flushes the old instance's transactions and fixes `C` as a safe head
      observed after they settle. The flush is best effort: it replaces what
      the provider's pool holds for the submitter account and waits until that
      is safe ([flush boundary](../cockroach.md#flush-and-stopping-block));
   2. syncs the input table to a safe head at or past `C`;
   3. applies genesis setup's detection gate at `C`. Before publishing
      anything, it refuses if the submitter's wallet nonce is unsettled or a
      submitter input lies above `C`. Either means a transaction escaped the
      flush or the old instance still runs. Stop that instance and rerun; the
      rerun flushes again and fixes a later `C`;
   4. checks that the base fits the table, replays, and writes the baseline
      dump and the executed list durably;
   5. publishes the era in one transaction: count `K`, stop block `C`, batch
      nonce `N'`, direct floor `F'`, the baseline and setup completion. It
      prints the baseline's digest, `N'`, `F'` and `L'` for the check.
3. **Check**, always in drills, and in an incident unless the replay would
   outlast the outage budget. Replay the newest trusted machine that ends a
   block, normally the watchdog's head, to `C` with `replay`, then read it with
   `export-state`. Its digest, batch nonce, fridge start and clock must equal
   the values the rebuild printed. On a mismatch, fix the code and rebuild
   before any client realigns.
4. **Run.** Start the sequencer with `run`. Its startup opens the root frame at
   its own safe head, which drains every queued direct from `F'` before any
   user op, as the first canonical frame drains them. Clear the watchdog's
   latch; its next tick compares the baseline at `C`.
5. **Realign clients.** Each client applies the executed list from `X_m`,
   committing its rows under the new era and generation. It checks that its
   count equals `available_from` and its digest the baseline's, deletes every
   snapshot past `X_m`, takes a snapshot, and subscribes.

## Queued directs at the stop block

At the end of `C` the canonical scheduler can still hold queued directs: the
well-formed directs from index `F'` through the last input at or below `C`.

The rebuild publishes the era the way genesis setup does. It opens no root
frame; `run` opens one at its own safe head.

The era gets a new field, the **direct floor**, set to `F'`. The lane drains
queued directs from the later of the floor and the first input after its
latest frame's `safe_block`. Today the lane floors at `base_safe_block`, a
block, and a rebuild sets it to `C`. The floor is optional, and a migration
writes none. An era without one, whether from genesis setup or from today's
procedure, drains from the first input after its `base_safe_block`, as today.

`base_safe_block` stays `C`. The baseline has judged every batch through `C`,
so acceptance scans only blocks after it
([replay boundaries](../cockroach.md#replay-boundaries)), and no batch
at or below `C` is judged again. The directs from `F'` are not yet executed;
the floor covers them.

A baseline this rebuild built is the canonical state at the end of `C`: the
machine's state plus `T[X_m..K)`, with the queued directs still queued. So the
sequencer serves it on `/finalized_state` as the era's first comparison
checkpoint, at `C`, as it serves genesis at block 0, and the watchdog compares
it on its next tick. A baseline from today's procedure holds the terminal
drain and stays a restore artifact, as today; an era with no floor recorded
marks it.

Today's procedure instead executes the queued directs during recovery (the
terminal drain) and publishes the result, and each client repeats that drain
(projection replay, a contract since deleted). That state is a
prediction: the canonical machine reaches it only inside the post-`C` input
that drains the last of the queued directs, so no comparison can check it, and
a rebuild that started from it would execute those directs twice.

## Rules for the next template

Each changes the guest, so each needs Bart's agreement and ships with the next
template, ours and his port's
([register](../../review/register.md#canonical-rules-open-for-the-c-port)). The
rebuild relies on the `scheduler` query and on a fridge that only pops its
front, as today's does. The floor gate serves applications, not recovery.

- **Fridge bound.** Only well-formed directs enter the fridge. The rest are
  dropped: they never execute and never count, where today every direct
  counts. The application decides well-formedness with a function on its
  trait that reads only the direct and the application's fixed configuration,
  such as the wallet's portal and token addresses, never state that
  transactions change; Bart's guest calls libdex's own. The sequencer's lane
  drops the same directs with the same function, or it diverges. A count
  bounds the fridge, and an overflow drains just enough of its front to make
  room, raising `L` to the youngest drained direct's block. The sequencer
  cannot mirror an overflow, so the batches it already sealed diverge, and a
  manual cockroach recovery answers that.
- **The floor gate.** The scheduler keeps `L`. A new gate, after gate e,
  rejects a batch whose first frame lies below `L`; empty batches stop at gate
  c and never reach it. So a transaction's block never goes backwards along
  `T`, and applications may read blocks as time. The gate makes
  [I3](../../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine)
  consensus-critical: every transition that opens a Tip, the sequencer's open
  batch, must assert that it starts at or above the latest valid frame.
  `scheduler_accepts` leaves the floor out, as it leaves out gate d
  ([I1](../../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)).
- **A `scheduler` inspect query** returning the machine's position, batch
  nonce, fridge start and clock. The position is the last input's index plus
  one, batches included, or zero for the template. The fridge start is the
  oldest queued direct's index, or the position when the fridge is empty, so
  the guest's fridge keeps each direct's index.
- **Progress is the count alone.** `last_executed_safe_block` leaves the
  shared progress contract, the C engine ABI's progress and the wallet's
  comparison bytes. Nothing reads it in this design: the count positions
  states, and `L` is the scheduler's clock. Today two paths read it, as a seed
  bound: `setup --recovery` and a client's projection replay. Both leave with
  the rebuild.
- **Direct inputs carry what libdex reads.** A user op never sees its batch's
  context. Whether a direct's input index, timestamp and prevRandao reach the
  application waits on Bart's answer; the template passes exactly the fields
  libdex reads. The index and timestamp cost nothing, since the sequencer's
  input table and the feed's direct rows carry them. prevRandao needs a column
  and a feed field, and the block proposer can bias it, so it comes only with a
  stated need.

Inclusion stays as today: the application's validation decides it, fee
balance included, through the shared boundary that the canonical scheduler and
live inclusion both use. The rebuild executes with the machine's full state,
so it includes exactly what the canonical machine included.

## Preconditions

- **The old instance stays down.** Recovery deliberately creates a second data
  directory pinned to the same submitter address, the old one can still run
  with the same key, and
  [the flush fences transactions, not processes](../cockroach.md#incident-playbook).
  No command that signs with the key may run against the old directory again
  ([restart policy](../../watchdog/operator-deployment.md#sequencer-restart-policy)).
- **The submitter key survives.** The rebuild signs the flush and `run` submits,
  so a lost or leaked key means a new deployment while the submitter is an
  account compiled into the template.
- **Recovery repairs only the sequencer's view.** Whatever the canonical
  machine executed stays, a faulty sequencer's accepted batches included. A dead
  canonical machine is unrecoverable; PR #56 makes gate d reject an
  out-of-range `fee_price`, so on templates built from it that sequencer bug
  becomes a divergence recovery can repair.
- **Consumers execute bit for bit.** Our scheduler reproduces the guest's,
  which the conformance vectors test. The sequencer and every client run the
  canonical validation and execution; Bart's client embeds libdex exactly as
  the guest runs it and can produce its comparison bytes.
- **The template tie.** The sequencer runs the same application code as the
  template, and the template never changes, so a sequencer release that fixes
  that code would disagree with the canonical machine. Setup would pin the
  application's `getTemplateHash()`, and `run` would refuse a template other
  than the one its release declares.
- **Detection.** The watchdog's divergence latch tells the operator that the
  sequencer disagrees with the canonical machine, after a bug or a fridge
  overflow alike.

## What changes from the current plan

Rows marked *settled* reopen a decision the owner settled on 2026-10-29 or
2026-11-02.

| Current plan | This candidate | Why |
|---|---|---|
| An application-free tool emits candidates; the sequencer executes them (*settled*) | The rebuild runs the scheduler with the application and records what executes | Only the sequencer executes the tool's output, and it holds the application anyway |
| The tool reads L1 by block range through its own gateway, with a completeness check that needs an archive node for an old base | The rebuild reads the fresh data directory's input table, which the L1 reader proves complete | Setup already syncs that table; the second reader duplicated it |
| The fridge's start found by block: strict frame clocks, whole-block overflow, the clock raised by every drain, the bound holding a block (*settled*) | The machine's `scheduler` query also reports its fridge start, an index, so the guest's fridge keeps each direct's index; `L` is raised only by an overflow | Reading by index needs none of those rules: the fridge only pops its front |
| An overflow drains whole blocks (*settled*) | It drains just enough, as settled on 2026-10-29 | Whole blocks served only the block-based fridge start |
| The direct floor at the block `L'` (*settled*) | The index `F'` | The same reason; it is the floor chosen on 2026-10-29 |
| Separate steps joined by files, with a commit point | One command, one transaction, as `setup --recovery` is today | Removes a file format, a crash window and a step to practice |
| A base per consumer (*settled*); clients vouched by a log of watchdog agreements with era and generation, a rule for which agreement is on a client's history, a crossed-base record and count zero before the first agreement | One base for every consumer, at or below every client's count; each client checks its digest against the machine | Each client is checked directly against one machine, trusted by its source, so no agreement log is needed; the check also tests the client's own execution |
| Dave's last finalized machine is the default base (*settled*) | Open | With one base, Dave's lag becomes a stretch every client rewrites from the list |
| `incident/canonical` as a base for a deployment without clients | Dropped | One rule for the base: it predates the fault |
| A list entry carries its count and what the canonical application reads, but no batch nonce and no transaction hash | A row is a feed row without `batch_nonce`: a direct's row carries its input index, timestamp and transaction hash. The file opens with a header: era, `X_m`, the machine's digest, `K` and the baseline's digest | A client applies and stores list rows exactly as it does feed rows, and checks its count and digests against the header |
| A client executes its part, skipping an op its state refuses | A client applies rows and stops at the first refused row | Every row executed canonically, so a refusal means a wrong base, list or engine |
| Replaying the watchdog's head to `C` is optional; the first comparison checks the rebuild a batch later | A check step before `run`, always in drills | Catches a wrong list or scheduler state before any client realigns |
| Authorization decides inclusion (*settled*) | Dropped; inclusion stays as today | Neither design needs it, since the application decides inclusion in both. It adds a halt path (the wallet's apply errors on an unpayable fee), and because it caps the fee at the balance, a faulty sequencer could include ops from unfunded senders and make libdex store their nonces at no cost |
| The effective block kept apart from `L` in the progress contract (*settled*) | Dropped from the contract | Nothing reads it; the count positions states and `L` is the scheduler's clock |
| The well-formedness rule ported into our scheduler library | A function on the application's trait, reading only the direct and fixed configuration | The rebuild has the application at hand; no port to keep in sync |
| Dropped directs: what the lane does is not stated | The lane drops them with the same function | Otherwise every dropped direct diverges the count |
| Direct context with prevRandao (*settled*) | The fields libdex reads; prevRandao only on need | A template's libdex is fixed, so a field it does not read is pure cost; prevRandao needs a column and a feed field, and the proposer can bias it |
| The import opens a root at `C`; a late start needs Tip repair | Publishing the era opens no root; `run` opens it at its own safe head | Publishing becomes genesis setup with other numbers |
| The rebuilt baseline is only a restore artifact | The era's first comparison checkpoint, at `C`, when this rebuild built it | Without the terminal drain it is the canonical state there |
| "Refuse if a submitter input above `C` is already ingested", in the flush step | Genesis setup's detection gate, at `C` | One existing gate instead of a new rule |
| The flush as a standalone subcommand | A step of the rebuild command | The key is needed by `run` on the same directory anyway |
| Retention by position, anchored on the client's base | Retention sets recovery time; the digest check guards correctness | A pruned snapshot costs time, never a wrong base |
| `/history`'s baseline and accepted checkpoint reshaped or retired with the new sync | Deleted when the rebuild lands | Nothing reads them in this design |

Kept as they are: the split at the machine's position, the floor gate,
fencing, the best-effort flush, backup rules 1 and 2, and the executed list
delivered as a file.

## Decisions for the owner

| Decision | Recommendation |
|---|---|
| Default machine | Open; this reopens the settled choice of Dave's machine. The watchdog's head is fresh and ends a block, so the list is short; it is trusted through the operator's watchdog. Dave's last finalized machine is anchored on chain but lags, so every client rewrites that lag from the list, losing the feed's batch nonces there |
| One base for every consumer | Adopt; this reopens the per-consumer bases settled on 2026-10-29, and settles the open choice of a client's base |
| Fridge start | The index the machine reports, in place of the block rules settled on 2026-11-02 |
| Authorization decides inclusion | Drop it, unless its semantics are wanted for their own sake: an included op always advances the nonce and pays what it can. Keeping it needs Bart to list every reason libdex refuses an op besides its nonce or an account binding, since each would become a charged business failure |
| Progress is the count alone | Adopt with the next template |
| Direct context | Wait for Bart's answer; prevRandao only on a stated need |
| Template tie | One template per release, enforced at setup and `run` |
| Shared gate function, `trait Scheduler` | Defer, as before |

## Questions for Bart

- Does libdex's drive hold only its flat buffer, or a padding tail too? Does
  that buffer hold the count?
- Can your client produce its comparison bytes: libdex's state exactly as the
  guest's drive holds it, zero tail included?
- Can your guest answer the `scheduler` query: its position, batch nonce,
  fridge start and clock?
- Does libdex read a transaction's block as time, for example to expire
  orders? That is the floor gate's reason.
- Which of a direct's index, timestamp and prevRandao does libdex read?
- Is your guest's compiled sequencer address the pinned submitter? Can its
  harness or scheduler answer an input with `RX_REJECTED`?
- Does your scheduler reproduce the conformance vectors, and reject an
  out-of-range `fee_price` at gate d, as ours does since PR #56?
- Which directs are well-formed for libdex, and how large can one be?
- Can your client follow the two backup rules? How large is its database,
  which SQLite version does it link, and does applying a row read anything
  besides the database and the row?
- What are libdex's per-op cycles and state size? Who fixes libdex during an
  incident?

## Measure before choosing

- The rebuild's replay from the base's position to `C`, at the watchdog head's
  age and at the production Dave lag.
- The guest's cost to drain a full fridge in one input, which the first frame
  after an outage and the root frame after a rebuild both do.
- A client's restore to `X_m` and its digest over a large state.
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

## Remaining work

- The owner's decisions above; the template rules wait on Bart's answers.
- Rebuild: generalize `fold_replay` to the two phases, add the clock to
  `Scheduler::resume_at`, carry each direct's index through the fridge, drop
  the terminal drain, record each executed transaction with its count, refuse
  a base that does not fit the table, and write the list. Fold the flush, the
  sync, the detection gate and the publication into one command that prints
  the baseline's digest and scheduler values.
- Sequencer: the lane drops directs the well-formedness function refuses,
  before storage gives them a count; the era's direct floor at the index `F'`,
  read where the lane finds the next undrained input; publishing the era
  without opening a root; the rebuilt baseline served as the era's first
  comparison checkpoint, only in an era that records a floor; and the floor
  gate in I1's documented omissions, with I3's assert at every transition that
  opens a Tip, which I3 lists under what enforces it. Draining from `F'`
  replaces the terminal drain, so rewrite what pins that drain today: the
  projection replay steps, the README's nonce note, the history guide's era
  baseline, snapshot lifecycle, invariants I6, I8, I11 and I20, and the
  rebuild drill's replica check.
- Guest, for the next template: the rules above, the `scheduler` query, the
  well-formedness function on the application's trait and the C binding,
  progress as the count alone (with I19, the application contract's §3, the
  history guide's progress facts and the snapshot format), and new conformance
  vectors.
- Clients: the two backup rules, the digest checks at `X_m` and at `K`, and a
  list driver that stops at the first refused row.
- Watchdog: `export-state`, which writes a machine's state-source bytes, its
  comparison digest and the values its `scheduler` query reports, and nothing
  application-specific.
- Dave's machine, if it is chosen as the base: read its last finalized root
  from the consensus contract, which nothing in this repository reads yet.
- Threat model: what losing or leaking the submitter key means, and that a
  revoked user op is not cancelled: anyone holding it can submit it again while
  its sender's nonce is unused, and an application that wants expiry checks a
  deadline in the payload against the op's `safe_block`.
- Drills that rebuild from a machine with directs queued at `C` and realign a
  toy-wallet client, in the [incident runbook](../../watchdog/incident-runbook.md#drills),
  on the devnet in CI. A path run once in a deployment's life rots.
- Then the deletions the
  [tracks board](../../plans/2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
  lists for the tool's landing, plus `/history`'s baseline and accepted
  checkpoint, which nothing reads in this design, and `setup --recovery`.
