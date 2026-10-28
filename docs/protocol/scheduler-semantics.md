# Scheduler acceptance semantics

The canonical scheduler folds an application's L1 inputs, in L1 order, into
application state. This page specifies that fold for anyone who reimplements
it. The reference is [`Scheduler<A>`](../../sequencer-core/src/scheduler/mod.rs),
run in the canonical machine by the [canonical-app harness](../../examples/canonical-app/src/scheduler/mod.rs).
This text is the contract and the reference can have bugs, so report any
disagreement. The [application contract](application-contract.md) owns what
the application does with each input. Rules marked **pending** describe the
reference today; changing one changes the canonical machine image or its wire
format, so they are settled with the C++ port's author
([register](../review/register.md#canonical-rules-open-for-the-c-port)).

## The one stream, the one classification

Each input is a Cartesi Rollups `EvmAdvance` from the InputBox. The scheduler
reads its `msgSender`, its `blockNumber` (the *inclusion block*), its `chainId`
and `appContract` (the signature domain), and its `payload`. It classifies by
sender only, never by payload content:

- `msgSender == sequencer_address`: the payload is a **batch**.
- Any other sender: a **direct input**, appended unchanged to the *fridge*,
  the FIFO of directs not yet executed.

`sequencer_address` is deployment data compiled into the canonical machine. It
must equal the batch-submitter address that `setup` pins; nothing checks this
at runtime. If they differ, the machine treats every batch as a direct input
while the sequencer still reports its batches accepted, so only a watchdog
comparison reveals it.

The scheduler's state is `next_batch_nonce` (`u64`, 0 at genesis), the fridge,
and the application, which holds its executed-input count and clock. Nothing
else carries over between inputs: no earlier batch's `safe_block` is kept. The
fridge has no size bound (**pending**).

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
fixed 65-byte field is **pending**.

## User-op signatures

A user op signs the EIP-712 struct `UserOp(uint32 nonce,uint16 max_fee,bytes data)`
([`user_op.rs`](../../sequencer-core/src/user_op.rs)) under the domain
`EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)`:
name `CartesiAppSequencer`, version `1`, the input's `chainId`, its
`appContract` as `verifyingContract`, and no salt
([`build_input_domain`](../../sequencer-core/src/lib.rs)). The sender is not
signed; it is recovered from `r ‖ s ‖ v` (alloy's `Signature::from_raw` and
`recover_address_from_prehash`), and any failure drops the op:

1. The signature must be 65 bytes, and `v` must be 0, 1, 27, 28 or at least 35.
   The y-parity is `v` for 0 and 1; for `v ≥ 27` it is 1 when `v` is even,
   else 0.
2. `r` and `s` must lie in `[1, n − 1]`, where `n` is the secp256k1 order.
3. If `s > n / 2`, `s` becomes `n − s` and the parity flips, so a high-`s`
   signature and its low-`s` twin of opposite parity recover the same sender.
4. The sender is the address of the public key recovered from `(r, s, parity)`
   and the EIP-712 signing hash.

This acceptance set is the library's, pinned on both the batch path and
`POST /tx` by the signature vectors except for `s ≥ n` and `v` from 29 to 34;
it is **pending**. Batches the sequencer
builds carry `v ∈ {27, 28}`, but keep a user's `s` as signed.

## Processing an input

Every input first runs the **censorship backstop**: pop and execute directs
from the front of the fridge while
`input.inclusion_block − front.inclusion_block ≥ MAX_WAIT_BLOCKS`. This bounds how
long a sequencer that stops submitting, or freezes `safe_block`, can hold a
direct back. Then a direct joins the fridge, and a batch runs these gates in
order; the first that applies decides the outcome:

| # | Gate | Outcome | `next_batch_nonce` |
|---|---|---|---|
| a | payload does not SSZ-decode as `Batch` | rejected | kept |
| b | `batch.nonce != next_batch_nonce` | rejected | kept |
| c | `frames` is empty | executed, as a no-op | +1 |
| d | frame structure is invalid (below) | rejected | kept |
| e | `inclusion_block − frames[0].safe_block ≥ MAX_WAIT_BLOCKS` | skipped as stale | kept |
| f | otherwise: execute every frame (below) | executed | +1 |

Ages saturate at zero. An empty batch is valid at any age, since it has no
first frame (**pending**). Gate d rejects a batch whose frames include a
`safe_block` above `inclusion_block`, or a `safe_block` lower than the previous
frame's, or a `fee_price > MAX_EXPONENT`. Gate d checks all frames before
any frame drain or user-op execution; the overdue-direct backstop still runs
before the gates. Structural validation looks only inside the batch: a later
batch may start below an earlier one's last `safe_block`, and its user ops then
see the lower value. A
stale batch keeps the nonce, so every later batch fails gate b until one
reuses that nonce, as a batch rebuilt after sequencer recovery does. A
rejected and a skipped batch have the same effect: no state change, nonce
kept. The reject reason the reference reports is diagnostic.

### Frame execution (gate f)

For each frame, in order:

1. **Drain covered directs.** Pop and execute directs from the front of the
   fridge while their `inclusion_block ≤ frame.safe_block`. The `safe_block` is
   the sequencer's claim that it accounted for every direct up to that block;
   the scheduler enforces the drain whatever the sequencer did.
2. **Execute the user ops** in order. Skip an op, with no effect, if (checked
   in this order) its sender cannot be recovered, `max_fee < frame.fee_price`,
   the application rejects it at fee `frame.fee_price`, or its `nonce` is
   `u32::MAX`, which has no successor. Otherwise apply it with fee
   `frame.fee_price` and clock `frame.safe_block`
   ([`validate_and_execute_user_op`](../../sequencer-core/src/application/mod.rs)).

## Outputs and the executed-input count

An input's outputs are the application's notices and vouchers in execution
order: overdue directs, then each frame's drained directs and applied ops.
Reports are diagnostics outside the outputs tree: the reference reports a
rejected batch with one fixed string and a stale skip not at all, and the
[golden guest tests](../../examples/canonical-test/src/golden.rs) ignore them.

The executed-input count is canonical state too. It advances by one for each
executed direct and each applied user op, including an app-level no-op or
business failure, and for nothing else: not for queueing a direct, a skipped
op, a batch envelope or an empty batch. A rejected or stale batch can still
advance it through the backstop. The [application contract](application-contract.md#4-canonical-history-cursor-executed_input_count)
owns the count; the [history contract](application-history.md) owns how
catch-up, recovery and the API use it.

## Fatal conditions

The reference panics, halting the machine, when the application returns an
error; when the batch nonce or the executed-input count has no `u64`
successor; and when `blockNumber` or `chainId` does not fit `u64`. A halt is a
permanent fixed point, not a reject ([rollup host semantics](../cartesi-machine.md#rollup-host-semantics)),
so a port must halt on the same inputs. No canonical successor state exists.

## Constants

| Constant | Value | Home |
|---|---|---|
| `MAX_WAIT_BLOCKS` | 1200 L1 blocks, about 4 h | [`lib.rs`](../../sequencer-core/src/lib.rs) |
| `MAX_EXPONENT` | 17101 | [`fee.rs`](../../sequencer-core/src/fee.rs) (`build.rs` generates it) |
| `sequencer_address` | per deployment | compiled into the canonical machine |

A fee exponent `n` stands for `(129/128)^n` smallest token units. The
application converts it; [`fee_to_linear`](../../sequencer-core/src/fee.rs)
owns the bit-exact conversion. A checked-in lookup table in its place is
**pending** ([Track 5](../plans/2026-07-coordination-tracks.md#track-5--fee-exponentiation-lut)).

## Conformance

[`consensus_vectors_v1.json`](../../tests/fixtures/consensus_vectors_v1.json)
pins EIP-712 hashes, keccak256, the signature cases above (every accepted `v`
form, `v = 2`, high `s`, 64 bytes, `r = 0`, `s = 0`, `r = n`) with the sender
each path recovers, SSZ bytes, fee conversions, `EvmAdvance` calldata, and
guest scenarios with their exact outputs. It is never regenerated: a mismatch
is a consensus change. Gate edge cases are pinned by the reference's unit tests
and the [guest tests](../../examples/canonical-test/src/main.rs); vectors for
them are **pending**.

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

The sequencer predicts this fold in two more places: the off-chain predicate
`scheduler_accepts`, which builds the gold frontier, and the inclusion lane's
live prediction. The recovery fold drives `Scheduler<A>` itself.
[I1](../invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)
maps all three and their one deliberate difference: the predicate skips gate d.
[I2](../invariants.md#i2-drain-attribution-accumulated-directs-land-in-the-clock-advanced-frame)
and [I3](../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine)
are the lane's mirrors of the drain order and of frame monotonicity, and
[I19](../invariants.md#i19-application-progress-follows-the-shared-execution-contract)
covers the shared execution boundary. Change one, check all.
