# Path C3 in depth: hosting a Cartesi Machine as the sequencer-side engine

[`integration-paths.md`](integration-paths.md) lists this as an option for
languages that cannot be linked into a Rust process. This page works it out:
what the design is, why it is attractive, what would have to be built, and
where the cost lands. Nothing described here exists in the repository; it is
an analysis, not a plan.

## The idea

Today the sequencer's `Application` is native code in its own process, and the
same logic is compiled a second time into the machine image. Path C3 removes
the second compilation: the sequencer's `Application` is an **adapter that
owns a Cartesi Machine instance** running the application's own image, and
forwards every trait call to the program inside it.

```
  sequencer process                                canonical node
  ┌─────────────────────────────────┐              ┌──────────────────────────┐
  │ sequencer library (Rust)        │              │ Cartesi Machine          │
  │   │ Application trait           │              │   scheduler ► ENGINE     │
  │   ▼                             │              └──────────────────────────┘
  │ MachineApp adapter (Rust)       │                          ▲
  │   │ emulator C API              │        same rootfs, same engine binary
  │   ▼                             │                          │
  │ Cartesi Machine                 │                          │
  │   host-mode dispatcher ► ENGINE ├──────────────────────────┘
  └─────────────────────────────────┘
```

The engine bytes, the runtime that interprets them, and the emulator that runs
that runtime are identical on both sides. Agreement is no longer something you
test for; it is what you built.

## Why it is attractive

