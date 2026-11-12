# The scheduler, for the next template

**Status:** implemented. The reference
[`Scheduler`](../../../sequencer-core/src/scheduler/mod.rs) runs these rules,
and [scheduler semantics](../../protocol/scheduler-semantics.md) is their
contract; read that page for the rules in force. This page stays until its
**Open** items settle with the port's author
([register](../../review/register.md#canonical-rules-open-for-the-c-port)),
then shrinks to what remains ([todo](todo.md#promote-to-real-docs)). **New**
marks a rule that changed from the earlier template.

The canonical scheduler turns an application's L1 inputs, in L1 order, into
one sequence of L2 transactions, and the application executes them. Every
validator runs it inside the canonical machine. The sequencer predicts it
while it sequences, and runs it on its own during a recovery. The reference is
the concrete [`Scheduler`](../../../sequencer-core/src/scheduler/mod.rs), with
[`application::execute`](../../../sequencer-core/src/application/mod.rs) running
each transaction it hands out. This text is the contract, and the reference can
have bugs, so report any disagreement.

## What changes

| Change | Why | Does recovery need it? |
|---|---|---|
| [State export](#state-export) | Setup starts a sequencer from a machine's whole state | Yes, and nothing else on this list |
| [The fridge's bound and payload cap](#the-fridge) | An unbounded fridge can exhaust the machine's memory, which halts the application for good | No; a recovery is only the remedy after an overflow |
| [Monotone blocks](#the-scheduler-clock) | A transaction's block never goes backwards, so applications may read blocks as time | No |
| [An underfunded op executes](#a-user-ops-fate) | Whether a sender can pay protects only the operator, so the lane checks it and consensus does not | No |
| [Shared code compares the nonce](#frame-execution) | No application code decides which ops execute | No |
| [Direct context](#direct-context) | The application learns a direct's input index and timestamp | No |
| [Progress is the L2 tx count alone](#outputs-and-progress) | The scheduler keeps the clock now, so the application need not | No |

Each change ships with the next template, ours and the port's, and with new
[conformance vectors](#conformance). A deployed application keeps its
template for life
([Cartesi Machine facts](../../cartesi-machine.md#versions-are-all-or-nothing)).

## The model

### The L2 transaction sequence

An **L2 transaction** is a direct input or a user op. The scheduler builds the
sequence one L1 input at a time:

1. **Every input first runs the backstop.** Directs queued for at least
   `max_wait_blocks` execute, oldest first, even when the input is a batch the
   gates then reject.
2. **A direct input**, from any sender but the sequencer, joins the fridge,
   the queue of directs not yet executed. **New:** one whose payload is over
   the cap never joins, and a full fridge drains its oldest blocks to make
   room.
3. **A batch**, the sequencer's input, is accepted or changes nothing
   ([batch validity](#batch-validity)).
4. **Each frame of an accepted batch** first executes every queued direct up
   to its safe block, oldest first, then offers its user ops in order.
5. **A user op is executed or skipped** ([a user op's fate](#a-user-ops-fate)).

### What counts

A state's **L2 tx count** is how many L2 transactions it has executed. It
names the state, and it names the transaction that state executes next, so
the feed numbers its rows with it. It counts transactions, not L1 inputs.

A user op has three outcomes, and only one of them leaves no trace:

| Outcome | What happens | Counts |
|---|---|---|
| **Skipped** | Nothing: no nonce, no fee, no effect | No |
| **Executed, succeeded** | The sender's nonce advances, the fee is charged, the effect applies | Yes |
| **Executed, failed** | The sender's nonce advances and the fee is charged, but the effect fails, such as a transfer larger than the balance | Yes |

An op whose sender cannot pay the whole fee executes too (**New**,
[a user op's fate](#a-user-ops-fate)). Every executed direct counts, whatever
the application does with it.
Nothing else counts: not a queued or dropped direct, not a skipped op, not a
batch envelope, not anything in a rejected or stale batch. This page avoids
"no-op": today's docs use it both for an op that counts and for an empty batch.

### The scheduler's state

| Field | Meaning |
|---|---|
| `next_batch_nonce` | The nonce the next accepted batch must carry; `u64`, 0 at genesis |
| `clock` | **New.** The latest block the scheduler has executed at; 0 at genesis ([scheduler clock](#the-scheduler-clock)) |
| `fridge` | The admitted directs not yet executed, oldest first; each keeps its input index (**New**) |

Nothing else carries over between inputs. **The scheduler's state never reads
the application's.** The nonce, the clock and the fridge depend only on the
inputs; the application affects them only by halting the machine. Whether the
application executes a user op changes none of them. The scheduler runs no
application code either: the fridge admits by sender and payload length. A
port must keep this property: it is what lets the sequencer run the scheduler
apart from the application during a recovery.

**The fridge is a queue.** Directs join at the back in input order and leave
only from the front, whichever rule drains them. So the fridge always holds
exactly the admitted directs from one input index, the *first queued input*,
up to the L1 input count.

### Two counts

| Count | Names | Who knows it |
|---|---|---|
| **L2 tx count** | A state, and the next L2 transaction | Every application state; a client knows only this one |
| **L1 input count** | A machine's position: how many InputBox inputs it has consumed | The machine |

They differ, because a batch envelope or a queued direct is an L1 input but
not an L2 transaction. The machine exports both
([state export](#state-export)).

### The invariant

**New.** The [scheduler clock](#the-scheduler-clock) gives two properties
after every input:

- **Blocks never go backwards** along the L2 sequence. Gate f gives this.
- **The clock splits the directs.** Every admitted direct at or below the
  clock has executed, and every one above it is still queued. Three more rules
  give this: strict frames, the backstop's raise and whole-block drains.

The split gives the clock a meaning an application can use: at clock `c`, it
has applied every direct up to block `c` and none after. Recovery does not
depend on it, since setup takes the first queued input from the machine
([state export](#state-export)). Two readers keep it true:

- **Setup checks it.** Among the inputs a base machine consumed, the admitted
  directs above its clock must be exactly the queued ones its export names
  ([setup](../../recovery/cockroach.md#what-setup-does)).
- **The conformance vectors assert it** after every input, through the
  `scheduler` report ([conformance](#conformance)).

## Inputs

Each input is a Cartesi Rollups `EvmAdvance` from the InputBox. The scheduler
reads its `msgSender`, its `blockNumber` (the *inclusion block*), its `chainId`
and `appContract` (the signature domain), its `payload`, and, for a direct,
its `index` and `timestamp` (**New**, [direct context](#direct-context)). It
classifies by sender only, never by payload content:

- `msgSender == sequencer_address`: the payload is a **batch**.
- Any other sender: a **direct input**.

`sequencer_address` is deployment data compiled into the canonical machine. It
must equal the batch-submitter address the sequencer's setup pins, and nothing
checks this at runtime (a setup check is **Open**). If they differ, the machine
treats every batch as a direct while the sequencer reports its batches
accepted, so only a watchdog comparison reveals it.

## Wire format

A batch payload is the SSZ encoding of `Batch`
([`batch.rs`](../../../sequencer-core/src/batch.rs)), fields in this order:

```text
Batch      { nonce: uint64, frames: List[Frame] }
Frame      { user_ops: List[WireUserOp], safe_block: uint64, fee_price: uint16 }
WireUserOp { nonce: uint32, max_fee: uint16, data: List[uint8], signature: List[uint8] }
```

Integers are little-endian. Each variable-size field has a 4-byte offset in its
container's fixed part, and the variable parts follow in field order. A list of
containers starts with one 4-byte offset per element. Lists have no length
limit. Decoding is strict: offsets must start right after the fixed part, never
decrease and stay in bounds, so only the canonical encoding decodes, and
re-encoding returns the same bytes. `max_fee` and `fee_price` are fee exponents
([constants](#constants)). A `signature` of any length decodes, but only 65
bytes can recover a sender; a fixed 65-byte field is **Open**.

## User-op signatures

A user op signs the EIP-712 struct `UserOp(uint32 nonce,uint16 max_fee,bytes data)`
([`user_op.rs`](../../../sequencer-core/src/user_op.rs)) under the domain
`EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)`:
name `CartesiAppSequencer`, version `1`, the input's `chainId`, its
`appContract` as `verifyingContract`, and no salt. The sender is not signed; it
is recovered from `r ‖ s ‖ v`, and any failure skips the op:

1. The signature must be 65 bytes, and `v` must be 0, 1, 27, 28 or at least 35.
   The y-parity is `v` for 0 and 1; for `v ≥ 27` it is 1 when `v` is even,
   else 0.
2. `r` and `s` must lie in `[1, n − 1]`, where `n` is the secp256k1 order.
3. If `s > n / 2`, `s` becomes `n − s` and the parity flips, so a high-`s`
   signature and its low-`s` twin of opposite parity recover the same sender.
4. The sender is the address of the public key recovered from `(r, s, parity)`
   and the EIP-712 signing hash.

This acceptance set is alloy's. Narrowing it to `v ∈ {27, 28}` and low `s` is
**Open**.

## Processing an input

1. **The backstop.** Pop and execute directs from the front of the fridge
   while `input.inclusion_block − front.inclusion_block ≥ max_wait_blocks`,
   raising the clock to each one's block (**New**). This bounds how long a
   sequencer that stops submitting, or freezes `safe_block`, can hold a direct
   back.
2. **A direct** joins the fridge, unless its payload is over the cap, and once
   there is room ([the fridge](#the-fridge)). The cap comes first: a direct
   that never joins drains nothing.
3. **A batch** runs the gates below.

### Batch validity

The first gate that applies decides the outcome:

| # | Gate | Outcome | `next_batch_nonce` |
|---|---|---|---|
| a | The payload does not SSZ-decode as `Batch` | Rejected | Kept |
| b | `batch.nonce != next_batch_nonce` | Rejected | Kept |
| c | `frames` is empty | Accepted; executes nothing | +1 |
| d | The frame structure is invalid (below) | Rejected | Kept |
| e | `inclusion_block − frames[0].safe_block ≥ max_wait_blocks` | Skipped as stale | Kept |
| f | **New.** `frames[0].safe_block < clock` | Rejected | Kept |
| g | Otherwise: execute every frame | Accepted | +1 |

Gate d requires, for every frame:

- its `safe_block` below `inclusion_block` (**New**: today, at or below);
- its `safe_block` at or above the previous frame's;
- its `fee_price` at most `MAX_EXPONENT`.

Gate d checks every frame before any frame executes; the backstop has already
run. Ages saturate at zero. An empty batch is accepted at any age, since it has
no first frame; rejecting empty batches is **Open**. A rejected batch and a
stale one have the same effect: no state change, nonce kept. So every later
batch fails gate b until one reuses that nonce, as only a new era's batches
do. The reject reason the reference reports is diagnostic.

### Frame execution

For each frame of an accepted batch, in order:

1. **Drain covered directs.** Pop and execute directs from the front of the
   fridge while their `inclusion_block ≤ frame.safe_block`. The `safe_block` is
   the sequencer's claim that it accounted for every direct up to that block;
   the scheduler enforces the drain whatever the sequencer did. Then raise the
   clock to `frame.safe_block` (**New**).
2. **Offer the user ops** in order. Skip an op, checked in this order, when:
   - its sender cannot be recovered ([signatures](#user-op-signatures));
   - its `max_fee` is below `frame.fee_price`;
   - its nonce is not its sender's next;
   - its sender's next nonce is `u32::MAX`, which has no successor.

   Otherwise the op executes with fee `frame.fee_price` and block
   `frame.safe_block`: the application advances the sender's nonce by one and
   applies the op, and the L2 tx count advances by one.

**New: shared code compares the nonce.** The application stores each sender's
next nonce and reports it; the shared executor compares it with the op's
([interfaces](interfaces.md#the-executor)). Today the application's own
validation decides
([`validate_and_execute_user_op`](../../../sequencer-core/src/application/mod.rs)).
The first two checks need no application, so the scheduler makes them alone.

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
| comes from a sender who cannot pay the fee (**New**) | The application's admission | Executed | Refused |
| exceeds the application's declared payload bound | The lane | Executed | Refused |
| breaks an operator policy | The application's admission, or the lane | Executed | Refused |
| fails in the application, such as a transfer above the balance | The application's execution | Executed | Admitted |
| is sponsored: the application charges it nothing | The application's execution | Executed | Admitted |
| is none of the above | | Executed | Admitted |

- **Skipped** means nothing changes: no nonce, no count, no fee.
- **Executed** means the sender's nonce and the L2 tx count each advance by
  one. What the op pays and whether its effect applies are the application's
  choice ([application](application.md#the-application-trait)). The
  application must not fail, since an application error halts the machine.
- **The lane refuses everything the scheduler skips**, by running the same
  checks first. No op is admitted and then skipped.

**New: an underfunded op executes.** Today the scheduler also skips an op whose
sender cannot pay the frame fee. That check protects only the sequencer from
its own inclusion, so it moves to the lane. With it gone, which transactions
execute depends on signatures, fee caps and nonces alone, never on a balance.
A fault in the application's logic can then change what a transaction does,
but not which transactions execute or what their counts are.

**A fee cap below the price skips the op.** The cap is part of what the sender
signed: execute this op only at a price up to the cap. Charging the price would
take more than the sender authorized. Executing at the cap instead would make
a frame's price mean something else for one op. The sequencer loses nothing by
the skip: it sets each frame's price, so it can put the op in a frame it fits.
A skipped op stays valid until its sender's nonce moves past it, as a pending
transaction does anywhere.

**A sponsored op needs no rule of its own.** Whether an executed op pays is
the application's choice, and whether an unpaid op is wanted is admission's.
An operator can run its own price feed for nothing in three ways, none of
which the scheduler sees:

- fund the feed's account: where the application credits fees to the operator,
  as the wallet does, the operator pays itself;
- exempt the role in the application: admission accepts its ops and execution
  charges them nothing;
- put its ops in a frame priced at exponent 0, one smallest unit.

A sponsored sender still signs a cap at or above the frame's price, such as
`MAX_EXPONENT`. An exemption must hang on something the signature proves, the
sender's role, or anyone can use it.

## The scheduler clock

**New.** The clock is the latest block the scheduler has executed at: an
accepted frame's safe block, or an executed direct's inclusion block. Every
frame of an accepted batch raises it, and so does every executed direct,
whether a frame, the backstop or an overflow drained it. It only rises: a
raise takes the later of the clock and the new block. Gate f rejects a batch
whose first frame lies below it, and gate d orders the rest.

**Gate f alone keeps blocks from going backwards.** Every frame lies at or
above the clock, so a frame's directs come from the clock or above and at or
below its safe block, and its user ops see the safe block. The backstop needs
no help: a batch whose first frame lies below a block the backstop drained is
already stale by gate e.

Three more rules keep the clock's split of the directs
([the invariant](#the-invariant)):

- **Strict frames** (gate d) let a frame drain every direct of its own safe
  block, because all of them landed before its batch. With equality allowed, a
  direct from that block landing after the batch would stay queued at the
  clock. Equality is also a claim no sequencer can make honestly: the block
  was not finished when the batch was sealed.
- **The backstop raises the clock**, as every drain does, so no direct it
  executed sits above the clock. The raise changes no outcome: gate e already
  skips any batch it would reject.
- **Overflow drains whole blocks**, so no direct at or below the clock stays
  queued ([the fridge](#the-fridge)). Draining only as many entries as needed
  would hand out the same L2 sequence under a hard bound, but it would leave
  part of a block queued at the clock.

Unless an overflow overtakes it, a correct sequencer never trips gate f or the
strict rule: it never opens a frame below an earlier one, and it opens frames
only at safe heads it has already seen, so its batch lands in a later block.
Its own frame ordering ([I3](../../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine))
becomes consensus-critical, though: a bug there now gets batches rejected.

## The fridge

- **Admission (New).** A direct whose payload is longer than
  `MAX_DIRECT_PAYLOAD_BYTES` never joins: it never executes and never counts.
  Every other direct joins, whatever it holds, and executes as today, where
  the application ignores what it cannot read. The cap is a constant of the
  template, set from the longest payload the application accepts. The
  sequencer's lane drops the same directs by the same constant, or the two
  diverge.
- **Why a constant.** A count bounds memory only if each entry's size is
  bounded too: portals accept zero-value deposits with extra data up to the
  InputBox's 64 KiB cap. A constant bounds the size without the scheduler
  running application code. A filter written by the application would also
  drop small garbage, but a zero-value portal deposit takes a slot for about
  the same L1 gas, so it would buy little.
- **Entries.** Each entry keeps the direct as delivered (sender, block,
  payload) with its input index (**New**), and the [direct context](#direct-context)
  the application receives.
- **The bound (New).** The fridge is full when it holds `FRIDGE_BOUND` entries
  or more. When a direct arrives at a full fridge, the scheduler executes whole
  blocks from the front, oldest first, until there is room, raising the clock
  to the last drained block. It never drains the arriving direct's own block:
  if only that block's directs remain, the direct joins over the bound. The
  bound is then soft by at most one block's directs, and memory stays bounded
  by what one L1 block can carry.
- **An overflow can reject sealed batches.** The sequencer cannot predict an
  overflow, and a batch it sealed earlier then lands in one of two ways. If
  its first frame is at or above the last drained block, it is accepted, and
  the L2 sequence is the one the sequencer predicted: the drained directs are
  the first ones that frame would have drained. If it is below, gate f rejects
  the batch, and every later one fails the nonce gate. The sequencer's
  acceptance check does not see the clock, so it counts those batches as
  accepted and diverges. A manual recovery answers that, and the watchdog's
  divergence latch reports it. An overflow also moves which input executes the
  drained directs, which the watchdog's comparison at that block can see
  ([register](../../review/register.md#bound-the-canonical-direct-input-fridge)).

The bound's value, the cap's value, and the largest drain the guest can afford
are **Open**
([register](../../review/register.md#bound-the-canonical-direct-input-fridge)).

## Direct context

A direct executes with its inclusion block as its block. **New:** the
application also receives its input index and timestamp. Both cost nothing: the
sequencer's input table and the feed already carry them. prevRandao comes only
on a stated need, since it needs a column and a feed field, and the block
proposer can bias it. Which of them libdex reads is a question for Bart. A user
op never sees its batch's context; it sees its frame's safe block and fee.

## Outputs and progress

An input's outputs are the application's notices and vouchers in execution
order: the backstop's directs first, then, for a direct, any overflow drain,
or, for a batch, each frame's drained directs and executed ops. Reports are diagnostics outside the outputs tree: the
reference reports a rejected batch with one fixed string and a stale skip not
at all.

The application reports its progress: its L2 tx count, and today also the
latest block it executed at. **New:** progress is the L2 tx count alone; the
clock moves to the scheduler.

## No reverts, and when to halt

The guest never answers an input with `RX_REJECTED`. A batch a gate rejects is
still accepted at the machine level. A revert would undo the whole input and
roll the scheduler's state back with the application's, so the scheduler's
state would depend on the application. It would also drop the backstop's
executions, which the reference keeps.

The reference halts the machine when the application returns an error; when
the batch nonce, the L2 tx count or the L1 input count has no `u64` successor;
and when an input's `index`, `blockNumber`, `timestamp` or `chainId` does not
fit `u64`. A halt is a permanent fixed point,
not a reject ([rollup host semantics](../../cartesi-machine.md#rollup-host-semantics)),
so a port must halt on the same inputs. No canonical successor state exists.

## State export

**New, and the only rule recovery needs.** The machine exports its whole state,
so the sequencer's setup can start from it:

- its L1 input count and its L2 tx count;
- the scheduler's state: the next batch nonce, the clock, and the first queued
  input, where the fridge starts;
- the application's state.

The queued directs themselves are not exported.
[The fridge is a queue](#the-schedulers-state), so the first queued input and
the L1 input count name them, and L1 holds their bytes.
[Application](application.md#the-state-export) specifies how the machine
answers. The harness answers the scheduler's part for every application; the
application supplies only its own state.

## The scheduler's interface

During a recovery the sequencer runs the scheduler apart from the
application: it feeds the scheduler one input, then executes each
transaction the scheduler hands out for it. The scheduler never reads the
application. That needs a free-standing scheduler with an interface of its
own, as the application has its trait and its C binding.
[Interfaces](interfaces.md#the-scheduler) outlines it, in Rust and in C; an
application's author may implement the C header with their own scheduler.
This page fixes the scheduler's behavior whatever the implementation.

## Constants

| Constant | Value | Home |
|---|---|---|
| `max_wait_blocks` | 7200 L1 blocks, about 24 h, for the wallet | the application's template |
| `MAX_EXPONENT` | 17101 | [`fee.rs`](../../../sequencer-core/src/fee.rs) (`build.rs` generates it) |
| `FRIDGE_BOUND` | **New**, value **Open** | the template |
| `MAX_DIRECT_PAYLOAD_BYTES` | **New**, value **Open** | the template |
| `sequencer_address` | per deployment | compiled into the canonical machine |

The window `max_wait_blocks` is at least one block. With zero, the backstop
could drain the arriving input's own block and leave a direct from that block
queued at the clock.

A fee exponent `n` stands for `(129/128)^n` smallest token units. The
application converts it; [`fee_to_linear`](../../../sequencer-core/src/fee.rs)
owns the bit-exact conversion. A checked-in lookup table in its place is
**Open** ([Track 5](../2026-07-coordination-tracks.md#track-5--fee-exponentiation-lut)).

## Conformance

[`consensus_vectors_v2.json`](../../../tests/fixtures/consensus_vectors_v2.json)
is the new template's vector set, written once and never regenerated. It
restates the earlier set's guest scenarios under strict frames, which change 18
of the 19, and adds scenarios for the rules this plan changes, including the
overflow drain and resume at a small fridge bound. A native test, the
in-machine golden tests and a test through `scheduler.h` hold it, and the last
is what a C port runs.
[Scheduler semantics](../../protocol/scheduler-semantics.md#conformance) owns
what the set holds and how each check runs.
