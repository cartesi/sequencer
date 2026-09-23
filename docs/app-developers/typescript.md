# An all-TypeScript application: what you have to write

[`integration-paths.md`](integration-paths.md) says there is no ready path for
TypeScript. This page spells out what building one involves: each component,
the exact rules it must follow, and where the risks are.

## What "fully TypeScript" can mean

The protocol has three parties. How much of each can be TypeScript differs.

| Party | Can it be TypeScript? | Notes |
|---|---|---|
| **The machine** (canonical execution) | **Yes, entirely.** | This is where protocol compliance is decided. L1 plus the machine define the truth; nothing here depends on Rust. You write the engine *and* a scheduler. |
| **Clients and indexer** | **Yes, entirely.** | See [`client-integration.md`](client-integration.md). The indexer reuses your engine as a library. |
| **The sequencer host** | **Your engine, yes. The sequencer, realistically no.** | The sequencer is a Rust library that calls your engine through a Rust trait (or, for compiled languages, the C ABI). A TypeScript engine needs a small non-TypeScript bridge, or a TypeScript rewrite of the sequencer — a far larger and riskier job. |

So the practical target is: **all application and protocol logic in TypeScript,
one small language-neutral bridge on the sequencer host.**

| # | Component | Runs | Size | Risk |
|---|---|---|---|---|
| 1 | Engine | Machine, sequencer host, indexer | Your application | Determinism |
| 2 | Scheduler | Machine | A few hundred lines | **Consensus-critical** |
| 3 | Machine entry point (rollup loop) | Machine | Small | Low |
| 4 | Engine server + bridge | Sequencer host | Small; bridge is not TypeScript | Low |
| 5 | Client SDK, indexer | Browser, server | Ordinary; the indexer reuses 1 and 2 | Low |
| 6 | Conformance tests against the Rust reference | CI | Medium | This is what makes 1–2 trustworthy |

## 1. The engine

Everything in [`application-model.md`](application-model.md) applies. The
TypeScript-specific discipline, because the same source runs on a JavaScript
runtime built for x86/ARM *and* one built for riscv64:

- **Integers only.** `bigint` for every amount, nonce, counter, and block
  number. `u64` values do not fit a `number`. No `Math.*` on values that reach
  state; transcendental functions may differ between builds.
- **Bytes, not strings, as keys and order.** Compare addresses as lowercase
  hex or raw bytes using code-unit order. Never `localeCompare` or `Intl` —
  they depend on the ICU data of the runtime.
- **Canonical state bytes are hand-specified.** Write an explicit binary
  encoder (fixed field order, sorted collections, fixed-width big-endian or
  little-endian integers). Not `JSON.stringify`, not a serializer whose output
  may change between library versions.
- **No ambient inputs**: `Date`, `Math.random`, `process.env`, timers,
  `crypto.getRandomValues`, network, or file reads during execution.
- **Pin everything**: the runtime version, the bundler output, and dependency
  versions. The bundle that goes into the machine image is part of the
  machine's hash; build it reproducibly and run the *same* bundle on the
  sequencer host.
