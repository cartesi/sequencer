# Hosting your application: paths by language

[`application-model.md`](application-model.md) describes the *engine* your
application logic must become. This page is about where that engine runs and
what connects it, and it is candid about which connections exist in this
repository and which you would have to build.

## Two hosts, one engine

```
                       ┌──────────────────────────────────────────┐
  POST /tx ──────────► │ SEQUENCER HOST  (a server you operate)    │
                       │   sequencer library (Rust)                │
                       │        │ Application trait                │
                       │        ▼                                  │
                       │   YOUR ENGINE  ── checkpoints on disk     │
                       └───────────────┬──────────────────────────┘
                                       │ batches
                                       ▼
                                  L1 InputBox  ◄── deposits (portals)
                                       │
                       ┌───────────────▼──────────────────────────┐
                       │ CARTESI MACHINE  (riscv64, canonical)     │
                       │   rollup device ► scheduler               │
                       │        │ same calls, same order           │
                       │        ▼                                  │
                       │   YOUR ENGINE  ── notices, vouchers       │
                       └──────────────────────────────────────────┘
```

**In the machine**, the scheduler owns the rollup request loop. For every
advance request it decides — by `msg_sender` — whether the input is a batch
from the sequencer or a direct input; it queues direct inputs, unpacks batches,
recovers each user operation's signer, applies the ordering rules, and calls
your engine. It emits the outputs your engine returns. It answers the `state`
inspect query with your engine's canonical state bytes.

**On the sequencer host**, the sequencer library owns HTTP, ordering, storage,
L1 submission, and recovery. It calls your engine through the Rust
[`Application`](../../sequencer-core/src/application/mod.rs) trait to validate
and apply operations as they arrive, and to create and restore checkpoints.

The two hosts must drive **behaviorally identical** engines, and must be
configured for the same deployment: same genesis state, same application
parameters (token addresses, portal addresses, fee recipient), and the
scheduler must know the sequencer's L1 address — it is how batches are
recognized.

## What exists

| Component | In this repository | Language |
|---|---|---|
| Sequencer host | [`sequencer/`](../../sequencer/) — a library; your binary calls `sequencer::run_main` | Rust |
| Host ↔ engine interface | [`Application`](../../sequencer-core/src/application/mod.rs) trait | Rust |
| C ABI for the engine | [`application-engine.h`](../../bindings/c-app-engine/include/application-engine.h), the [`c-app-engine`](../../bindings/c-app-engine/README.md) adapter, and the `c-app-sequencer` host binary | C header; Rust adapter |
| C reference engine + conformance tests | [`examples/c-wallet-engine/`](../../examples/c-wallet-engine/), [`examples/c-wallet-sequencer/`](../../examples/c-wallet-sequencer/) | Rust exporting C |
| Scheduler | [`Scheduler<A>`](../../sequencer-core/src/scheduler/mod.rs), generic over `Application` | Rust |
| Machine harness (rollup loop around the scheduler) | [`examples/canonical-app/`](../../examples/canonical-app/src/scheduler/mod.rs) | Rust |
| Machine image build | [`examples/canonical-app/justfile`](../../examples/canonical-app/justfile) | — |
| Reference engine | [`examples/app-core/`](../../examples/app-core/) (wallet) | Rust |
| End-to-end tests of the pair | [`examples/canonical-test/`](../../examples/canonical-test/), [`tests/e2e/`](../../tests/e2e/) | Rust |

Both hosts reach the engine through one Rust trait. For languages with a C
ABI, the repository provides the sequencer-host side of the adapter: a header
your engine implements and a host binary that links it. The machine side
still needs a few lines of Rust around the reference scheduler. There is no
TypeScript or Python binding, and no adapter that runs a Cartesi Machine as
the sequencer-side engine.

## Path A — Rust

The supported path. Follow the wallet example:

1. **Engine crate.** Implement `Application` (and `CanonicalState`, which
   supplies the bytes for the `state` inspect) for your state type, as
   [`wallet.rs`](../../examples/app-core/src/application/wallet.rs) does.