- **Parity by construction.** Every determinism rule in
  [`application-model.md`](application-model.md#determinism) and
  [`typescript.md`](typescript.md#1-the-engine) — `bigint` discipline, ICU
  differences, runtime version pinning, x86-versus-riscv64 behavior — exists
  because the engine runs on two different platforms. Under C3 it runs on one.
- **Any language, one adapter.** The adapter is application-independent and
  language-independent. Written once, it hosts a TypeScript, Python, Go, or C++
  engine with no FFI shim per application. This is the only path where the
  application developer writes *nothing* on the sequencer host.
- **Checkpoints are the emulator's.** `store` writes a complete machine state;
  `load` restores it. The recovery checkpoint the contract asks for is a
  primitive the emulator already has, and it is the same artifact the watchdog
  already produces for its own compare loop.
- **The genesis state is the machine image.** `setup` loads the stored
  template; there is no separate genesis tool.
- **A host-side driver already exists in miniature.** The watchdog drives a
  machine from the host today — load, feed advance inputs over the cmio
  interface, run the `state` inspect, store —
  in [`watchdog/machine_cartesi.lua`](../../watchdog/machine_cartesi.lua).
  The adapter is that loop in Rust over the emulator's C API, with a
  request/response protocol instead of L1 inputs.

## What the guest must expose: a host mode

The canonical image runs *scheduler → engine* and speaks L1 inputs. The
sequencer does not want that interface: it has already recovered the signer
and decided the frame fee and safe block, and it needs validation as a
separate, read-only step. So the same program needs a second front-end, a
**host-mode dispatcher**, selected by an entrypoint argument or environment
variable baked into the host image:

| Request | Carried as | Reply |
|---|---|---|
| `validate(sender, nonce, max_fee, data, current_fee)` | advance; `msg_sender = sender`; tagged payload | one report: accept, or reject + reason + values |
| `apply_user_op(sender, fee, data, safe_block)` | advance; `msg_sender = sender`, `block_number = safe_block` | notices/vouchers as ordinary outputs, then a report with the new progress |
| `apply_direct(sender, block_number, payload)` | advance; `msg_sender = sender`, `block_number = block` | same |
| `progress()` | inspect | one report: `(count, clock)` |
| `state()` | inspect `state` — identical to the canonical image | one report: canonical state bytes |

Keeping the standard advance framing means the guest's existing rollup loop
(`cmt`/`rollup-init`) is reused unchanged; only the handler behind it differs.
The two images are built from one rootfs and differ only in the entrypoint.
The host image's hash is not consensus-relevant.

Why not drive the unmodified canonical image with synthetic one-operation
batches instead? Three reasons, each fatal on its own: the reference
scheduler's inspect answers only `state`, so there is no validation request;
a direct input fed as an L1 input is *queued*, not executed, so
`apply_direct_input` could not advance progress by one as the contract
requires; and every user operation would pay a redundant signature recovery
under emulation. The dispatcher is the honest design, and it forces the
engine/scheduler separation that every other path also needs.

Validation purity remains self-trusted, exactly as for a native engine: the
guest promises not to change logical state while answering `validate`. The
machine's registers and cycle counter change, its application state does not.
No fork or state discard is needed.

## What the adapter must do

A Rust type `MachineApp` implementing `Application`:

- **`validate_user_op` / `apply_*`**: resume the machine, answer its pending
  input request with the encoded call, run until it asks for the next input,
  collect outputs and reports on the way, decode them. Notices and vouchers
  arrive in the standard output ABI, so decoding is the same as any rollups
  consumer's.
- **`progress()`**: an inspect round trip, or the value the last apply
  reported — either way the engine owns it.
- **`from_dump(prefix)`**: `load` the stored machine under `prefix`. An
  absent directory must surface as `Io(NotFound)`; a corrupt one as
  `Internal` or `InvalidData`.
- **`create_dump(prefix)`**: `store` the machine under `prefix`, run the
  `state` inspect and write its bytes as the canonical state file beside it,
  then `fsync` every file and directory up to and including the parent of
  `prefix`. The emulator's `store` does not promise durability by itself.
- **`state_file_in_dump(prefix)`**: the canonical state file's path — or,
  if the application keeps its canonical state in a dedicated flash drive,
  that drive's image file inside the store. The contract already allows
  either ([`docs/snapshots/format.md`](../snapshots/format.md)).
- **`delete_dump`**: recursive deletion.
- **Errors**: a guest halt, a cmio exception, or a malformed reply is
  `Internal` (terminal — the sequencer stops rather than continue on
  undefined state); an emulator or filesystem failure is `Io` (retryable).
- **A cycle budget** per request. A canonical machine has no gas: an input
  that never terminates stalls the rollup. Bounding `mcycle` per call turns
  that into a loud adapter failure *before* the operation is committed —
  a diagnostic, not a defense, since direct inputs are already on L1.
- **`max_method_payload_bytes()`** is answered without an instance, so the
  adapter learns it from a build-time or startup setting rather than from the
  machine, even though the rest of it is generic.

The machine handle is used by one thread at a time and can move between
threads, which is all the trait requires (`Send`, no `Sync`, no `Clone`).

## Where the cost lands

### Per-operation latency

Every trait call is one emulated round trip: resume the guest, run through
its input loop, execute the handler, yield. A user operation costs two
(validate, then apply). The yield mechanics are cheap; the handler is not —
it is the application's logic interpreted on an emulated riscv64 CPU, usually
one to two orders of magnitude slower than native, and for a TypeScript
engine that is an interpreter running inside an emulator. The one large
saving over the canonical path is that the host never recovers signatures:
the sender arrives already recovered.

The acknowledgement budget is 500 ms end to end and the lane is single-file,
so per-operation cost is also the throughput ceiling. A compiled engine with
small state will fit comfortably; a scripted engine needs measuring before
anything else is decided. After an outage the lane also applies the whole
missed direct-input backlog in one turn
([application contract §5](../protocol/application-contract.md#5-operational-capacity-for-l1-reconciliation)),
emulated.

### Checkpoint size and frequency — the dominant cost

The sequencer creates a checkpoint at **every batch close**, synchronously on
the lane, and the contract requires it to be durable before the call returns.
Batches close on a byte budget derived from L1 gas economics (kilobytes) or a
wall-clock deadline, so under load they close often — every minute or faster.

A machine `store` writes the complete memory image: all of RAM (128 MiB in
the reference image) plus every flash drive. There is no incremental store.
Each batch close would therefore write and `fsync` on the order of a hundred
megabytes or more, with the lane — and every `POST /tx` — stalled for the
duration, and superseded checkpoints of that size accumulating until GC.
Compare the wallet's checkpoint: one SSZ file of a few kilobytes.

This is the item most likely to decide C3's viability. Mitigations are
real but limited: shrink the machine (RAM length, rootfs) as far as the
runtime allows; keep application state in a dedicated drive so it can be
served directly as the canonical file; put the data directory on fast local
storage. None of them avoids writing RAM. The contract cannot help either:
`create_dump` is synchronous by design, and forking the machine to store from
a copy would not shorten the wait, because durability must be established
before the lane continues.

### Restored-instance independence

`from_dump` instances must stay usable after the source checkpoint is
garbage-collected. The emulator maps drive images from their files; on Linux,
unlinking a mapped file keeps its data alive until unmapped, so recursive
deletion is safe in principle — but this must be verified against the
emulator version and the mapping mode actually used, not assumed.

### Recovery and the watchdog

Because the sequencer's dumps would be machine checkpoints, one might expect
the watchdog's canonical checkpoints to become interchangeable with them.
They are not, quite: a canonical machine at block `B` holds unexecuted direct
inputs in its scheduler queue and runs in scheduler mode; a host-mode machine
holds only application state and progress. Recovery reconstructs the pending
range from the application's clock
([application contract §3](../protocol/application-contract.md#3-the-safe-block-clock--last_executed_safe_block))
and expects a host-mode engine. Transplanting application state from a
canonical checkpoint into a host-mode machine is possible if that state lives
in a dedicated drive, but it is additional work, not a free consequence —
though it is work every application owes anyway, since a canonical-to-native
export tool is a
[production requirement](../protocol/application-contract.md#7-canonical-recovery-integration).
Under C3 that exporter is unusually simple: same image, same drive.

## What would have to be built

1. The host-mode dispatcher in the application (small; application side).
2. `MachineApp` over the emulator's C API (medium; application-independent):
   request framing, output ABI decoding, cycle budget, error classification,
   store/load with fsync, state file.
3. Image build producing the canonical and host entrypoints from one rootfs,
   with the stored host template used as `setup`'s genesis.
4. Conformance: run the reference wallet in host mode inside a machine and
   compare it, operation by operation, against the native wallet through the
   existing test suites.
5. Measurements before committing: round trip per call for the real engine;
   `store` time and size at the chosen machine size; backlog replay rate.

## Where C3 fits

Against the process bridge in [`typescript.md`](typescript.md#4-the-sequencer-host)
the trade is symmetric: the bridge runs the engine at native speed with
kilobyte checkpoints and buys parity through discipline and tests; C3 runs it
emulated with hundred-megabyte checkpoints and gets parity for free.

Two uses follow:

- **As a production engine** for applications whose throughput is modest and
  whose batches close slowly — where a multi-second stall per batch close is
  acceptable — and whose team values never having to think about
  cross-platform determinism.
- **As a conformance oracle** for everyone else. The same adapter, used only
  in tests, feeds the same operations to the native engine (or the bridge)
  and to the machine-hosted one and compares canonical state bytes after
  every step. That is the cross-runtime test recommended at the end of
  [`application-model.md`](application-model.md#a-test-you-should-write-first),
  with the machine side driven exactly as the sequencer would drive it. It
  captures most of C3's value at no production cost, and it is the first
  thing to build if C3 is pursued at all.
