# The scheduler, for the next template

**Status:** proposed on 2026-11-05 for the next template, to settle with Bart;
nothing here is implemented. Today's rules are in
[scheduler semantics](../../protocol/scheduler-semantics.md), which this page
replaces when the template ships. It states every rule, not a diff. **New**
marks a rule that changes; **Open** marks one still to settle with the port's
author ([register](../../review/register.md#canonical-rules-open-for-the-c-port)).

The canonical scheduler turns an application's L1 inputs, in L1 order, into
one sequence of L2 transactions, and the application executes them. Every
validator runs it inside the canonical machine. The sequencer predicts it
while it sequences, and runs it on its own during a recovery. The reference is
[`Scheduler<A>`](../../../sequencer-core/src/scheduler/mod.rs). This text is
the contract, and the reference can have bugs, so report any disagreement.

## What changes

| Change | Why | Does recovery need it? |
|---|---|---|
| [State export](#state-export) | Setup starts a sequencer from a machine's whole state | Yes, and nothing else on this list |
| [The fridge bound](#the-fridge) | An unbounded fridge can exhaust the machine's memory, which halts the application for good | No; a recovery is only the remedy after an overflow |
| [Monotone blocks](#the-scheduler-clock) | A transaction's block never goes backwards, so applications may read blocks as time | No |
| [An underfunded op counts](#frame-execution) | An op's fate depends only on what its signer controls | No |
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
   `MAX_WAIT_BLOCKS` execute, oldest first, even when the input is a batch the
   gates then reject.
2. **A direct input**, from any sender but the sequencer, joins the fridge,
   the queue of directs not yet executed. **New:** only a well-formed one
   joins, and a full fridge drains its oldest blocks to make room.
3. **A batch**, the sequencer's input, is accepted or changes nothing
   ([batch validity](#batch-validity)).
4. **Each frame of an accepted batch** first executes every queued direct up
   to its safe block, oldest first, then offers its user ops in order.
5. **A user op is executed or skipped** ([frame execution](#frame-execution)).

### What counts

A state's **L2 tx count** is how many L2 transactions it has executed. It
names the state, and it names the transaction that state executes next, so
the feed numbers its rows with it. The code calls it `executed_input_count`,
though it counts transactions, not L1 inputs.

A user op has three outcomes, and only one of them leaves no trace:

| Outcome | What happens | Counts |
|---|---|---|
| **Skipped** | Nothing: no nonce, no fee, no effect | No |
| **Executed, succeeded** | The sender's nonce advances, the fee is charged, the effect applies | Yes |
| **Executed, failed** | The sender's nonce advances and the fee is charged, but the effect fails, such as a transfer larger than the balance | Yes |

An op whose sender cannot pay the whole fee executes too (**New**,
[frame execution](#frame-execution)). Every executed direct counts, whatever the application does with it.
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
application executes a user op changes none of them. A port must keep this
property: it is what lets the sequencer run the scheduler without the
application during a recovery.

### Two counts

| Count | Names | Who knows it |
|---|---|---|
| **L2 tx count** | A state, and the next L2 transaction | Every application state; a client knows only this one |
| **L1 input count** | A machine's position: how many InputBox inputs it has consumed | The machine |

They differ, because a batch envelope or a queued direct is an L1 input but
not an L2 transaction. The machine exports both
([state export](#state-export)).

### The invariant

**New.** With the four rules of the [scheduler clock](#the-scheduler-clock),
whole-block drains among them, one invariant holds after every input:
every admitted direct at or below the clock has executed, and every one above
it is still queued. Along the L2 sequence, blocks never go backwards.

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
   while `input.inclusion_block − front.inclusion_block ≥ MAX_WAIT_BLOCKS`,
   raising the clock to each one's block (**New**). This bounds how long a
   sequencer that stops submitting, or freezes `safe_block`, can hold a direct
   back.
2. **A direct** joins the fridge, if it is well formed and once there is room
   ([the fridge](#the-fridge)).
3. **A batch** runs the gates below.

### Batch validity

The first gate that applies decides the outcome:

| # | Gate | Outcome | `next_batch_nonce` |
|---|---|---|---|
| a | The payload does not SSZ-decode as `Batch` | Rejected | Kept |
| b | `batch.nonce != next_batch_nonce` | Rejected | Kept |
| c | `frames` is empty | Accepted; executes nothing | +1 |
| d | The frame structure is invalid (below) | Rejected | Kept |
| e | `inclusion_block − frames[0].safe_block ≥ MAX_WAIT_BLOCKS` | Skipped as stale | Kept |
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
batch fails gate b until one reuses that nonce, as a batch rebuilt after a
recovery does. The reject reason the reference reports is diagnostic.

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
   - the application reports that its nonce is not the sender's next;
   - its `nonce` is `u32::MAX`, which has no successor.

   Otherwise the op executes with fee `frame.fee_price` and block
   `frame.safe_block`, through the shared execution boundary
   ([`validate_and_execute_user_op`](../../../sequencer-core/src/application/mod.rs)):
   the application advances the sender's nonce by one, charges the fee and
   applies the op, and the L2 tx count advances by one.

**New: an underfunded op executes.** Today the scheduler also skips an op whose
sender cannot pay the frame fee. With the next template it executes: its nonce
advances and it counts. How much fee it pays and whether its effect runs are
the application's choice; charging what the sender has, a saturating
subtraction, is the recommendation. The application must not fail on it,
since an application error halts the machine. Two reasons:

- An op's fate depends only on what its signer controls: the signature, the
  nonce and the fee cap.
- An op on L1 always uses up its nonce. A skipped op stays valid, in public,
  and any later batch can include it once its sender has funds.

Ingress is policy, not consensus: the sequencer still refuses an underfunded op
at `POST /tx`, since it would pay less than the L1 data it costs
([application](application.md#the-application-trait)).

## The scheduler clock

**New.** The clock is the latest block the scheduler has executed at: an
accepted frame's safe block, or an executed direct's inclusion block. Every
frame of an accepted batch raises it, and so does every executed direct,
whether a frame, the backstop or an overflow drained it. Gate f rejects a
batch whose first frame lies below it, and gate d orders the rest. Four rules
work together:

- **Gate f** keeps every frame at or above the clock, so the blocks the
  application sees never go backwards: a frame's directs come from above the
  clock and at or below its safe block, and its user ops see the safe block.
- **Strict frames** (gate d) let a frame drain every direct of its own safe
  block, because all of them landed before its batch. With equality allowed, a
  direct from that block landing after the batch would stay queued at the
  clock, breaking the [invariant](#the-invariant).
- **The backstop raises the clock** without changing any outcome: a batch whose
  first frame lies below a block the backstop drained is already stale by
  gate e. The raise only keeps the invariant.
- **Overflow drains whole blocks**, so no direct at or below the clock stays
  queued ([the fridge](#the-fridge)).

Unless an overflow overtakes it, a correct sequencer never trips gate f or the
strict rule: it never opens a frame below an earlier one, and it opens frames
only at safe heads it has already seen, so its batch lands in a later block.
Its own frame ordering ([I3](../../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine))
becomes consensus-critical, though: a bug there now gets batches rejected.

## The fridge

- **Admission (New).** Only a direct the application's *direct filter* accepts
  joins. The rest are dropped: they never execute and never count. The filter
  reads only the direct and the application's fixed configuration, such as a
  portal's address, never state that transactions change. The sequencer drops
  the same directs with the same filter, or the two diverge
  ([application](application.md#the-application-trait)).
- **Entries.** Each entry keeps the direct as delivered (sender, block,
  payload) with its input index (**New**), and the [direct context](#direct-context)
  the application receives.
- **The bound (New).** The fridge holds at most `FRIDGE_BOUND` entries. When a
  direct arrives at a full fridge, the scheduler executes whole blocks from the
  front, oldest first, until there is room, raising the clock to the last
  drained block. It never drains the arriving direct's own block: if only that
  block's directs remain, the direct joins over the bound. The bound is then
  soft by at most one block's directs, and memory stays bounded by what one L1
  block can carry.
- **An overflow diverges.** The sequencer cannot predict an overflow, so the
  batches it sealed before one diverge, and a manual recovery answers that. The
  watchdog's divergence latch reports it.

The bound's value, an entry's largest size, and the largest drain the guest
can afford are **Open**
([register](../../review/register.md#bound-the-canonical-direct-input-fridge)).
A count bounds memory only if each entry's size is bounded too: portals accept
zero-value deposits with extra data up to the InputBox's 64 KiB cap.

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
the batch nonce or the L2 tx count has no `u64` successor; and when
`blockNumber` or `chainId` does not fit `u64`. A halt is a permanent fixed point,
not a reject ([rollup host semantics](../../cartesi-machine.md#rollup-host-semantics)),
so a port must halt on the same inputs. No canonical successor state exists.

## State export

**New, and the only rule recovery needs.** The machine exports its whole state,
so the sequencer's setup can start from it:

- its L1 input count and its L2 tx count;
- the scheduler's state: the next batch nonce, the clock and the fridge, with
  each entry's input index;
- the application's state.

It answers through inspect queries, which [application](application.md#the-state-export)
specifies. The harness answers the scheduler's part for every application; the
application supplies only its own state.

## The scheduler's interface

During a recovery the sequencer runs the scheduler without the application:
first it schedules the whole rebuilt range, then the application executes it.
That needs a free-standing scheduler with an interface of its own, as the
application has its trait and its C binding. That interface is pending, to
design with Bart ([todo](todo.md#build)). This page fixes the scheduler's
behavior whatever the interface.

## Constants

| Constant | Value | Home |
|---|---|---|
| `MAX_WAIT_BLOCKS` | 1200 L1 blocks, about 4 h | [`lib.rs`](../../../sequencer-core/src/lib.rs) |
| `MAX_EXPONENT` | 17101 | [`fee.rs`](../../../sequencer-core/src/fee.rs) (`build.rs` generates it) |
| `FRIDGE_BOUND` | **New**, value **Open** | the template |
| `sequencer_address` | per deployment | compiled into the canonical machine |

A fee exponent `n` stands for `(129/128)^n` smallest token units. The
application converts it; [`fee_to_linear`](../../../sequencer-core/src/fee.rs)
owns the bit-exact conversion. A checked-in lookup table in its place is
**Open** ([Track 5](../2026-07-coordination-tracks.md#track-5--fee-exponentiation-lut)).

## Conformance

[`consensus_vectors_v1.json`](../../../tests/fixtures/consensus_vectors_v1.json)
pins today's template: EIP-712 hashes, keccak256, signature cases, SSZ bytes,
fee conversions, `EvmAdvance` calldata, and guest scenarios with their exact
outputs. It is never regenerated, since a mismatch is a consensus change. The
next template ships a new vector set:

- the guest scenarios rewritten for strict frames, which change 18 of the 19;
- scenarios for gate f, an overflow, the direct filter, an underfunded op, and
  the export queries;
- the cases v1 leaves out: `s ≥ n`, `v` from 29 to 34, the fee-range reject,
  and the gate edge cases the reference's unit tests pin today.
