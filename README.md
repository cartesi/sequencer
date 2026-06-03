# Sequencer

A sequencer for Cartesi app-specific rollups. Provides low-latency soft confirmations for user operations, posts them to L1 in batches, and maintains a deterministic replay feed that matches the application's final execution order.

**Security-critical infrastructure.** Handle every change with the care financial systems demand.

## What It Does

Rollup applications need fast transaction confirmations. Waiting for L1 finality on every user action (minutes) makes interactive applications impractical. The sequencer bridges this gap: it accepts signed user operations, immediately confirms them (soft confirmation), and asynchronously posts batches to L1. The application sees these batches posted on chain.

The core guarantee: **the off-chain sequencer and the rollup's on-chain scheduler produce identical execution order.** Users get instant feedback while the system converges to L1 truth.

## Two Chains Synchronizing

The sequencer maintains an optimistic chain of batches — a tree that normally degenerates into a list. Each batch contains frames, and each frame contains user operations plus a `safe_block` reference. The `safe_block` is the synchronization primitive: it tells the on-chain scheduler "drain all direct inputs (deposits) up to this L1 block, then execute these user ops." Both sides follow the rule, producing identical state.

```
Sequencer (off-chain)              Scheduler (on-chain)
  frame: safe_block=100               drain directs up to block 100
         user_ops=[A, B, C]           execute A, B, C
  frame: safe_block=105               drain directs up to block 105
         user_ops=[D]                 execute D
```

When things go well, the sequencer's chain and the scheduler's view converge. When they don't — batches arrive stale on L1 — the sequencer detects the divergence and recovers.

## Trust Model

The sequencer is a **centralized, single-writer** system. It cannot steal funds or forge invalid state — the rollup validates everything independently, and the proof system later enforces it. But the sequencer can:

- **Censor** — refuse to include a user's operations.
- **Go offline** — stop providing soft confirmations.
- **Diverge** — if batches fail to land on L1 in time, soft confirmations that were issued become invalid.

**Direct inputs** (L1 → L2 messages, used for deposits) bypass the sequencer entirely. They are posted directly to L1 and are **uncensorable** by the sequencer — the scheduler drains them at every `safe_block` boundary. A censoring sequencer can delay when a direct input is executed (up to `MAX_WAIT_BLOCKS`, ~4h), but cannot prevent it.

The third case is handled by the recovery subsystem. Batches that are too old when they reach L1 (`inclusion_block − safe_block ≥ MAX_WAIT_BLOCKS`) are skipped by the scheduler. This "staleness" poisons the nonce counter: all subsequent batches become unreachable regardless of their individual freshness. The sequencer detects this via a danger-zone threshold, preemptively goes offline, flushes the L1 mempool, and cascade-invalidates the doomed chain. See [`docs/recovery/`](docs/recovery/) for the full design, TLA+ formal verification, and design history.

The sequencer trusts its own code is bug-free. Recovery means recovery from liveness failures, which can legitimately happen even in the absence of bugs (infrastructure outages, network failures, gateway failure). Code-level bugs are a separate problem handled by tests and review. See [`docs/threat-model/README.md`](docs/threat-model/README.md) for the complete threat model applied across the codebase.

## Failure Modes

The sequencer is designed to handle:

- **L1 provider outages** — workers retry with exponential backoff. The inclusion lane and API continue operating locally. A wall-clock fallback detects when an outage pushes batches into the danger zone.
- **Process crashes** — recovery runs at startup. All recovery state is derived from SQLite (atomic transactions) and L1 safe state. No external coordination needed.
- **Extended downtime** — on restart, the sequencer syncs to the current L1 safe head, flushes if needed, and recovers.
- **Adversarial L1 mempool** — block builders and private mempools are treated as adversarial. The recovery flusher consumes every pending nonce slot with a no-op so delayed "zombie" submissions cannot land later.

## Interfaces

### User Operations

Users submit signed operations via `POST /tx` (JSON). Operations are signed with EIP-712 using the rollup's chain ID and app address. The sequencer validates the signature, executes the operation against the current app state, and returns a soft confirmation.

### Sequenced Transaction Feed

Subscribers connect via `GET /ws/subscribe?from_offset=<u64>` (WebSocket). The feed delivers all sequenced transactions (user ops + direct inputs) in deterministic order, matching the on-chain execution order. This is the primary interface for downstream consumers (frontends, indexers). The endpoint is designed for a small number of indexer subscribers, which serve users directly.

### Batch Submission

The batch submitter posts closed batches to L1's InputBox contract. Each batch carries a sequential nonce for deduplication; L1 wallet nonces guarantee ordering. The submitter is stateless — it derives pending work from SQLite and L1 state each tick.

## Running