- **Fees**: port `feeToLinear` exactly
  ([`client-integration.md`](client-integration.md#choosing-max_fee) has a
  BigInt version) and test it against the Rust function for every exponent.

## 2. The scheduler

This replaces [`Scheduler<A>`](../../sequencer-core/src/scheduler/mod.rs) and
the [`canonical-app` harness](../../examples/canonical-app/src/scheduler/mod.rs).
The specification is
[`docs/protocol/scheduler-semantics.md`](../protocol/scheduler-semantics.md).
What follows is that algorithm as an implementation checklist.

### State

Kept in memory for the life of the machine, alongside the engine:

```ts
type QueuedDirect = { sender: Address; inclusionBlock: bigint; payload: Uint8Array };

let queue: QueuedDirect[] = [];       // FIFO of direct inputs not yet executed
let nextBatchNonce = 0n;              // u64
const SEQUENCER: Address = "0x…";     // the batch submitter; part of the image
const MAX_WAIT_BLOCKS = 1200n;
```

### Per advance request

```ts
function processInput(meta: Metadata, payload: Uint8Array): Output[] {
  const outputs: Output[] = [];
  const block = meta.blockNumber;                        // bigint

  // 1. Backstop — runs for EVERY input, before anything else.
  while (queue.length && block - queue[0].inclusionBlock >= MAX_WAIT_BLOCKS)
    outputs.push(...applyDirect(queue.shift()!));

  // 2. Classify by sender. Never by looking at the payload.
  if (meta.msgSender !== SEQUENCER) {
    queue.push({ sender: meta.msgSender, inclusionBlock: block, payload });
    return outputs;
  }

  // 3. A batch.
  const batch = decodeBatch(payload);                    // strict SSZ, see below
  if (!batch) return outputs;                            // rejected, nonce kept
  if (batch.nonce !== nextBatchNonce) return outputs;    // rejected, nonce kept
  if (batch.frames.length === 0) { nextBatchNonce++; return outputs; }

  let prev = batch.frames[0].safeBlock;
  for (const f of batch.frames) {                        // structural check
    if (f.safeBlock > block) return outputs;             // rejected, nonce kept
    if (f.safeBlock < prev) return outputs;              // rejected, nonce kept
    prev = f.safeBlock;
  }

  if (block - batch.frames[0].safeBlock >= MAX_WAIT_BLOCKS)
    return outputs;                                      // stale: skipped, nonce kept

  const domain = eip712Domain(meta.chainId, meta.appContract);
  for (const f of batch.frames) {
    while (queue.length && queue[0].inclusionBlock <= f.safeBlock)   // drain first
      outputs.push(...applyDirect(queue.shift()!));
    for (const op of f.userOps) outputs.push(...executeUserOp(domain, f, op));
  }
  nextBatchNonce++;
  return outputs;
}
```

Details that are easy to get wrong:

- The checks run in exactly this order. The first failing check decides.
- **Only an executed batch advances the nonce** — including an empty one. A
  rejected or stale batch leaves it unchanged. The subtraction in the two age
  tests saturates at zero.
- The overdue drain in step 1 happens even when the input then turns out to
  be a rejected batch, and its effects **stay**.
- Staleness looks at the **first** frame only.
- The drain rule is inclusive: `inclusionBlock <= safeBlock`.
- The chain id and application address for the signature domain come from the
  metadata of the input being processed.

### Per user operation

```ts
function executeUserOp(domain, frame, op): Output[] {
  const sender = recoverSigner(domain, op);      // null → skip silently
  if (!sender) return [];
  if (op.maxFee < frame.feePrice) return [];     // protocol guard → skip
  if (engine.validateUserOp(sender, op, frame.feePrice).kind === "reject") return [];
  return engine.applyUserOp(
    { sender, fee: frame.feePrice, data: op.data }, frame.safeBlock);
}

function applyDirect(d: QueuedDirect): Output[] {
  return engine.applyDirectInput(
    { sender: d.sender, blockNumber: d.inclusionBlock, payload: d.payload });
}
```

A skipped operation changes nothing: no nonce, no fee, no counter. After every
successful apply, assert that the engine's counters moved by exactly one input
and that the clock is `max(previous, block)`; the reference does, and stops if
they did not.

### Batch wire format (SSZ)

Integers are little-endian. A variable-length field is represented in its
container's fixed part by a 4-byte offset, measured from the start of that
container. A list of variable-length items is a run of 4-byte offsets
(measured from the start of the list) followed by the items; an empty list is
zero bytes.

```
Batch       nonce: u64 | offset(frames): u32            ‖ frames…
Frame       offset(user_ops): u32 | safe_block: u64 | fee_price: u16   ‖ user_ops…
WireUserOp  nonce: u32 | max_fee: u16 | offset(data): u32 | offset(signature): u32
                                                        ‖ data ‖ signature
```

The definition is [`sequencer-core/src/batch.rs`](../../sequencer-core/src/batch.rs),
decoded by the `ethereum_ssz` crate. Your decoder must **accept and reject
exactly the same byte strings** as that crate: offsets that point into the
fixed part, skip bytes, go backwards, or run past the end are decode failures
there. A lenient TypeScript decoder that accepts a payload Rust
rejects is a fork. Do not assume a general-purpose SSZ library has identical
strictness; fuzz yours against the Rust decoder.

### Signature recovery

- The struct is `UserOp(uint32 nonce,uint16 max_fee,bytes data)`; the domain
  has `name = "CartesiAppSequencer"`, `version = "1"`, `chainId`,
  `verifyingContract`, and no salt. Standard EIP-712 hashing.
- The signature must be exactly 65 bytes: `r ‖ s ‖ v`. Any other length →
  skip.
- `v`: `0`/`1` → parity as is; `27`/`28` → `v − 27`; `35` and above →
  `(v − 35) mod 2`; anything else → skip.
- A high-`s` signature is **accepted**: it is normalized to low-`s` with the
  parity flipped, which recovers the same address. Libraries that reject
  high-`s` by default must be configured not to.
- `r` or `s` equal to zero or not below the curve order, or a failed
  recovery → skip.
- The address is the last 20 bytes of `keccak256` of the uncompressed public
  key without its prefix byte.

### Outputs and inspect

- Emit the outputs returned for an input, in order, as notices and vouchers,
  then finish. Reports are not part of the protocol; the reference emits one
  for a rejected batch as a diagnostic.
- An inspect request whose payload is empty or the ASCII bytes `state` is
  answered with one report containing the engine's canonical state bytes.
  The watchdog compares that report with the file the sequencer serves.

## 3. The machine entry point

The usual rollup loop, with two constraints:

- **Always finish with `accept`**, including for a rejected or stale batch.
  Rejecting an advance request rolls the machine back to before the input,
  which would undo the backstop drain in step 1 and the queueing of direct
  inputs. "Rejected batch" is a scheduler outcome, not a rollup status.
- **An engine failure is fatal.** Do not catch it and continue with
  half-applied state. Let the process fail.

Use `bigint` when reading `block_number` and `chain_id` from the metadata.

## 4. The sequencer host

The sequencer calls the engine through the Rust `Application` trait, and the
provided C ABI only helps a language that can produce a static archive. A
TypeScript engine lives in another process, so something must carry those
calls across. The simplest shape is a **process bridge**: your engine runs as
a long-lived Node process; a Rust type implements `Application` by exchanging
framed messages with it over a pipe or Unix socket. The bridge is
application-independent — written once, it serves any engine in any language
— but it is not provided in this repository.

A JSON-RPC version of exactly this bridge — an "Engine API" for the
sequencer — is sketched in
[`docs/plans/2026-09-remote-engine-protocol.md`](../plans/2026-09-remote-engine-protocol.md).
The messages mirror the trait one to one:

| Request | Response | Trait method |
|---|---|---|
| `validate(sender, nonce, max_fee, data, current_fee)` | `accept` \| `reject(reason, values)` \| `fatal(message)` | `validate_user_op` |
| `apply_user_op(sender, fee, data, safe_block)` | `outputs[]` \| `fatal` | `apply_valid_user_op` |
| `apply_direct(sender, block_number, payload)` | `outputs[]` \| `fatal` | `apply_direct_input` |
| `progress()` | `(count, clock)` | `progress` |
| `create_dump(path)` | `ok` \| `io_error(kind)` \| `fatal` | `create_dump` |
| process start with `--from-dump path` | ready \| `not_found` \| `invalid` | `from_dump` |

What the TypeScript side of that must get right:

- One request at a time, answered in order. No concurrency inside the engine.
- `reject` carries one of the three protocol reasons and its values
  ([`application-model.md`](application-model.md#the-three-outcomes)). An
  exception is `fatal`, never `reject`.
- `create_dump` must be durable before it answers: write the files,
  `fsyncSync` each file, then `fsyncSync` the directory and its parent. It
  must fail if the path already exists.
- Restoring from a missing path must be reported distinctly (`not_found`);
  the sequencer's startup logic depends on telling it apart from corruption.
- The canonical state file inside a dump is produced by the same encoder that
  answers the `state` inspect in the machine.
- The maximum payload size is reported to the Rust side once, without an
  engine instance; keep it equal to what the engine enforces.

Latency is not a concern at this boundary — a local round trip is far below
the acknowledgement budget — but every operation pays for it twice (validate,
then apply), so keep messages binary and small.

### Rewriting the sequencer in TypeScript instead

Nothing on L1 can tell which program produced a batch, so a TypeScript
sequencer is possible in principle. To be compliant it must at least: read
InputBox events at *safe* finality; open frames whose `safe_block` only moves
forward and execute every covered direct input before the frame's operations;
persist each operation before acknowledging it; encode batches bit-exactly and
submit them in nonce order from the dedicated address; never let a batch reach
L1 `MAX_WAIT_BLOCKS` after its first frame's `safe_block`; and, when it cannot
guarantee that, stop, neutralize every pending L1 transaction, and rebuild its
state from L1 before accepting operations again. That last requirement is the
bulk of this repository — the recovery design and its formal models in
[`docs/recovery/`](../recovery/README.md). Treat a rewrite as building a new
security-critical product, not as a port.

## 5. Runtime and performance in the machine

The machine needs a JavaScript runtime built for riscv64 — Node.js, or a
small embeddable engine with your bundle. Two things to measure before
committing:

- **Signature recovery.** Every user operation costs one secp256k1 recovery
  and one EIP-712 hash, executed under emulation. In pure JavaScript this is
  likely to dominate the cost of a batch and sets the ceiling on sustainable
  throughput. A native secp256k1 binding compiled for riscv64 is the usual
  remedy.
- **Runtime parity.** The sequencer host runs the same bundle on a different
  build of the runtime. The determinism rules in section 1 are what make that
  safe; the conformance tests below are what prove it.

## 6. Conformance tests

An independent scheduler is only as trustworthy as its tests against the
reference. In rough order of value:

1. **Scheduler differential test.** Implement a trivial recording engine in
   both Rust and TypeScript (accept everything, log each call). Feed both
   schedulers the same generated input streams — directs, valid batches,
   wrong nonces, non-monotonic frames, stale batches, empty batches, garbage
   payloads, bad signatures — and compare the call logs and final batch
   nonce. The Rust side needs no sequencer, only `sequencer-core`.
2. **The pinned edge cases.** Port each property listed under "Test-pinned
   properties" in
   [`scheduler-semantics.md`](../protocol/scheduler-semantics.md#test-pinned-properties).
3. **Codec vectors.** SSZ batches, EIP-712 hashes, recovered addresses
   (including high-`s` and each `v` form), and `feeToLinear` for every
   exponent, generated from the Rust code and checked in.
4. **Cross-runtime engine test.** The same input stream through the engine on
   the host runtime and inside the machine; compare canonical state bytes and
   outputs after every input
   ([`application-model.md`](application-model.md#a-test-you-should-write-first)).
5. **End to end.** The real sequencer with the bridge, a devnet, the machine
   image, and the watchdog comparing the two.
