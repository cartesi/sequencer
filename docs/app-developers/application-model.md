# Reshaping your application logic

This page explains, in language-neutral terms, what your application's core
logic must look like to work with the sequencer. The code samples are
TypeScript and Go **illustrations of the contract** — the repository does not
ship TypeScript or Go bindings (see [`integration-paths.md`](integration-paths.md)).
The authoritative contract is
[`docs/protocol/application-contract.md`](../protocol/application-contract.md);
the reference implementation is the wallet in
[`examples/app-core/`](../../examples/app-core/src/application/wallet.rs).

## From a request loop to a state machine

A typical Cartesi application today is a loop around the rollup HTTP API (or
`libcmt`): fetch the next request, branch on advance or inspect, mutate some
state, emit outputs, finish.

```ts
// BEFORE — the application owns the loop and the I/O
while (true) {
  const req = await rollup.finish("accept");
  if (req.type === "advance") {
    const { msg_sender, block_number, block_timestamp } = req.metadata;
    handle(msg_sender, req.payload);          // mutates state
    await rollup.notice(...);                 // emits outputs as it goes
  }
}
```

With the sequencer, the loop no longer belongs to you. Two different hosts
drive your logic: the **scheduler** inside the machine, and the **sequencer**
outside it. Both call the same small set of entry points and expect the same
results. So the core of your application becomes a plain, I/O-free state
machine — an *engine*:

```ts
// AFTER — an engine: no loop, no network, no clock
interface Engine {
  // 1. May this operation be included? Read-only.
  validateUserOp(sender: Address, op: UserOp, currentFee: number): Accept | Reject;

  // 2. Apply an operation that passed validation. Returns notices/vouchers.
  applyUserOp(op: ValidUserOp, safeBlock: bigint): Output[];

  // 3. Apply an input that came straight from L1 (deposits, etc.).
  applyDirectInput(input: DirectInput): Output[];

  // 4. How many inputs have been applied, and the latest block seen.
  progress(): { executedInputCount: bigint; lastExecutedSafeBlock: bigint };

  // 5. Durable state: save, and (statically) restore.
  createDump(path: string): void;
}
```

```go
// The same contract in Go
type Engine interface {
    ValidateUserOp(sender Address, op UserOp, currentFee uint16) (Outcome, error)
    ApplyUserOp(op ValidUserOp, safeBlock uint64) ([]Output, error)
    ApplyDirectInput(in DirectInput) ([]Output, error)
    Progress() Progress
    CreateDump(path string) error
}
```

The data that crosses this boundary:

| Type | Fields | Notes |
|---|---|---|
| `UserOp` | `nonce: u32`, `max_fee: u16`, `data: bytes` | Exactly what the user signed. `data` is your payload. |
| `ValidUserOp` | `sender: address`, `fee: u16`, `data: bytes` | What you apply. The signature was already verified; `fee` is what to charge. |
| `DirectInput` | `sender: address`, `block_number: u64`, `payload: bytes` | An L1 input. `sender` is the L1 `msg_sender` (a portal, for deposits). |
| `Output` | notice `bytes`, or voucher (`destination`, `value`, `payload`) | Returned in order, not emitted as a side effect. |

