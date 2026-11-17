# Sequencer

A sequencer for Cartesi app-specific rollups. Provides low-latency soft confirmations for user operations, posts them to L1 in batches, and exposes its current application execution order for replica replay.

**Security-critical infrastructure.** Handle every change with the care financial systems demand.

## What It Does

Rollup applications need fast transaction confirmations. Waiting for L1 finality on every user action (minutes) makes interactive applications impractical. The sequencer bridges this gap: it accepts signed user operations, immediately confirms them (soft confirmation), and asynchronously posts batches to L1. The application sees these batches posted on chain.

The protocol objective is that, under supported honest operation, **the
off-chain sequencer predicts the same execution order the rollup's on-chain
scheduler later produces.** The scheduler is the program inside the
application's Cartesi Machine that orders every input from L1. Soft
confirmations are optimistic and may be invalidated by the recovery cases
below; L1 remains canonical truth.

## What This Repository Contains

The sequencer is a Rust library. An application crate composes it with its
`Application` implementation into a binary, as
[`examples/wallet-sequencer/`](examples/wallet-sequencer/) does with the toy
wallet in [`examples/app-core/`](examples/app-core/), the example application
for development, tests, and the Sepolia reference deployment. Native engines
bind through the [C ABI adapter](bindings/c-app-engine/README.md).

