# Scheduler acceptance semantics

The canonical scheduler turns an application's L1 inputs, in InputBox order,
into one sequence of L2 transactions, and the application executes them. Every
validator runs it inside the canonical machine; the sequencer predicts it while
it sequences. This page specifies it for anyone who reimplements it. The
reference is the concrete [`Scheduler`](../../sequencer-core/src/scheduler/mod.rs),
which decides the sequence, with [`application::execute`](../../sequencer-core/src/application/mod.rs)
running each transaction it hands out.
[`CanonicalRunner`](../../sequencer-core/src/scheduler/driver.rs) joins the two,
and the [canonical-app harness](../../examples/canonical-app/src/scheduler/mod.rs)
runs it in the machine. This text is the contract and the reference can have
bugs, so report any disagreement. The [application contract](application-contract.md)
owns what the application does with each transaction. Rules marked **Open**
are still to settle with the C++ port's author
([register](../review/register.md#canonical-rules-open-for-the-c-port)):
changing one changes the canonical machine image or its wire format.

## The one stream, the one classification

An **L2 transaction** is a direct input or a user op. The scheduler builds the
sequence one L1 input at a time:

1. **Every input first runs the backstop.** Directs queued for at least
   `MAX_WAIT_BLOCKS` execute, oldest first, even when the input is a batch the
   gates then reject.
2. **A direct input** joins the *fridge*, the queue of directs not yet
   executed, unless its payload is over the cap. A full fridge first drains
   its oldest blocks to make room ([the fridge](#the-fridge)).
3. **A batch** is accepted or changes nothing ([batch validity](#batch-validity)).
4. **Each frame of an accepted batch** first executes every queued direct up
   to its safe block, oldest first, then hands out its user ops in order.
5. **A user op executes or is skipped** ([a user op's fate](#a-user-ops-fate)).

Each input is a Cartesi Rollups `EvmAdvance` from the InputBox. The scheduler
reads its `msgSender`, its `index`, its `blockNumber` (the *inclusion block*),
its `timestamp`, its `chainId` and `appContract` (the signature domain), and its
`payload`. It classifies by sender only, never by payload content:

- `msgSender == sequencer_address`: the payload is a **batch**.
- Any other sender: a **direct input**.

`sequencer_address` is deployment data compiled into the canonical machine. It
must equal the batch-submitter address that `setup` pins, and nothing checks
this at runtime (a setup check is **Open**). If they differ, the machine treats
every batch as a direct input while the sequencer still reports its batches
accepted, so only a watchdog comparison reveals it.

### What counts

The application's **L2 tx count** is how many L2 transactions it has
executed. It names a state and the transaction that state executes next, so
the feed numbers its rows with it. It counts transactions, not L1 inputs.

| Outcome of a user op | What happens | Counts |
|---|---|---|
| **Skipped** | Nothing: no nonce, no fee, no effect. | No |
| **Executed, succeeded** | The sender's nonce advances, the fee is charged and the effect applies. | Yes |
| **Executed, failed** | The sender's nonce advances and the fee is charged, but the effect fails, such as a transfer above the balance. | Yes |

An op whose sender cannot pay the whole fee executes too. Every executed direct
counts, whatever the application does with it. Nothing else counts: not a
queued or dropped direct, not a skipped op, not a batch envelope, not anything
in a rejected or stale batch. A rejected or stale batch can still advance the
count through the backstop.

### The scheduler's state

| Field | Meaning |
|---|---|
| `l1_input_count` | How many InputBox inputs the scheduler has consumed: its position. |
| `next_batch_nonce` | The nonce the next accepted batch must carry; `u64`, 0 at genesis. |
| `clock` | The latest block the scheduler has executed at; 0 at genesis ([scheduler clock](#the-scheduler-clock)). |
| fridge | The admitted directs not yet executed, oldest first, each with its input index. |

Nothing else carries over between inputs. **The scheduler's state never reads
the application's.** The nonce, the clock and the fridge depend only on the
inputs; the application affects them only by halting the machine, and whether
it executes a user op changes none of them. The scheduler runs no application
code either: the fridge admits by sender and payload length, and the nonce
check belongs to the executor. A port must keep this property, which lets the
scheduler run apart from the application.

**The fridge is a queue.** Directs join at the back in input order and leave
only from the front, whichever rule drains them. So it always holds exactly the
admitted directs from one input index, the *first queued input*, up to the L1
input count.

The two counts differ, because a batch envelope or a queued direct is an L1
input but not an L2 transaction. The application knows its L2 tx count, and a
client knows only that one; the machine knows both
([state export](#state-export)).

### The invariant

The [scheduler clock](#the-scheduler-clock) gives two properties after every
input:

- **Blocks never go backwards** along the L2 sequence. Gate f gives this.
- **The clock splits the directs.** Every admitted direct at or below the clock
  has executed, and every one above it is still queued. Strict frames, the
  backstop's raise and whole-block overflow drains give this.

The split gives the clock a meaning an application can use: at clock `c`, it
has applied every direct up to block `c` and none after.
[`Scheduler::resume`](../../sequencer-core/src/scheduler/mod.rs) refuses an
exported state whose queued directs break it.

## Wire format

A batch payload is the SSZ encoding of `Batch`
([`batch.rs`](../../sequencer-core/src/batch.rs)), fields in this order:

```text
Batch      { nonce: uint64, frames: List[Frame] }
Frame      { user_ops: List[WireUserOp], safe_block: uint64, fee_price: uint16 }
WireUserOp { nonce: uint32, max_fee: uint16, data: List[uint8], signature: List[uint8] }
```

Integers are little-endian. Each variable-size field has a 4-byte offset in its
container's fixed part, and the variable parts follow in field order. A list of
containers starts with one 4-byte offset per element. Lists have no length
limit. Decoding is strict: offsets must start right after the fixed part, never
decrease and stay in bounds, so only the canonical encoding of a `Batch`
decodes, and re-encoding returns the same bytes.
`max_fee` and `fee_price` are fee exponents ([constants](#constants)). A
`signature` of any length decodes, but only 65 bytes can recover a sender; a
fixed 65-byte field is **Open**.

## User-op signatures

A user op signs the EIP-712 struct `UserOp(uint32 nonce,uint16 max_fee,bytes data)`
([`user_op.rs`](../../sequencer-core/src/user_op.rs)) under the domain
`EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)`:
name `CartesiAppSequencer`, version `1`, the input's `chainId`, its
`appContract` as `verifyingContract`, and no salt
([`build_input_domain`](../../sequencer-core/src/lib.rs)). The sender is not
signed; it is recovered from `r ‖ s ‖ v` (alloy's `Signature::from_raw` and
`recover_address_from_prehash`), and any failure skips the op:

1. The signature must be 65 bytes, and `v` must be 0, 1, 27, 28 or at least 35.
   The y-parity is `v` for 0 and 1; for `v ≥ 27` it is 1 when `v` is even,
   else 0.
2. `r` and `s` must lie in `[1, n − 1]`, where `n` is the secp256k1 order.
3. If `s > n / 2`, `s` becomes `n − s` and the parity flips, so a high-`s`
   signature and its low-`s` twin of opposite parity recover the same sender.
4. The sender is the address of the public key recovered from `(r, s, parity)`
   and the EIP-712 signing hash.

This acceptance set is the library's, pinned on both the batch path and
`POST /tx` by the signature vectors, and for `s = n` and `v` from 29 to 34 on
the batch path by the guest scenarios ([conformance](#conformance)).
Narrowing it to `v ∈ {27, 28}` and low `s` is **Open**. Batches the sequencer
builds carry `v ∈ {27, 28}`, but keep a user's `s` as signed.

## Processing an input

1. **The backstop.** Pop and execute directs from the front of the fridge
   while `input.inclusion_block − front.inclusion_block ≥ MAX_WAIT_BLOCKS`,
   raising the clock to each one's block. This bounds how long a sequencer
   that stops submitting, or freezes `safe_block`, can hold a direct back.
2. **A direct** joins the fridge, unless its payload is over the cap, and once
   there is room ([the fridge](#the-fridge)). The cap comes first: a direct
   that never joins causes no overflow drain, though the backstop has run.
3. **A batch** runs the gates below.

### Batch validity

The first gate that applies decides the outcome:

| # | Gate | Outcome | `next_batch_nonce` |
|---|---|---|---|
| a | The payload does not SSZ-decode as `Batch`. | Rejected | Kept |
| b | `batch.nonce != next_batch_nonce` | Rejected | Kept |
| c | `frames` is empty. | Accepted; executes nothing | +1 |
| d | The frame structure is invalid (below). | Rejected | Kept |
| e | `inclusion_block − frames[0].safe_block ≥ MAX_WAIT_BLOCKS` | Skipped as stale | Kept |
| f | `frames[0].safe_block < clock` | Rejected | Kept |
| g | Otherwise: execute every frame. | Accepted | +1 |

Gate d requires, for every frame:

- its `safe_block` strictly below `inclusion_block`;
- its `safe_block` at or above the previous frame's;
- its `fee_price` at most `MAX_EXPONENT`.

Gate d checks every frame before any frame executes; the backstop has already
run. Ages saturate at zero. An empty batch is accepted at any age, since it has
no first frame; rejecting empty batches is **Open**. A rejected batch and a
stale one have the same effect: no state change, nonce kept. So every later
batch fails gate b until one reuses that nonce, as a batch rebuilt after a
recovery does. The reject reason the reference reports is diagnostic.

### Frame execution

For each frame of an accepted batch, in order:

1. **Drain covered directs.** Pop and execute directs from the front of the
   fridge while their `inclusion_block ≤ frame.safe_block`. The `safe_block` is
   the sequencer's claim that it accounted for every direct up to that block;
   the scheduler enforces the drain whatever the sequencer did. Then raise the
   clock to `frame.safe_block`.
2. **Hand out the user ops** in order. The scheduler skips an op whose
   `max_fee` is below `frame.fee_price` or whose sender cannot be recovered
   ([signatures](#user-op-signatures)); both leave no trace, so their order is
   unobservable. The executor then reads the sender's next nonce from the
   application and skips the op, checked in this order, when its nonce is not
   that one, or when that one is `u32::MAX`, which has no successor.

   Otherwise the op executes with fee `frame.fee_price` and block
   `frame.safe_block`: the application advances the sender's nonce and the
   L2 tx count by one each, and the executor checks both.

The application stores each sender's next nonce and reports it; shared code
compares it with the op's, so no application code decides which ops execute
([application contract](application-contract.md#user-nonces)).

### A user op's fate

Two parties decide, and they ask different questions:

- **The canonical scheduler asks whether the sender authorized the op.** A
  check that protects the user is consensus: the signature, the nonce and the
  fee cap. An op that fails one is **skipped**, with no trace.
- **The sequencer's lane asks whether the operator wants the op.** A check that
  protects the operator is admission: can the sender pay, does the payload fit
  the batch, does the operator's policy allow it. An op that fails one is
  **refused** at `POST /tx`. The canonical scheduler never asks: the fee is
  owed to the sequencer, and only the sequencer includes ops.

| An op that… | Checked by | Canonical scheduler | Sequencer's lane |
|---|---|---|---|
| has no recoverable sender | The scheduler | Skipped | Refused |
| caps its fee below the frame's price | The scheduler | Skipped | Refused |
| carries a nonce that is not its sender's next | The executor, from the application's stored nonce | Skipped | Refused |
| comes from a sender whose next nonce is `u32::MAX` | The executor | Skipped | Refused |
| comes from a sender who cannot pay the fee | The application's admission | Executed | Refused |
| exceeds the application's declared payload bound | The lane | Executed | Refused |
| breaks an operator policy | The application's admission, or the lane | Executed | Refused |
| fails in the application, such as a transfer above the balance | The application's execution | Executed | Admitted |
| is sponsored: the application charges it nothing | The application's execution | Executed | Admitted |
| is none of the above | | Executed | Admitted |

- **Skipped** means nothing changes: no nonce, no count, no fee.
- **Executed** means the sender's nonce and the L2 tx count each
  advance by one. What the op pays and whether its effect applies are the
  application's choice ([application contract](application-contract.md#the-execution-methods)).
  The application must not fail, since an application error halts the machine.
- **The lane refuses everything the scheduler skips**, by running the same
  checks first ([`check_live_user_op`](../../sequencer-core/src/application/mod.rs)).
  No op is admitted and then skipped.

**An underfunded op executes.** Whether a sender can pay protects only the
sequencer from its own inclusion, so the lane checks it and consensus does not.
Which transactions execute then depends on signatures, fee caps and nonces
alone, never on a balance: a fault in the application's logic can change what
a transaction does, but not which transactions execute or what their counts
are.

**A fee cap below the price skips the op.** The cap is part of what the sender
signed: execute this op only at a price up to the cap. Charging the price would
take more than the sender authorized, and executing at the cap would make a
frame's price mean something else for one op. The sequencer loses nothing: it
sets each frame's price, so it can put the op in a frame it fits. A skipped op
stays valid until its sender's nonce moves past it.

**A sponsored op needs no rule of its own.** Whether an executed op pays is the
application's choice, and whether an unpaid op is wanted is admission's. An
operator can run its own price feed for nothing by funding the feed's account
where fees are credited to the operator, as the wallet does; by exempting the
role in the application; or by putting its ops in a frame priced at exponent 0.
A sponsored sender still signs a cap at or above the frame's price, and an
exemption must hang on something the signature proves, the sender's role, or
anyone can use it.

## The scheduler clock

The clock is the latest block the scheduler has executed at: an accepted
frame's safe block, or an executed direct's inclusion block. Every frame of an
accepted batch raises it, and so does every executed direct, whether a frame,
the backstop or an overflow drained it. It only rises: a raise takes the later
of the clock and the new block. Gate f rejects a batch whose first frame lies
below it, and gate d orders the rest.

**Gate f alone keeps blocks from going backwards.** Every frame lies at or
above the clock, so a frame's directs come from the clock or above and at or
below its safe block, and its user ops see the safe block. The backstop needs
no help: a batch whose first frame lies below a block the backstop drained is
already stale by gate e.

Three more rules keep the clock's [split of the directs](#the-invariant):

- **Strict frames** (gate d) let a frame drain every direct of its own safe
  block, because all of them landed before its batch. With equality allowed, a
  direct from that block landing after the batch would stay queued at the
  clock. Equality is also a claim no sequencer can make honestly: the block
  was not finished when the batch was sealed.
- **The backstop raises the clock**, as every drain does, so no direct it
  executed sits above the clock. The raise changes no outcome: gate e already
  skips any batch it would reject.
- **Overflow drains whole blocks**, so no direct at or below the clock stays
  queued ([the fridge](#the-fridge)).

Unless an overflow overtakes it, a correct sequencer never trips gate f or the
strict rule: it never opens a frame below an earlier one, and it opens frames
only at safe heads it has already seen, so its batch lands in a later block.
Its own frame ordering ([I3](../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine))
is consensus-critical: a bug there gets batches rejected.

## The fridge

- **Admission.** A direct whose payload is longer than the cap
  (`max_direct_payload_bytes`) never joins: it never executes and never counts.
  Every other direct joins, whatever it holds, and the application ignores what
  it cannot read. The sequencer's lane drops the same directs by the same
  constant, or the two diverge.
- **Why a constant.** A count bounds memory only if each entry's size is
  bounded too: portals accept zero-value deposits with extra data up to the
  InputBox's 64 KiB cap. A constant bounds the size without the scheduler
  running application code. A filter written by the application would also
  drop small garbage, but a zero-value portal deposit takes a slot for about
  the same L1 gas, so it would buy little.
- **Entries.** Each entry keeps the direct as delivered, with the
  [direct context](#direct-context) the application receives.
- **The bound.** The fridge is full when it holds `fridge_bound` entries or
  more. When a direct arrives at a full fridge, the scheduler executes whole
  blocks from the front, oldest first, until there is room, raising the clock
  to each drained block. It never drains the arriving direct's own block: if
  only that block's directs remain, the direct joins over the bound. The bound
  is then soft by at most one block's directs, and memory stays bounded by what
  one L1 block can carry.
- **An overflow can reject sealed batches.** The sequencer cannot predict an
  overflow, and a batch it sealed earlier then lands in one of two ways. If its
  first frame is at or above the last drained block, it is accepted, and the L2
  sequence is the one the sequencer predicted: the drained directs are the
  first ones that frame would have drained. If it is below, gate f rejects the
  batch, and every later one fails the nonce gate. The sequencer's acceptance
  check does not see the clock, so it counts those batches as accepted and
  diverges; the watchdog's divergence latch reports it, and a manual recovery
  answers it. An overflow also moves which input executes the drained directs,
  which the watchdog's comparison at that block can see.

Both constants belong to the application's template
([`Application::scheduler_limits`](../../sequencer-core/src/application/mod.rs)).
Their values for the wallet are provisional, and the values and the largest
drain the guest can afford are **Open**
([register](../review/register.md#bound-the-canonical-direct-input-fridge)).

## Direct context

A direct executes with its inclusion block as its block, its input index and
its timestamp ([`Direct`](../../sequencer-core/src/l2_tx.rs)). The sequencer's
input table and the feed carry both. prevRandao is not passed: it would need a
column and a feed field, and the block proposer can bias it, so it comes only
on a stated need. A user op never sees its batch's context; it sees its frame's
safe block and fee.

## Outputs and the L2 tx count

An input's outputs are the application's notices and vouchers in execution
order: the backstop's directs first, then, for a direct, any overflow drain,
or, for a batch, each frame's drained directs and executed ops. Reports are
diagnostics outside the outputs tree: the reference reports a rejected batch
with one fixed string and a stale skip not at all.

The L2 tx count advances by one for each executed direct and each
executed user op, and for nothing else ([what counts](#what-counts)). The
[application contract](application-contract.md#4-the-l2-tx-count-l2_tx_count)
owns the count; the [history contract](application-history.md) owns how
catch-up, recovery and the API use it.

## Fatal conditions

The guest never answers an input with `RX_REJECTED`. A batch a gate rejects is
still accepted at the machine level. A revert would undo the whole input and
roll the scheduler's state back with the application's, so the scheduler's
state would depend on the application. It would also drop the backstop's
executions, which the reference keeps.

The reference halts the machine when the application returns an error; when
the batch nonce, the L2 tx count or the L1 input count has no `u64`
successor; and when an input's `index`, `blockNumber`, `timestamp` or `chainId`
does not fit `u64`. A halt is a permanent fixed point, not a reject
([rollup host semantics](../cartesi-machine.md#rollup-host-semantics)), so a
port must halt on the same inputs. No canonical successor state exists.

## State export

The machine answers two inspect queries, so a reader can take its whole state:

| Query | Report |
|---|---|
| `state`, or an empty payload | The application's comparison bytes, whole ([application contract](application-contract.md#6-checkpoint-lifecycle)). |
| `scheduler` | The scheduler's state and the L2 tx count, 48 bytes. |

The `scheduler` report is
[`SchedulerExport`](../../sequencer-core/src/scheduler/mod.rs): six
little-endian `u64`s, in this order:

1. `l1_input_count`, the machine's position;
2. `l2_tx_count`, the application's L2 tx count;
3. `next_batch_nonce`;
4. `clock`;
5. `first_queued_input`, the oldest queued direct's index, or the L1 input
   count when none is queued;
6. `fridge_len`, how many directs are queued.

The queued directs themselves are not exported. [The fridge is a
queue](#the-schedulers-state), so the first queued input and the L1 input count
name them, and L1 holds their bytes.
[`Scheduler::resume`](../../sequencer-core/src/scheduler/mod.rs) rebuilds a
scheduler from the report and those inputs, and refuses a range that does not
match it. The harness answers the `scheduler` query for every application. Any
other query, or an application failure while encoding its state, gets a report
saying so, never a halt.

`setup --base` reads a stored machine's export through these two queries
([`sequencer::machine`](../../sequencer/src/machine.rs)), checks it against
its input table, and resumes the scheduler from it
([what setup does](../recovery/cockroach.md#what-setup-does)).
A C or C++ guest reaches the reference scheduler through
[`scheduler.h`](../../bindings/c-scheduler/include/scheduler.h), whose
`scheduler_state` returns the same fields but the L2 tx count, which the
application reports.

## Constants

| Constant | Value | Home |
|---|---|---|
| `MAX_WAIT_BLOCKS` | 1200 L1 blocks, about 4 h | [`lib.rs`](../../sequencer-core/src/lib.rs) |
| `MAX_EXPONENT` | 17101 | [`fee.rs`](../../sequencer-core/src/fee.rs) (`build.rs` generates it) |
| `fridge_bound` | 16384 entries for the wallet, provisional | the application's template |
| `max_direct_payload_bytes` | 1024 bytes for the wallet, provisional | the application's template |
| `sequencer_address` | per deployment | compiled into the canonical machine |

[`SchedulerConfig`](../../sequencer-core/src/scheduler/mod.rs) carries all but
`MAX_EXPONENT`, built for an application by `SchedulerConfig::for_application`.
The window `max_wait_blocks` is at least one block, which the reference
asserts: with zero, the backstop could drain the arriving input's own block and
leave a direct from that block queued at the clock.

A fee exponent `n` stands for `(129/128)^n` smallest token units. The
application converts it; [`fee_to_linear`](../../sequencer-core/src/fee.rs)
owns the bit-exact conversion. A checked-in lookup table in its place is
**Open** ([Track 5](../plans/2026-07-coordination-tracks.md#track-5--fee-exponentiation-lut)).

## Conformance

[`consensus_vectors_v2.json`](../../tests/fixtures/consensus_vectors_v2.json)
is this template's vector set. It is written once per template and never
regenerated: a mismatch is a consensus change to explain, not a reason to
update the file, and the next template ships a new file. It holds three kinds
of case:

- **Byte-level cases**, carried unchanged from the earlier template's set:
  EIP-712 hashes, keccak256, the signature cases above (every accepted `v`
  form, `v = 2`, high `s`, 64 bytes, `r = 0`, `s = 0`, `r = n`) with the sender
  each path recovers, SSZ bytes, the wallet's output payloads, fee conversions
  and `EvmAdvance` calldata.
- **Guest scenarios**, each replayed from genesis on the devnet wallet
  template. A step is one input (sender, block, timestamp, payload) with its
  outcome, the transactions it handed out in order and whether each user op
  executed, its exact outputs, and the `scheduler` report after it; a scenario
  ends with the `state` report. They restate the earlier template's scenarios
  under strict frames, and cover every gate (gate d on a later frame as well as
  the first), the clock, the backstop (before a dropped direct too), the
  payload cap, each fate of a user op, and `s = n` and `v` from 29 to 34 on the
  batch path. The `scheduler` report after every step makes every scenario
  assert that [the clock splits the directs](#the-invariant).
- **Scheduler-only scenarios**, under their own configuration with a fridge
  bound of 2, which the wallet's bound of 16384 keeps the guest scenarios from
  reaching. They pin the overflow drain: whole blocks, oldest first, never the
  arriving block, and the clock it raises. Resume cases pin which exported
  states and re-supplied ranges resume, and how a resumed scheduler continues.

The skip for a sender whose next nonce is `u32::MAX` has no scenario, since
reaching it from genesis takes 2^32 ops; the executor's unit tests in
[`application/mod.rs`](../../sequencer-core/src/application/mod.rs) hold it,
and a C engine's caller must apply it too.

The expected values came once from the reference, `CanonicalRunner` over the
wallet for the guest scenarios and the `Scheduler` alone for the rest, and four
checks hold them:

| Check | Where | What it compares |
|---|---|---|
| Native, guest | `the_reference_reproduces_every_guest_scenario` in [`wallet.rs`](../../examples/app-core/src/application/wallet.rs) | Every field of every step and the final state, and the clock's split from the pinned data alone. |
| Native, scheduler only | `the_reference_reproduces_every_scheduler_only_scenario` in [`scheduler/mod.rs`](../../sequencer-core/src/scheduler/mod.rs) | Every field of every step, and each resume's result and refusal reason. |
| In the machine | The golden tests in [`golden.rs`](../../examples/canonical-test/src/golden.rs), on the built image | The guest scenarios' outputs, whether a reject report appears, and both inspect reports. |
| Through the header | [`conformance.rs`](../../bindings/c-scheduler/tests/conformance.rs) in `c-scheduler` | For both kinds of scenario, each input's outcome and reject reason, every handed-out transaction, the state after it, and whether each resume is refused. |

The last check is the one a C or C++ port of
[`scheduler.h`](../../bindings/c-scheduler/include/scheduler.h) is held to. It
calls only the header's functions and reads only the file, but it links this
crate's reference export, so today a port replays the file through the header
the same way in its own harness; packaging the check to link another
implementation is in the
[todo](../plans/2026-11-sequencer-recovery/todo.md#build). It compares the
reject reason because the header reports it, though no output depends on it.
The header has one status for every refused resume, so only the native check
compares the refusal's reason.

## Sequencer side

The rest of this page is for sequencer developers.

### Sequencer frame-clock policy

Gate d constrains frame clocks only within a batch: non-decreasing and no later
than its inclusion block. The scheduler does not require a frame for every L1
block. During an admitted live run, the sequencer advances logical frame time
when the latest persisted safe head `H` is at least five blocks beyond the open
frame clock `S`. It then opens exactly one frame at `H`, drains the complete
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

Five blocks is a sequencer policy chosen comfortably inside the happy-path
deposit budget. If actual safe-head publication cadence threatens that budget,
revisit the constant from measurements rather than adding interpolation or a
second timer.

### The three implementations and why they agree

The sequencer predicts this scheduler in two more places: the off-chain
predicate `scheduler_accepts`, which builds the gold frontier, and the
inclusion lane's live prediction.
[I1](../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)
maps all three and their deliberate differences: the predicate skips gates d
and f, a self-trust whose reason is recorded at
[`scheduler_accepts`](../../sequencer-core/src/protocol.rs). The lane shares
the reference's code where it can: it runs every transaction, live and on
replay, through `application::execute`; it refuses at `POST /tx`, through
`check_live_user_op`, every op the executor would skip; and it admits directs
by the application's payload cap, through one storage predicate that every
query enumerating directs to execute uses.
[I2](../invariants.md#i2-drain-attribution-accumulated-directs-land-in-the-clock-advanced-frame)
and [I3](../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine)
are the lane's mirrors of the drain order and of frame monotonicity, and
[I19](../invariants.md#i19-application-progress-follows-the-shared-execution-contract)
covers the shared execution boundary. Change one, check all.