```bash
SEQ_ETH_RPC_URL=http://127.0.0.1:8545 \
SEQ_CHAIN_ID=31337 \
SEQ_APP_ADDRESS=0x1111111111111111111111111111111111111111 \
SEQ_BATCH_SUBMITTER_PRIVATE_KEY=0xac09...f2ff80 \
cargo run -p sequencer
```

Required: `SEQ_ETH_RPC_URL`, `SEQ_CHAIN_ID`, `SEQ_APP_ADDRESS`, `SEQ_BATCH_SUBMITTER_PRIVATE_KEY` (or `_FILE`).

Optional: `SEQ_HTTP_ADDR` (default `127.0.0.1:3000`), `SEQ_DATA_DIR` (default `sequencer-data` — SQLite file is `sequencer.db` inside; created if missing), `SEQ_PREEMPTIVE_MARGIN_BLOCKS` (default `300`), `SEQ_SECONDS_PER_BLOCK` (default `12`), `SEQ_LONG_BLOCK_RANGE_ERROR_CODES` (default `-32005,-32600,-32602,-32616`), `SEQ_BATCH_SUBMITTER_PRIVATE_KEY_FILE` (alternative to `SEQ_BATCH_SUBMITTER_PRIVATE_KEY`; first line of the file is the key), `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`, `SEQ_BATCH_SUBMITTER_CONFIRMATION_DEPTH`.

Fixed protocol identity (EIP-712):

- domain name: `CartesiAppSequencer`
- domain version: `1`
- `chain_id` and `verifying_contract` come from `SEQ_CHAIN_ID` and `SEQ_APP_ADDRESS`

Most queue sizes, polling intervals, and safety limits are now internal runtime constants instead of public launch-time configuration.

## API

### `POST /tx`

Request shape:

```json
{
  "message": {
    "nonce": 0,
    "max_fee": 1,
    "data": "0x..."
  },
  "signature": "0x...",
  "sender": "0x..."
}
```

Notes:

- `signature` must be 65 bytes.
- `sender` is required and must match the recovered signer.
- `message.data` is SSZ-encoded method payload bytes.
- payload size is bounded at ingress; oversized requests are rejected before entering the hot path.
- overload is enforced at queue admission: if the inclusion-lane queue is full, `POST /tx` returns HTTP `429` with code `OVERLOADED` and message `queue full`.
- queue capacity is an internal runtime constant tuned alongside inclusion-lane chunking to absorb short bursts; if this starts triggering persistently, it is a signal to revisit runtime sizing or throughput rather than add another admission layer.

### `GET /ws/subscribe?from_offset=<u64>`

WebSocket stream of sequenced L2 transactions from persisted order.

Notes:

- `from_offset` is optional and defaults to `0`.
- messages are JSON text frames.
- binary fields are hex-encoded (`0x`-prefixed).
- the current runtime enforces a subscriber cap of `64` and a catch-up cap of `50000` events.
- if the requested catch-up window exceeds that cap, the server upgrades and then immediately closes the socket with close code `1008` (`POLICY`) and reason `catch-up window exceeded`.

Message shapes:

```json
{ "kind": "user_op", "offset": 10, "sender": "0x...", "fee": 1, "data": "0x..." }
```

```json
{ "kind": "direct_input", "offset": 11, "payload": "0x..." }
```

Success response:

```json
{
  "ok": true,
  "sender": "0x...",
  "nonce": 0
}
```

### Operator snapshot endpoints (internal only)

These serve application state to the operator's watchdog and indexers.
**They are operator-internal — no auth — and must not be exposed publicly**
(gated by network controls today; bound to a separate internal port once the
api split lands).

- `GET /finalized_state/inclusion_block` — cheap JSON the watchdog polls to
  detect advance: `{ "inclusion_block": <u64>, "l2_tx_index": <u64> }`. `404`
  if no finalized snapshot exists.
- `GET /finalized_state` — streams the L1-finalized state file
  (`application/octet-stream`); headers `X-Inclusion-Block`, `X-L2-Tx-Index`,
  and `ETag: "block-<n>"` (send `If-None-Match` for a `304`).
- `GET /latest_snapshot` — streams the latest snapshot (latest pending if any,
  else finalized) for indexers that fetch state then subscribe at
  `X-L2-Tx-Index`.

Both streaming routes hold a GC lease on the dump for the response lifetime,
released even on client disconnect.

## Storage Model

- `batches`: batch metadata
- `frames`: frame boundaries within each batch
- `frames.fee`: committed fee for each frame
- `user_ops`: included user operations
- `sequenced_l2_txs`: append-only ordered replay rows (`UserOp` xor `DirectInput`); inserting into `user_ops` also appends the corresponding replay row via trigger `trg_sequence_user_op`
- `safe_inputs`: direct-input payload stream
- `batch_policy`: singleton knobs and constants for DA-style batch sizing and fee derivation; `batch_policy_derived` view exposes `recommended_fee` and `batch_size_target`