Notice what is **not** there: no timestamp, no `prev_randao`, no input index,
no way to emit a report from execution. If your logic uses any of these, see
[Time](#time) and [Outputs](#outputs).

## Two kinds of input

**User operations** arrive through the sequencer. The user is identified by
their signature — the host recovers the signer's address and passes it as
`sender`. You never see or verify the signature yourself.

**Direct inputs** arrive from L1. They are everything sent to your
application's InputBox by anyone except the sequencer: portal deposits, and
also any plain `addInput` a user makes. What a non-portal direct input means
is your decision. The wallet example accepts ERC-20 portal deposits and treats
everything else as a no-op.

One consequence deserves emphasis. In the machine, the input that carries a
batch has the *sequencer* as its `msg_sender`. If your current code does
`balances[msg_sender] -= amount`, that logic must move behind the engine
boundary, where `sender` is the recovered signer. The scheduler performs the
unpacking and signature recovery; your engine never parses a batch.

## Nonces

On L1, replay protection is free: the account nonce stops anyone from
resubmitting a transaction. A signed message sent over HTTP has no such
protection, so **your application state must hold a nonce per sender**.

- Each sender starts at nonce `0`.
- `validateUserOp` rejects an operation whose `nonce` differs from the sender's
  expected nonce.
- `applyUserOp` increments the sender's nonce — always, even when the operation
  then fails for business reasons.

`ValidUserOp` carries no nonce field: applying an operation consumes whatever
nonce the state currently expects.

## Fees

The sequencer pays L1 gas to post batches, and recovers that cost through a
**fee your application charges on every user operation**. This also prices out
spam. The protocol fixes how the fee is *expressed*; your application decides
what token it is *paid in* and who receives it.

**The exponent encoding.** A fee travels as a 16-bit integer `n` that means
`floor((129/128)^n)` of your fee token's smallest unit. Each step is about
+0.78 %; `n = 0` is one unit. There are two fee numbers:

- `max_fee` — chosen by the user, signed into the operation: "I accept any fee
  up to this."
- `current_fee` — chosen by the sequencer for the current frame from L1 gas
  prices. It is what the operation actually pays. Clients read it, and a
  suggested `max_fee`, from the sequencer's `GET /fee`
  ([`client-integration.md`](client-integration.md#choosing-max_fee)).

The host checks `max_fee >= current_fee` before calling you; comparing the
exponents is enough. Your engine needs the *linear* amount to check a balance
and to charge it:

```ts
function validateUserOp(sender, op, currentFee) {
  if (op.nonce !== this.nonceOf(sender))
    return reject("InvalidNonce", { expected: this.nonceOf(sender), got: op.nonce });

  const cost = feeToLinear(currentFee);              // bigint
  if (this.balanceOf(sender) < cost)
    return reject("InsufficientFeeBalance", { required: cost, available: this.balanceOf(sender) });

  return accept();
}
```

`feeToLinear` must produce **bit-identical results** everywhere your logic
runs, because a one-unit difference changes balances and therefore state. The
reference is [`sequencer-core/src/fee.rs`](../../sequencer-core/src/fee.rs):
integer fixed-point arithmetic with 64 fractional bits, a 15-entry table of
`(129/128)^(2^i)` built by repeated squaring in
[`build.rs`](../../sequencer-core/build.rs), and binary exponentiation over the
bits of `n`. A port to another language must reproduce that algorithm exactly —
do not use floating point or a math library `pow`. Test your port against the
Rust function across the whole exponent range.

The wallet example charges the fee in the single ERC-20 it supports and credits
it to a configured sequencer address. Whatever you choose, it is part of your
state transition and must be identical in both hosts.

## The three outcomes

Every user operation ends in exactly one of three ways. Keeping them apart is
the most important rule on this page.

| Outcome | When | Effect on state | What the user sees |
|---|---|---|---|
| **Rejected** | Validation says no: wrong nonce, fee above `max_fee`, cannot pay the fee | **None.** Nonce not consumed, nothing charged, nothing recorded | HTTP `422` with the reason |
| **Included** | Validation passed | Nonce consumed, fee charged, counters advance — **even if the operation then fails** | HTTP `200` |
| **Fatal** | The engine itself is broken (a bug, corrupted state, disk failure) | Undefined; the host discards the engine and stops | Sequencer outage |

Work through what this means for ordinary failures. A user signs a transfer of
100 tokens but holds 40. That is **not** a rejection. Validation only looks at
nonce and fee, so the operation is included: the nonce is consumed, the fee is
charged, the transfer does nothing, and no notice is emitted. The same holds
for a payload your decoder cannot parse — it is an included no-op.

Why so strict? Rejections are decided off-chain and leave no trace on L1. If
"insufficient balance for the transfer" were a rejection, the machine —
replaying the batch later — would have to reach the same verdict at the same
point, and any subtle difference would fork the state. Limiting rejection to
two cheap, well-defined checks keeps the two hosts trivially in agreement. It
also means a user cannot spam failing operations for free.

Two rules follow:

- **Never turn bad input into a fatal error.** Payload bytes come from
  whoever signed or posted them. An engine that throws on unparseable input
  hands every user a kill switch for the sequencer. Every entry point must be
  *total*: any byte string produces either a rejection or an included result.
- **Never turn an engine fault into a rejection or a no-op.** If your state is
  inconsistent, fail loudly. Continuing silently means the two hosts may
  diverge without anyone noticing.

Direct inputs have no rejection path at all: every direct input is included.
One your application does not understand is an included no-op.

## Validation is read-only

`validateUserOp` must not change anything — no counters, no caches that affect
results, no lazy initialization that alters later behavior. The hosts call it
on different schedules: the sequencer validates operations that may never be
included, the machine validates during batch execution, and restart replay
skips validation entirely because it applies operations that were already
accepted. State that mutates during validation would differ between the three.

## Determinism

Given the same state and the same input, your engine must produce the same new
state, the same outputs in the same order, and the same serialized bytes — on
an x86 server, on an ARM laptop, and in the riscv64 machine.

General rules:

- No wall clock, no randomness, no environment, network, or filesystem reads
  during execution.
- No floating point in anything that affects state. Use integers (`bigint`,
  `big.Int`, `uint256`) for amounts.
- No iteration over unordered collections when order affects results or
  serialized bytes. Sort first.
- No dependence on memory addresses, thread scheduling, or hash seeds.

Language notes:

- **TypeScript / JavaScript.** `Date.now()` and `Math.random()` are out.
  `number` is a float: use `bigint` for balances and amounts. `Map` and `Set`
  iterate in insertion order, which is deterministic only if insertions are;
  sort keys before serializing. `JSON.stringify` key order follows insertion
  order — use a canonical encoder for state bytes. Engine version differences
  (V8 vs. QuickJS, or different Node versions) can change number formatting and
  sort stability guarantees; pin the runtime.
- **Go.** `map` iteration order is deliberately randomized: never range over a
  map to build outputs or state bytes without sorting the keys. Keep execution
  on one goroutine. Avoid `float64`. `time.Now()` is out.
- **C / C++.** Avoid `std::unordered_map` iteration order, uninitialized
  padding in serialized structs, and signed-overflow or other undefined
  behavior that compilers may resolve differently per target.

## Time

User operations are not L1 transactions and carry no timestamp. The only clock
your engine receives is a **block number**:

- for a user operation, the `safe_block` of the frame it was sequenced in — an
  L1 block the sequencer has fully accounted for;
- for a direct input, the `block_number` of the L1 block that included it.

Treat your application clock as the running maximum of these numbers (see
[Progress counters](#progress-counters)) rather than assuming each input's
number is higher than the last. Many user operations share the same
`safe_block`; the sequencer advances it every few L1 blocks, not per operation. If your application uses timestamps today — order expiry,
interest accrual, vesting — redefine those rules in block numbers. Do not
estimate a timestamp from the block number unless the estimate is itself a
deterministic rule of your application.

## Progress counters

Your state must include two integers and keep them current:

- `executed_input_count` — how many inputs have been applied. It increases by
  exactly one for every included user operation and every direct input,
  including no-ops. Rejections do not count.
- `last_executed_safe_block` — the highest block number carried by any applied
  input: `max(previous, safe_block)` for user ops,
  `max(previous, block_number)` for direct inputs. It starts at zero.

```go
func (e *Engine) ApplyDirectInput(in DirectInput) ([]Output, error) {
    outputs := e.handleDeposit(in)          // may do nothing at all
    e.count++                               // always
    if in.BlockNumber > e.clock { e.clock = in.BlockNumber }
    return outputs, nil
}
```

The host checks these after every call and stops if they are wrong. They are
how the sequencer lines your state up with its own records after a restart,
and how recovery knows which L1 block a saved state reflects. They are part of
your state: they must be saved and restored with everything else.

## Outputs

`applyUserOp` and `applyDirectInput` *return* their notices and vouchers, in
order. They do not emit them. Inside the machine, the scheduler's harness
emits the returned outputs to the rollup; beside the sequencer, outputs are
computed and dropped — only the machine's outputs exist on-chain.

Consequences:

- Output bytes and output order are part of determinism.
- Execution cannot emit reports. If you used reports for logging, log through
  your host's ordinary logging instead; if you used them to return data, move
  that to inspect or to your indexer.
- The sequencer's `POST /tx` response does not contain your outputs. A frontend
  that needs the result of an operation derives it from the indexer (see
  [`client-integration.md`](client-integration.md#reading-state)).

## Payload format and size

`data` is opaque to the sequencer. Use whatever encoding suits you; the wallet
uses SSZ. Two practical constraints:

- Your application declares a **maximum payload size** in bytes. The sequencer
  rejects larger payloads at the door and uses the number to size batches.
- Every byte is posted to L1 and paid for by the fee. A compact binary encoding
  is materially cheaper for your users than JSON.

The limit applies to user operations only. Direct inputs come from L1 with
whatever size L1 allowed; handle them defensively.

## Saving and restoring state

The sequencer restarts. When it does, it restores your engine from the most
recent checkpoint and re-applies the operations recorded since. Your engine
therefore needs:

- **Create a checkpoint** at a path the host supplies. When the call returns,
  the data must survive a power cut: write, `fsync` the files, `fsync` the
  containing directories. Creating a checkpoint must not change logical state,
  and later execution must not modify a finished checkpoint.
- **Restore from a checkpoint** into a fresh, independent engine. It must keep
  working after the checkpoint it came from is deleted.
- **Name the canonical state file** inside a checkpoint: one file whose bytes
  are the deterministic serialization of your state.

That last file has a second job. The watchdog fetches it from the sequencer,
asks the machine for its state, and compares the two **byte for byte**. So:

- The serialization must be canonical — sorted, no padding, no optional
  encodings, progress counters included. Two engines with equal logical state
  must produce identical bytes regardless of the history that led there.
- The machine must be able to produce the same bytes. With the reference
  scheduler, that is the response to the inspect query `state`.

If everything needed to resume your engine *is* that canonical serialization
(as in the wallet), the checkpoint is just that one file. If your engine needs
more to resume — a database directory, a machine snapshot — the checkpoint is a
directory holding that plus the canonical file. Layouts are described in
[`docs/snapshots/format.md`](../snapshots/format.md).

## Capacity

The sequencer applies operations one at a time, on a single ordered lane, and
targets a sub-500 ms acknowledgement. A slow `validate` or `apply` directly
limits throughput and latency for every user. When the sequencer catches up on
L1 it applies the whole backlog of direct inputs before returning to user
operations, and checkpoint creation happens on the same lane. Keep all three
fast, and measure them with realistic state sizes.

## A test you should write first

Before any hosting work, build a harness that feeds the same sequence of user
operations and direct inputs to two independent instances of your engine —
ideally one native build and one riscv64 build — and compares canonical state
bytes and outputs after every step. Include: wrong nonces, unaffordable fees,
unparseable payloads, failing business operations, deposits of unsupported
tokens, a checkpoint/restore in the middle, and the full fee-exponent range.
Every divergence this catches on your desk is one the watchdog will not have
to catch in production.
