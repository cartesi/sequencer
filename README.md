# Sequencer

A sequencer for Cartesi app-specific rollups. Provides low-latency soft confirmations for user operations, posts them to L1 in batches, and maintains a deterministic replay feed that matches the application's final execution order.

## What It Does

Rollup applications need fast transaction confirmations. Waiting for L1 finality on every user action (minutes) makes interactive applications impractical. The sequencer bridges this gap: it accepts signed user operations, immediately confirms them (soft confirmation), and asynchronously posts batches to L1. The application sees these batches posted on chain.

The core guarantee: **the off-chain sequencer and the rollup scheduling routine produce identical execution order.** Users get instant feedback while the system converges to L1 truth.

## Two Chains Synchronizing

The sequencer maintains an optimistic chain of batches — a tree structure that normally degenerates into a list. Each batch contains frames, and each frame contains user operations plus a `safe_block` reference. The `safe_block` is the synchronization primitive: it tells the rollup scheduling routine "drain all direct inputs (deposits) up to this L1 block, then execute these user ops." Both sides follow this rule, producing identical state.

```
Sequencer (off-chain)              Scheduler (on-chain)
  frame: safe_block=100               drain directs up to block 100
         user_ops=[A, B, C]           execute A, B, C
  frame: safe_block=105               drain directs up to block 105
         user_ops=[D]                  execute D
```

When things go well, the sequencer's chain and the scheduler's view converge. When they don't (batches arrive stale on L1), the sequencer detects the divergence and recovers.

## Trust Model

The sequencer is a **centralized, single-writer** system. It cannot steal funds or forge invalid state — the rollup validates everything independently, and the proof system later enforces it. But the sequencer can:

- **Censor**: refuse to include a user's operations.
- **Go offline**: stop providing soft confirmations.
- **Diverge**: if batches fail to land on L1 in time, soft confirmations that were issued become invalid.

**Direct inputs** (L1 → L2 messages, used for deposits) bypass the sequencer entirely. They are posted directly to L1 and are **uncensorable** by the sequencer — the scheduling routine drains them at every `safe_block` boundary. A censoring sequencer can delay when a direct input is executed (up to `MAX_WAIT_BLOCKS`), but cannot prevent it.

The third case is handled by the recovery subsystem. Batches that are too old when they reach L1 (`inclusion_block - safe_block >= MAX_WAIT_BLOCKS`) are skipped by the scheduler. This "staleness" poisons the nonce counter: all subsequent batches become unreachable regardless of their individual freshness. The sequencer detects this via a danger-zone threshold, preemptively goes offline, flushes the L1 mempool, and cascade-invalidates the doomed chain. See [`docs/recovery/`](docs/recovery/) for the full design, TLA+ formal verification, and design history.

## Failure Modes

The sequencer is designed to handle:

- **L1 provider outages**: workers retry with exponential backoff. The inclusion lane and API continue operating locally. A wall-clock fallback detects if the outage pushes batches into the danger zone.
- **Process crashes**: recovery runs at startup. All recovery state is derived from SQLite (atomic transactions) and L1 safe state. No external coordination needed.
- **Extended downtime**: on restart, the sequencer syncs to the current L1 safe head, flushes if needed, and recovers.

The sequencer trusts its own code is bug free. Recovery means recovery from liveness failures, which can legitimately happen even in the absence of bugs (i.e. infrastructure outages, network failures, gateway failure).

## Interfaces

### User Operations

Users submit signed operations via `POST /tx` (JSON-RPC). Operations are signed with EIP-712 (using the app's chain ID and address). The sequencer validates the signature, executes the operation against the current app state, and returns a soft confirmation.

### Sequenced Transaction Feed

Subscribers connect via `GET /ws/subscribe?from_offset=<u64>` (WebSocket). The feed delivers all sequenced transactions (user ops + direct inputs) in deterministic order, matching the on-chain execution order. This is the primary interface for downstream consumers (frontends, indexers). This route is not optimized for direct user connection. Instead, we designed this endpoint for few indexers, with these indexers serving users directly.

### Batch Submission

The batch submitter posts closed batches to L1's InputBox contract. Each batch carries a sequential nonce for deduplication. L1 wallet nonces in turn guarantee ordering. The submitter is stateless — it derives pending work from SQLite and L1 state each tick.

## Running

```bash
SEQ_ETH_RPC_URL=http://127.0.0.1:8545 \
SEQ_CHAIN_ID=31337 \
SEQ_APP_ADDRESS=0x1111111111111111111111111111111111111111 \
SEQ_BATCH_SUBMITTER_PRIVATE_KEY=0xac09...f2ff80 \
cargo run -p sequencer
```

Required: `SEQ_ETH_RPC_URL`, `SEQ_CHAIN_ID`, `SEQ_APP_ADDRESS`, `SEQ_BATCH_SUBMITTER_PRIVATE_KEY` (or `_FILE`).

Optional: `SEQ_HTTP_ADDR` (default `127.0.0.1:3000`), `SEQ_DATA_DIR` (default `sequencer-data`), `SEQ_PREEMPTIVE_MARGIN_BLOCKS` (default `75`), `SEQ_SECONDS_PER_BLOCK` (default `12`), `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`, `SEQ_BATCH_SUBMITTER_CONFIRMATION_DEPTH`.

## Development

```bash
cargo check                                          # compile
cargo test --workspace --exclude canonical-test       # test (includes Anvil-backed tests)
cargo fmt --all                                      # format
cargo clippy --all-targets --all-features -- -D warnings  # lint
```

Some tests require [Foundry](https://getfoundry.sh) (`anvil` on PATH). They run by default and fail with a clear message if unavailable. This project uses Nix + direnv for tooling — `direnv allow` provides Foundry, TLA+, and other dependencies.

## Further Reading

- [`AGENTS.md`](AGENTS.md) — developer guide: architecture, conventions, storage model, testing guidance.
- [`docs/recovery/`](docs/recovery/) — recovery design, TLA+ formal specs, design history.
- [`sequencer-core/`](sequencer-core/) — shared domain types (`Application`, `SignedUserOp`, `Batch`, `Frame`).
- [`examples/app-core/`](examples/app-core/) — example wallet app implementing the `Application` trait.

## License

Apache-2.0. See [`LICENSE`](LICENSE). Authors in [`AUTHORS`](AUTHORS).
