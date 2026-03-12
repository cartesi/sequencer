# Sequencer Prototype

Prototype sequencer, currently backed by a dummy wallet app (`Transfer`, `Withdrawal`).

Current focus is reliability of sequencing, persistence, and replay semantics.

## Status

- Language: Rust (edition 2024)
- API: Axum (`POST /tx`, `GET /ws/subscribe`)
- Hot path: single blocking inclusion lane
- Storage: SQLite (`rusqlite`, WAL)
- Signing: EIP-712 (`alloy`)
- Payload encoding: SSZ

## Core Design

- **User ops** arrive through the API, are validated, executed, and persisted by the inclusion lane.
- **Direct inputs** are stored in SQLite (`direct_inputs`) and sequenced in append-only replay order (`sequenced_l2_txs`).
- **Deposits** are direct-input-only (L1 -> L2) and are not accepted as user ops.
- **Ordering** is deterministic and persisted. Replay/catch-up reads `sequenced_l2_txs` joined with `user_ops` and `direct_inputs`.
- **Frame fee** is fixed per frame (`frames.fee`):
  - users sign `max_fee`
  - inclusion validates `max_fee >= current_frame_fee`
  - execution charges `current_frame_fee`
  - the next frame fee is sampled from `recommended_fees` when rotating to a new frame

## Quick Start

From repo root:

```bash
cargo check
cargo test
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

Run the server with a local deployment domain:

```bash
SEQ_ETH_RPC_URL=http://127.0.0.1:8545 \
SEQ_DOMAIN_CHAIN_ID=31337 \
SEQ_DOMAIN_VERIFYING_CONTRACT=0x1111111111111111111111111111111111111111 \
cargo run -p sequencer
```

Optional runtime inputs:

- `SEQ_HTTP_ADDR` defaults to `127.0.0.1:3000`
- `SEQ_DB_PATH` defaults to `sequencer.db`
- `SEQ_LONG_BLOCK_RANGE_ERROR_CODES` defaults to `-32005,-32600,-32602,-32616`

Required runtime inputs:

- `SEQ_ETH_RPC_URL`
- `SEQ_DOMAIN_CHAIN_ID`
- `SEQ_DOMAIN_VERIFYING_CONTRACT`

Fixed protocol identity:

- domain name: `CartesiAppSequencer`
- domain version: `1`

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

## Storage Model

- `batches`: batch metadata
- `frames`: frame boundaries within each batch
- `frames.fee`: committed fee for each frame
- `user_ops`: included user operations
- `direct_inputs`: direct-input payload stream
- `sequenced_l2_txs`: append-only ordered replay rows (`UserOp` xor `DirectInput`)
- `recommended_fees`: singleton mutable recommendation for the next frame fee

## Project Layout

- `sequencer/src/main.rs`: thin binary entrypoint
- `sequencer/src/lib.rs`: public crate surface
- `sequencer/src/config.rs`: runtime input parsing and EIP-712 domain construction
- `sequencer/src/runtime.rs`: sequencer bootstrap and component wiring
- `sequencer/src/api/`: HTTP API and error mapping
- `sequencer/src/inclusion_lane/`: hot-path inclusion loop, chunk/frame/batch rotation, catch-up
- `sequencer/src/input_reader/`: safe-input ingestion from InputBox into SQLite
- `sequencer/src/l2_tx_feed/`: DB-backed ordered-L2Tx feed for WS subscriptions
- `sequencer/src/storage/`: schema, migrations, SQLite persistence, and replay reads
- `sequencer-core/src/`: shared domain types and interfaces (`Application`, `SignedUserOp`, `SequencedL2Tx`, feed message types)
- `examples/app-core/src/`: wallet prototype implementing `Application`
- `benchmarks/`: benchmark harnesses and benchmark spec

## Prototype Limits

- Wallet state is in-memory and not persisted.
- Schema and migrations are still in prototype mode and may change.

## Local Test Prerequisites

- Some `sequencer` tests spin up `Anvil`; install Foundry locally if you want the full test suite:
- Self-contained benchmarks also spawn `Anvil` from a preloaded rollups state dump.

```bash
foundryup
```

- Prepare local benchmark + guest build dependencies:

```bash
just setup
```

- Enable the Anvil-backed reader tests explicitly:

```bash
RUN_ANVIL_TESTS=1 cargo test -p sequencer --lib
```

## License

Apache-2.0. See `LICENSE`.

Authors are listed in `AUTHORS`.