Alongside it are the canonical guest that runs the scheduler on chain
([`examples/canonical-app/`](examples/canonical-app/)), a Rust client
([`sdk/rust-client/`](sdk/rust-client/)), and the
[watchdog](docs/watchdog/README.md), which re-derives canonical state from L1
and compares it with the sequencer's. Releases ship `wallet-sequencer`
binaries, canonical machine images, and the
[watchdog container image](docs/watchdog/operator-deployment.md#release-image). The
[architecture map](AGENTS.md#architecture-map) covers the whole workspace.

## Two Chains Synchronizing

The sequencer maintains an optimistic chain of batches — a tree that normally degenerates into a list. Each batch contains frames, and each frame contains user operations plus a `safe_block` reference. The `safe_block` is the synchronization primitive: it tells the on-chain scheduler "drain all direct inputs (deposits) up to this L1 block, then execute these user ops." Both sides follow the rule, producing identical state.

```
Sequencer (off-chain)              Scheduler (on-chain)
  frame: safe_block=100               drain directs up to block 100
         user_ops=[A, B, C]           execute A, B, C
  frame: safe_block=105               drain directs up to block 105
         user_ops=[D]                 execute D
```

The sequencer opens a frame at a new `safe_block` once five newly-safe L1 blocks
have accumulated ([frame-clock policy](docs/protocol/scheduler-semantics.md#sequencer-frame-clock-policy)).

A batch is **stale** when it lands on L1 `MAX_WAIT_BLOCKS` (1200 blocks, ~4h)
or more after its first frame's `safe_block`. The scheduler skips a stale batch
without consuming its nonce, so every later batch is rejected too.

When things go well, the sequencer's chain and the scheduler's view converge.
When batches risk becoming stale on L1, the sequencer stops serving and startup
determines the required repair. A lost or untrustworthy local state instead
requires an operator rebuild. Both recovery modes are described below.

## Trust Model

The sequencer is a **centralized, single-writer** system. It cannot steal funds or forge invalid state — the rollup validates everything independently, and the proof system later enforces it. But the sequencer can:

- **Censor** — refuse to include a user's operations.
- **Go offline** — stop providing soft confirmations.
- **Diverge** — if batches fail to land on L1 in time, soft confirmations that were issued become invalid.

**Direct inputs** (L1 → L2 messages, used for deposits) bypass the sequencer entirely. They are posted directly to L1 and are **uncensorable** by the sequencer — the scheduler drains them at every `safe_block` boundary. A censoring sequencer can delay when a direct input is executed (up to `MAX_WAIT_BLOCKS`, ~4h), but cannot prevent it.

The [threat model](docs/threat-model/README.md) owns the full set of actors and
assumptions.

## Recovery

**[Automatic recovery](docs/recovery/README.md)** (also called standard
recovery) runs at startup using the existing database. It handles liveness failures such as outages and
extended downtime: reconcile L1 outcomes, invalidate the affected optimistic
suffix, and resume from retained state. Stale batches do not consume the
scheduler's expected nonce, so their successors cannot be accepted until recovery
supplies a replacement at that nonce.

**[Cockroach recovery](docs/recovery/cockroach.md)** is an operator-triggered
rebuild when the local database is lost or cannot be trusted. This includes a
sequencer bug that corrupted state or emitted malformed batches: fix the bug,
choose a trusted canonical application checkpoint, then rebuild in a fresh data
directory. The command processes historical L1 inputs through the canonical
scheduler and prepares a baseline for resuming normal operation.

Soft confirmations are an **optimistic prediction**. A content-identity check
compares each batch that L1 accepts at the sequencer's nonces with the batch it
sealed locally; a foreign or byte-different landing stops the sequencer until an
operator performs cockroach recovery. The check does not replace the
[watchdog](docs/watchdog/README.md); [I9 and I15](docs/invariants.md) record its
bounds.

The [threat model](docs/threat-model/README.md#self-trust) explains the boundary
between normal operation's self-trust and manual repair after a bug.

## Failure Modes

The sequencer is designed to handle:

- **L1 provider outages** — workers retry on their next tick while the inclusion lane and API keep operating locally ([provider behavior](docs/running.md#the-l1-endpoint)). A wall-clock fallback detects when an outage pushes batches into the danger zone.
- **Undiagnosed interruptions (OOM, SIGKILL, reboot)** — restart can recover automatically: every boot derives any required recovery from SQLite and L1 safe state through startup recovery, never assuming the previous exit was clean.
- **Extended downtime** — startup syncs to the current L1 safe head, flushes if needed, and recovers before admission. A terminal exit requires operator investigation; rebuilding untrustworthy state follows the cockroach recovery procedure above.
- **Adversarial L1 mempool** — block builders and private mempools are treated as adversarial. Recovery waits until every covered wallet-nonce slot is consumed at safe depth, whether the original transaction or a flush no-op wins, so delayed "zombie" submissions cannot land later.

## Interfaces

The [API contract](docs/api.md) owns routes, listeners, message shapes, and
status codes. Only the four wallet-facing routes (`POST /tx`, `GET /fee`,
`GET /nonce`, and `GET /domain`) are public; every other route is internal,
unauthenticated, and must stay off the public network.

### User Operations

Users submit signed operations via `POST /tx` (JSON). Operations are signed with EIP-712 using the rollup's chain ID and app address. The sequencer validates the signature, executes the operation against the current app state, and returns a soft confirmation. To sign, a wallet reads `GET /fee` (the live frame fee, the next-frame recommendation, and a suggested `max_fee`), `GET /nonce?sender=` (the nonce to sign next), and `GET /domain` (the EIP-712 domain, to assert against the one it pins).

### Sequenced Transaction Feed

Subscribers restore an HTTP snapshot, then use one WebSocket stream to replay
application inputs and follow the optimistic tip. Recovery can replace that
history; the snapshot's era, generation, and input count bind a resume request
to the state the consumer actually holds. The endpoint serves a small number of
infrastructure subscribers, which serve users directly. See the
[bootstrap workflow](docs/protocol/application-history.md#replica-bootstrap-and-resume)
and [wire contract](docs/api.md).

### Batch Submission

The batch submitter posts closed batches to L1's InputBox contract. Each batch carries a sequential nonce for deduplication; L1 wallet nonces guarantee ordering. The submitter is stateless — it derives pending work from SQLite and L1 state each tick.

## Running

`setup` pins the deployment identity and initial L1 state, and runs once; `run`
boots the sequencer from that database and holds the batch-submitter key.
[Running the sequencer](docs/running.md) covers invocation, configuration, the
L1 endpoint, exit codes, and upgrades.

To try it locally, run `just devnet`. It starts Anvil with the rollups
contracts and a set-up `wallet-sequencer-devnet`, prints their endpoints, and
runs until Ctrl+C. The first run downloads dependencies and builds the devnet
machine image, which needs the tools in the
[devnet prerequisites](docs/watchdog/getting-started.md#prerequisites); the Nix
development shell provides them.

## Storage Model

- `batches`: batch metadata
- `frames`: frame boundaries within each batch
- `frames.fee`: committed fee for each frame
- `user_ops`: included user operations
- `application_inputs`: current application sequence keyed by mandatory pre-execution offset; each row references a user op or an external direct input and its owning batch/frame
- `safe_inputs`: every raw InputBox observation, including batch envelopes
- `history_state`: immutable era baseline (application count and accounted L1 block) plus recovery generation
- `snapshots` and `dumps`: immutable application-state artifacts, one registered atomically with each batch seal plus the era baseline, and their streaming leases. Restart restores the latest valid snapshot and replays application inputs from its count; acceptance, derived from `safe_accepted_batches`, selects the recovery checkpoint and the garbage-collection frontier ([snapshot lifecycle](docs/snapshots/lifecycle.md)).
- `batch_policy`: singleton fee and batch-sizing knobs; `batch_policy_derived` exposes `recommended_fee` and `batch_size_target` ([formula](sequencer/src/storage/migrations/0001_schema.sql)). A batch closes when its `batch_size_target` byte budget fills or `CARTESI_SEQUENCER_MAX_BATCH_OPEN_SECONDS` elapses, whichever comes first. The [threat model](docs/threat-model/README.md#fee-oracle) owns how the oracle prices and refreshes.

The schema is one baseline migration; a release that changes it is
[upgraded by rebuilding](docs/running.md#upgrades).

## Development

The shared [development commands](AGENTS.md#shell-and-commands) cover Rust
toolchain selection, Nix/direnv tooling, compilation, tests, formatting, and
linting. Read the [testing guidance](AGENTS.md#testing-guidance) before choosing
validation for a change; some tests require Anvil or libslirp.

## Documentation

[`AGENTS.md`](AGENTS.md) is the developer guide: architecture, conventions, and
rules. Its [Reading Routes](AGENTS.md#reading-routes) name the document that
owns each contract. The main entry points:

| Topic | Document |
|---|---|
| Invocation, configuration, exit codes, and upgrades | [Running the sequencer](docs/running.md) |
| Routes, messages, and status codes | [API contract](docs/api.md), rendered as [`openapi.yaml`](openapi.yaml) |
| How the scheduler orders inputs | [Scheduler semantics](docs/protocol/scheduler-semantics.md) |
| What an application must implement | [Application contract](docs/protocol/application-contract.md) |
| Repair after outages or lost state | [Automatic recovery](docs/recovery/README.md) and [cockroach recovery](docs/recovery/cockroach.md) |
| Actors, assumptions, and residual risks | [Threat model](docs/threat-model/README.md) |
| Independent checking of the sequencer's state | [Watchdog](docs/watchdog/README.md) |

## License

Apache-2.0. See [`LICENSE`](LICENSE). Authors in [`AUTHORS`](AUTHORS).