2. **Sequencer binary.** A few lines, as in
   [`examples/wallet-sequencer/src/main.rs`](../../examples/wallet-sequencer/src/main.rs):

   ```rust
   #[tokio::main]
   async fn main() -> std::process::ExitCode {
       sequencer::run_main(|| Ok(MyApp::genesis(MyConfig::mainnet()))).await
   }
   ```

   The closure builds the genesis state and runs only during plain `setup`;
   it returns a `Result` so an engine that loads genesis from a file can fail
   cleanly. The resulting binary has the `setup`, `run`, and `flush-mempool`
   subcommands described in the project [`README.md`](../../README.md#running).
3. **Machine binary.** Equally short, as in
   [`canonical-app-devnet.rs`](../../examples/canonical-app/src/bin/canonical-app-devnet.rs):

   ```rust
   fn main() {
       let rollup = RollupCmt::try_new().expect("failed to initialize rollup");
       let app = MyApp::genesis(MyConfig::mainnet());
       run_scheduler_forever(rollup, app, SchedulerConfig::new(SEQUENCER_ADDRESS));
   }
   ```

   Cross-compile it for `riscv64gc-unknown-linux-musl` and build the machine
   image the way the canonical-app `justfile` does.

Because both hosts compile the same Rust source, agreement between them comes
mostly for free. What remains your responsibility is determinism across
architectures and the canonical state encoding.

## Path B — Go, C, C++, and other languages with a C ABI

These languages can produce a static library with C-callable functions. The
repository defines the functions to export —
[`application-engine.h`](../../bindings/c-app-engine/include/application-engine.h),
one entry point per method in
[`application-model.md`](application-model.md#from-a-request-loop-to-a-state-machine)
plus open/destroy/drain-output — and ships the Rust side that calls them.

**Sequencer host.** Nothing to write in Rust. Implement the header in your
language, build a static archive for the host's architecture, and build the
provided host binary against it:

```bash
APPLICATION_ENGINE_LIB=/abs/path/libengine.a APPLICATION_ENGINE_HEADER=/abs/path/application-engine.h cargo build -p c-app-sequencer
```

The build needs libclang (bindings are generated from the header). Genesis
state is produced by a tool of yours and passed as `--state-file` to plain
`setup`; deployment configuration never crosses the ABI. The
[build guide](../../bindings/c-app-engine/README.md) covers depending on the
host from another repository and the conformance suite, which you should run
against your engine before anything else.

**Machine.** The adapter type `EngineApp` implements `Application`, so the
reference scheduler can drive your engine inside the machine too — but the
machine harness also needs `CanonicalState` (the bytes for the `state`
inspect), which `EngineApp` does not implement. You write a small Rust
wrapper: `EngineApp` plus a `CanonicalState` impl that obtains your canonical
bytes, wrapped in `run_scheduler_forever` as in Path A, cross-compiled and
linked against your archive built for riscv64. The repository's end-to-end
tests exercise the C engine on the sequencer host against the *Rust* wallet
image; running `EngineApp` inside a machine image is yours to validate.

The header documents its own rules in detail. The ones a casual
implementation gets wrong:

- **Three validation results**: `OK`, `INVALID` with a reason record, and a
  failure status. Never report bad input as `INTERNAL_ERROR` — that is fatal
  for the whole sequencer — and never report an engine fault as `INVALID`.
- **Every entry point is total** over the bytes it is handed, and no
  exception or panic may cross the ABI.
- **One handle, one caller at a time**; it may move between threads.
- **Progress is the engine's**: `application_engine_progress` reports the
  count and clock from your own state.
- **`NOT_FOUND` and `INVALID_DUMP`** on open must stay distinct from
  `IO_ERROR`; startup uses the distinction.
- **Outputs are drained** after each execution, exactly as many as it
  reported, and buffers you hand out are copied before the next call.
- **Checkpoints are durable before you return**, immutable afterwards, and
  entirely under the given prefix; the sequencer deletes them itself.

Go specifics: build the engine with `-buildmode=c-archive` and export the
header's functions through cgo; a Go panic must be recovered at every export.
Confirm that your Go toolchain supports that build mode for `linux/riscv64`
before committing to this path. The Go runtime brings
its own scheduler and garbage collector into both processes; neither affects
results if execution stays on one goroutine and avoids the pitfalls in
[Determinism](application-model.md#determinism), but measure the latency impact
of GC pauses on the sequencer host.

## Path C — TypeScript, Python, and other interpreter-hosted languages

There is no ready path. These languages cannot be linked into a Rust process
as a library, and the engine must run in two places. The realistic options:

### C1. Port the engine, keep everything else

Move the state-transition core — often a small fraction of the codebase — to
Rust (Path A) or Go/C++ (Path B). Frontend, indexer, APIs, and tooling stay in
TypeScript. This is the lowest-risk option and the only one that needs no new
infrastructure. If the core is small, it is also the cheapest.

### C2. Embed the interpreter

Link an embeddable interpreter (for JavaScript, an engine such as QuickJS) into
a Rust shim, load your bundled engine code into it, and implement
`Application` by calling into it — on both hosts. Structurally this is Path B
with the interpreter as the "C library".

What you take on: the exact same interpreter version and build options on both
sides; an interpreter whose behavior is deterministic across x86/ARM and
riscv64 (number formatting, sort stability, `bigint` arithmetic); a checkpoint
story (serialize engine state to canonical bytes and rebuild the interpreter
on restore, rather than snapshotting the interpreter heap); and performance —
an interpreter on the emulated riscv64 machine is slow, and the machine must
still keep up with every batch. Node.js-specific APIs will not be available.

### C3. Run the machine on the sequencer host

Implement `Application` with an adapter that drives a Cartesi Machine instance
running the *same image* as the canonical machine: apply an operation by
feeding it to the machine, checkpoint by snapshotting the machine alongside a
canonical state file. The contract allows this shape — a checkpoint may be a
directory holding a full machine state plus the state file
([`docs/snapshots/format.md`](../snapshots/format.md)).

Its appeal is that both hosts run one binary, in any language, so agreement is
by construction. Its costs are substantial and unproven here: no such adapter
exists; emulated execution has to fit the sub-second acknowledgement budget
for every operation; validation must be runnable without committing state; and
machine snapshots are taken on the same lane that serves users. Inside the
machine you would also need the scheduler in front of your code (next
section). Treat this as a research project, not a migration step.

## The scheduler when your machine code is not Rust

On Paths B and C2 the machine binary is a Rust program (scheduler + adapter)
with your engine linked in, so the reference scheduler is used as is.

If instead you want your existing in-machine program to keep owning the rollup
loop, the scheduler's job has to be done in your language. That means
reimplementing, bit for bit:

- classification of inputs by `msg_sender`;
- SSZ decoding of `Batch` → `Frame` → user operation
  ([`batch.rs`](../../sequencer-core/src/batch.rs));
- EIP-712 hashing with the input's chain id and application address, and
  secp256k1 signer recovery — operations with unrecoverable signatures are
  skipped;
- the batch nonce, the structural frame checks, the staleness rule, and the
  forced execution of overdue direct inputs;
- per frame: execute queued direct inputs up to `safe_block`, then the frame's
  user operations, each through the `max_fee` guard and your validation;
- the bit-exact fee conversion.

The algorithm is specified in
[`docs/protocol/scheduler-semantics.md`](../protocol/scheduler-semantics.md),
together with the list of edge cases the reference tests pin. This is
consensus-critical code: a discrepancy is not a bug users see, it is a fork
between what the sequencer promised and what the machine computed. Prefer
reusing the Rust scheduler; if you port it, port its tests first.

## Configuration that must agree

| Value | Sequencer host | Machine |
|---|---|---|
| Chain id | `setup` environment | Read from each input's metadata |
| Application address | `setup` environment | Read from each input's metadata |
| Sequencer (batch submitter) L1 address | `setup` environment; `run` holds its key | Compiled into the image via `SchedulerConfig` |
| Application parameters and genesis state | The genesis closure (Rust) or `--state-file` (C ABI) | Compiled into the image |
| `MAX_WAIT_BLOCKS`, EIP-712 domain name and version | From `sequencer-core` | From `sequencer-core` — or your port |

The sequencer's L1 address is part of the machine image, and therefore of the
machine's initial hash. Decide it, and how its key is managed, before you
deploy the application contract. Use a dedicated address for it.

## Trying it locally

The quickest way to see all the pieces interact is to run the wallet example
before changing anything. From the repository root, inside the Nix/direnv
environment:

```bash
just setup
```

```bash
just canonical-build-machine-image
```

```bash
just test-rollups-e2e
```

The end-to-end suite starts Anvil with the rollups contracts, runs the
wallet sequencer, deposits through the portal, submits signed operations,
restarts and replays, and compares the sequencer's state with the machine's.
[`docs/watchdog/getting-started.md`](../watchdog/getting-started.md) shows how
to keep the stack running interactively. Once that works, substitute your
engine for `app-core` and keep the same tests.

## Operating it

Running a sequencer is an operational commitment that an L1-only application
does not have: a server with a funded L1 key, persistent storage, a supervisor
that honors the exit-code contract, an L1 RPC endpoint, an indexer, and the
watchdog. The project [`README.md`](../../README.md#running) covers
configuration and exit codes;
[`docs/watchdog/operator-deployment.md`](../watchdog/operator-deployment.md)
covers production deployment. Before going live you also owe the rebuild
drill: a versioned tool that converts a trusted machine checkpoint into a
sequencer checkpoint, and a rehearsed `setup --recovery` from a non-genesis
checkpoint ([`application-model.md`](application-model.md#rebuilding-from-the-machine),
[`docs/recovery/cockroach.md`](../recovery/cockroach.md#recovery-readiness-before-deployment)).
If the sequencer is down, users cannot transact quickly, but their funds are
not at risk and deposits still land.