## Project Layout

- `sequencer/src/main.rs`: thin binary entrypoint
- `sequencer/src/lib.rs`: public crate surface (`run`, `RunConfig`)
- `sequencer/src/http.rs`: shared HTTP error type, JSON error shape, and `axum::serve` orchestration
- `sequencer/src/runtime/`: process bootstrap, config parsing, EIP-712 domain, shutdown signal, shared clock
- `sequencer/src/ingress/`: public write path — `POST /tx` (`api.rs`) and the inclusion lane (`inclusion_lane/`: hot-path loop, chunk/frame/batch rotation, catch-up, snapshot lifecycle)
- `sequencer/src/egress/`: internal read path — WS subscribe + health probes (`api/`) and the DB-backed ordered-L2Tx feed (`l2_tx_feed/`)
- `sequencer/src/l1/`: L1 client surface — input reader, batch submitter, provider, partition helper
- `sequencer/src/recovery/`: preemptive recovery startup, runtime danger detector, mempool flusher
- `sequencer/src/storage/`: schema, migrations, SQLite persistence (split per writer role), and replay reads
- `sequencer-core/src/`: shared domain types and interfaces (`Application`, `SignedUserOp`, `SequencedL2Tx`, feed message types)
- `examples/app-core/src/`: wallet prototype implementing `Application`
- `tests/benchmarks/`: benchmark harnesses and benchmark spec

Related docs:
- App snapshots (format + lifecycle): `docs/snapshots/`
- Watchdog — local dev: [`docs/watchdog/getting-started.md`](docs/watchdog/getting-started.md); Sepolia/mainnet: [`docs/watchdog/operator-deployment.md`](docs/watchdog/operator-deployment.md)

## Prototype Limits

- The `Application` trait exposes snapshot dump/load capability (format in `docs/snapshots/format.md`). The inclusion lane drives the snapshot lifecycle — dump at batch close, promote to finalized on L1 observation, and garbage-collect superseded dumps — and at startup rebuilds application state by loading the latest snapshot and replaying the persisted L2-tx stream from that snapshot's offset. The lifecycle and its rationale (per-range atomic promotion, GC, leasing, crash-safety) are documented in `docs/snapshots/lifecycle.md`. The snapshot is served to the operator's watchdog/indexers over internal-only HTTP routes (`/finalized_state`, `/finalized_state/inclusion_block`, `/latest_snapshot`) — no auth, gated by network-level access control until the planned per-port api split lands.
- Schema and migrations are still in prototype mode and may change.

## Local Test Prerequisites

- Some `sequencer` tests spin up `Anvil`; install Foundry locally if you want the full test suite:
- Self-contained benchmarks also spawn `Anvil` from a preloaded rollups state dump.

## Development

```bash
cargo check                                              # compile
cargo test --workspace --exclude canonical-test          # test (canonical-test needs libslirp)
cargo fmt --all                                          # format
cargo clippy --all-targets --all-features -- -D warnings # lint
```

Some tests require [Foundry](https://getfoundry.sh) (`anvil` on PATH). They run by default and fail with a clear message if unavailable. This project uses Nix + direnv for tooling — `direnv allow` provides Foundry, TLA+, and other dependencies.

## Further Reading

- [`AGENTS.md`](AGENTS.md) — developer guide: architecture, conventions, duality, recovery, invariants, rules.
- [`CLAUDE.md`](CLAUDE.md) — quick reference for shell setup and commands.
- [`docs/threat-model/README.md`](docs/threat-model/README.md) — trust boundaries, in-scope and out-of-scope threats.
- [`docs/recovery/README.md`](docs/recovery/README.md) — recovery design, TLA+ formal verification, design history.
- [`docs/watchdog/getting-started.md`](docs/watchdog/getting-started.md) — step-by-step: run the watchdog with a local sequencer.
- [`docs/watchdog/operator-deployment.md`](docs/watchdog/operator-deployment.md) — watchdog on live L1 (Sepolia staging, mainnet production).
- [`docs/watchdog/README.md`](docs/watchdog/README.md) — watchdog architecture, modules, and test commands.
- [`sequencer-core/`](sequencer-core/) — shared domain types (`Application`, `SignedUserOp`, `Batch`, `Frame`).
- [`examples/app-core/`](examples/app-core/) — placeholder wallet app implementing the `Application` trait.

## License

Apache-2.0. See [`LICENSE`](LICENSE). Authors in [`AUTHORS`](AUTHORS).
