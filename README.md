# Sequencer

A sequencer for Cartesi app-specific rollups. Provides low-latency soft confirmations for user operations, posts them to L1 in batches, and exposes its current application execution order for replica replay.

**Security-critical infrastructure.** Handle every change with the care financial systems demand.

## What It Does

Rollup applications need fast transaction confirmations. Waiting for L1 finality on every user action (minutes) makes interactive applications impractical. The sequencer bridges this gap: it accepts signed user operations, immediately confirms them (soft confirmation), and asynchronously posts batches to L1. The application sees these batches posted on chain.

The protocol objective is that, under supported honest operation, **the
off-chain sequencer predicts the same execution order the rollup's on-chain
scheduler later produces.** The scheduler is the program inside the
application's Cartesi Machine that orders every input from L1. Soft
confirmations are optimistic and may be revoked by the recovery described
below; L1 remains canonical truth.

## What This Repository Contains

The sequencer is a Rust library. An application crate composes it with its
`Application` implementation into a binary, as
[`examples/wallet-sequencer/`](examples/wallet-sequencer/) does with the toy
wallet in [`examples/app-core/`](examples/app-core/), the example application
for development, tests, and the Sepolia reference deployment. Native engines
bind through the [C ABI adapter](bindings/c-app-engine/README.md). A C or C++
guest reaches the canonical scheduler through the proposed
[`scheduler.h`](bindings/c-scheduler/README.md), and a port of it is held to the
[conformance vectors](docs/protocol/scheduler-semantics.md#conformance).

Alongside it are the canonical guest that runs the scheduler on chain
([`examples/canonical-app/`](examples/canonical-app/)), a Rust client
([`sdk/rust-client/`](sdk/rust-client/)), and the
[watchdog](docs/watchdog/README.md), which re-derives canonical state from L1
and compares it with the sequencer's. Releases ship `wallet-sequencer`
binaries, canonical machine images, and the
[watchdog container image](docs/watchdog/operator-deployment.md#release-image). The
[architecture map](AGENTS.md#architecture-map) covers the whole workspace.

## Two Chains Synchronizing

The sequencer maintains an optimistic list of batches, which L1 accepts in order. Each batch contains frames, and each frame contains user operations plus a `safe_block` reference. The `safe_block` is the synchronization primitive: it tells the on-chain scheduler "drain all direct inputs (deposits) up to this L1 block, then execute these user ops." Both sides follow the rule, producing identical state.

```
Sequencer (off-chain)              Scheduler (on-chain)
  frame: safe_block=100               drain directs up to block 100
         user_ops=[A, B, C]           execute A, B, C
  frame: safe_block=105               drain directs up to block 105
         user_ops=[D]                 execute D
```

The sequencer opens a frame at a new `safe_block` once five newly-safe L1 blocks
have accumulated ([frame-clock policy](docs/protocol/scheduler-semantics.md#sequencer-frame-clock-policy)).

A batch is **stale** when it lands on L1 the application's `max_wait_blocks`
(7200 blocks, ~24h, for the wallet) or more after its first frame's
`safe_block`. The scheduler skips a stale batch
without consuming its nonce, so every later batch is rejected too.

When things go well, the sequencer's chain and the scheduler's view converge.
When a batch can no longer land before it goes stale, or local state is lost or
untrustworthy, an operator rebuilds the sequencer, as described below.

## Trust Model

The sequencer is a **centralized, single-writer** system. It cannot steal funds or forge invalid state — the rollup validates everything independently, and the proof system later enforces it. But the sequencer can:

- **Censor** — refuse to include a user's operations.
- **Go offline** — stop providing soft confirmations.
- **Diverge** — if batches fail to land on L1 in time, soft confirmations that were issued become invalid.

**Direct inputs** (L1 → L2 messages, used for deposits) bypass the sequencer entirely. They are posted directly to L1 and are **uncensorable** by the sequencer — the scheduler drains them at every `safe_block` boundary. A censoring sequencer can delay when a direct input is executed (up to the application's `max_wait_blocks`, ~24h for the wallet), but cannot prevent it. A direct whose payload is over the application's cap never executes, on L1 or in the sequencer.

The [threat model](docs/threat-model/README.md) owns the full set of actors and
assumptions.

## Recovery

The sequencer has one recovery, **[cockroach recovery](docs/recovery/cockroach.md)**:
an operator-triggered rebuild when the era has ended or the local database is
lost or cannot be trusted, including after a sequencer bug that corrupted state
or emitted malformed batches. The operator
runs `setup --base <machine>` in a new data directory: setup rebuilds the
history from a trusted canonical machine and L1 through a stop block and
publishes a new era, and each client realigns at its own count.

An era ends when its oldest unaccepted batch can no longer land before it goes
stale, the **void history**: no later batch can then be accepted, so the
sequencer stops and refuses every start
([batch landing and outages](docs/recovery/README.md)).

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

- **L1 provider outages** — workers retry on their next tick while the inclusion lane and API keep operating locally ([provider behavior](docs/running.md#the-l1-endpoint)). The sequencer acts only on what L1 shows: it reports the safe head's age for monitoring and makes no wall-clock estimate ([outages](docs/recovery/README.md#when-batches-are-not-landing)).
- **Undiagnosed interruptions (OOM, SIGKILL, reboot)** — restart resumes: every boot syncs L1 and checks the terminal facts from SQLite and L1 safe state, never assuming the previous exit was clean ([what the sequencer does](docs/recovery/README.md#what-the-sequencer-does)). Terminal errors returned through a command bracket best-effort record their cause in `terminal_faults`, except a terminal fact, which the database already holds and every `run` states; terminal runtime aborts leave only process diagnostics.
- **Extended downtime** — startup syncs to the current L1 safe head and resumes. A stop longer than the window ends the era, and every start refuses until cockroach recovery, above.
- **Adversarial L1 mempool** — block builders and private mempools are treated as adversarial. No batch nonce is reused inside an era, so a delayed "zombie" submission carries a batch the sequencer still holds. Before a rebuild, setup's flush replaces what the provider's pool holds for the submitter; a submission of the old era that escapes it is refused by setup's escape check or, if it lands later, read as a foreign batch that ends the new era ([the flush and the stop block](docs/recovery/cockroach.md#the-flush-and-the-stop-block)).

## Interfaces

The [API contract](docs/api.md) owns routes, listeners, message shapes, and
status codes. Only the four wallet-facing routes (`POST /tx`, `GET /fee`,
`GET /nonce`, and `GET /domain`) are public; every other route is internal,
unauthenticated, and must stay off the public network.

### User Operations

Users submit signed operations via `POST /tx` (JSON). Operations are signed with EIP-712 using the rollup's chain ID and app address. The sequencer validates the signature, executes the operation against the current app state, and returns a soft confirmation. To sign, a wallet reads `GET /fee` (the live frame fee, the next-frame recommendation, and a suggested `max_fee`), `GET /nonce?sender=` (the nonce to sign next), and `GET /domain` (the EIP-712 domain, to assert against the one it pins).

### Sequenced Transaction Feed

Subscribers restore an HTTP snapshot, then use one WebSocket stream to replay
application inputs and follow the optimistic tip. A cockroach rebuild starts
a new era; the snapshot's era and input count bind a resume request
to the state the consumer actually holds. The endpoint serves a small number of
infrastructure subscribers, which serve users directly. See the
[bootstrap workflow](docs/protocol/application-history.md#replica-bootstrap-and-resume)
and [wire contract](docs/api.md).

### Batch Submission

The batch submitter posts closed batches to L1's InputBox contract. Each batch carries a sequential nonce for deduplication; L1 wallet nonces guarantee ordering. The submitter is stateless — it derives pending work from SQLite and L1 state each tick.

## Running

`setup` creates a data directory, once: it pins the deployment identity,
flushes the submitter's wallet, and rebuilds the history from a base through a
stop block. `run` boots the sequencer from that directory. Both take the
batch-submitter key. [Running the sequencer](docs/running.md) covers
invocation, configuration, the L1 endpoint, exit codes, supervision, and
upgrades.

To try it locally, run `just devnet`. It starts Anvil with the rollups
contracts and a set-up `wallet-sequencer-devnet`, prints their endpoints, and
runs until Ctrl+C. The first run downloads dependencies and builds the devnet
machine image, which needs the tools in the
[devnet prerequisites](docs/watchdog/getting-started.md#prerequisites); the Nix
development shell provides them.

## Storage Model

- `batches`: one row per batch, keyed by its nonce, with the safe input that carried its accepted landing (`accepted_input`)
- `frames`: frame boundaries within each batch
- `frames.fee`: committed fee for each frame
- `user_ops`: included user operations
- `application_inputs`: current application sequence keyed by mandatory pre-execution offset; each row references a user op or an external direct input and its owning batch/frame
- `safe_inputs`: every raw InputBox observation, including batch envelopes
- `era`: one immutable row (base count, stop block, rebuilt history end, first queued input, anchor batch nonce); it exists exactly when setup has completed
- `rebuilt_history`: the transactions setup executed from the base through the stop block, each with its count; the feed serves them before the live rows
- `snapshots`: one immutable row per batch-close or baseline artifact (its directory and count), registered atomically with each batch seal; download leases live in the serving process. Restart restores the latest valid snapshot and replays application inputs from its count; acceptance, read from `batches.accepted_input`, selects the comparison checkpoint and the garbage-collection frontier ([snapshot lifecycle](docs/snapshots/lifecycle.md)).
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
| Invocation, configuration, exit codes, supervision, and upgrades | [Running the sequencer](docs/running.md) |
| Routes, messages, and status codes | [API contract](docs/api.md), rendered as [`openapi.yaml`](openapi.yaml) |
| How the scheduler orders inputs | [Scheduler semantics](docs/protocol/scheduler-semantics.md) |
| What an application must implement | [Application contract](docs/protocol/application-contract.md) |
| Outages, and the rebuild after lost state | [Batch landing and outages](docs/recovery/README.md) and [cockroach recovery](docs/recovery/cockroach.md) |
| Actors, assumptions, and residual risks | [Threat model](docs/threat-model/README.md) |
| Independent checking of the sequencer's state | [Watchdog](docs/watchdog/README.md) |

## License

Apache-2.0. See [`LICENSE`](LICENSE). Authors in [`AUTHORS`](AUTHORS).
