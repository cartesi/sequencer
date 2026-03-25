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
- **Direct inputs** are stored in SQLite (`safe_inputs`) and sequenced in append-only replay order (`sequenced_l2_txs`).
- **Deposits** are direct-input-only (L1 -> L2) and are not accepted as user ops.
- **Ordering** is deterministic and persisted. Replay/catch-up reads `sequenced_l2_txs` joined with `user_ops` and `safe_inputs`.
- **Frame fee** is fixed per frame (`frames.fee`):
  - users sign `max_fee`
  - inclusion validates `max_fee >= current_frame_fee`
  - execution charges `current_frame_fee`
  - when opening a new frame or batch, the sequencer samples **`recommended_fee`** from the `batch_policy_derived` SQLite view (derived from `gas_price`, amortization `alpha`, and on-chain DA constants in `batch_policy`)
- **Batch closure by size** uses **`batch_size_target`** from the same view (stored on `WriteHead` as `max_batch_user_op_bytes`). The inclusion lane compares it to a **worst-case estimate** of in-batch user-op bytes (`batch_user_op_count × (per-op metadata cap + max method payload)`), not the exact SSZ-encoded batch size. A **time-based** max open duration also closes batches.

## Quick Start

From repo root:

```bash
cargo check
cargo test
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

Run the server (example uses Anvil account #0 as batch submitter; use your own key in production):

```bash
SEQ_ETH_RPC_URL=http://127.0.0.1:8545 \
SEQ_CHAIN_ID=31337 \
SEQ_APP_ADDRESS=0x1111111111111111111111111111111111111111 \
SEQ_BATCH_SUBMITTER_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
cargo run -p sequencer
```

At startup the process checks that the RPC `eth_chainId` matches `SEQ_CHAIN_ID`.

Optional runtime inputs:

- `SEQ_HTTP_ADDR` defaults to `127.0.0.1:3000`
- `SEQ_DATA_DIR` defaults to `sequencer-data` (SQLite file is `sequencer.db` inside that directory; the directory is created if missing)
- `SEQ_LONG_BLOCK_RANGE_ERROR_CODES` defaults to `-32005,-32600,-32602,-32616`
- `SEQ_BATCH_SUBMITTER_PRIVATE_KEY_FILE` instead of `SEQ_BATCH_SUBMITTER_PRIVATE_KEY` (first line of the file is the key)
- `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`, `SEQ_BATCH_SUBMITTER_CONFIRMATION_DEPTH`

Required runtime inputs:

- `SEQ_ETH_RPC_URL`
- `SEQ_CHAIN_ID`
- `SEQ_APP_ADDRESS`
- `SEQ_BATCH_SUBMITTER_PRIVATE_KEY` or `SEQ_BATCH_SUBMITTER_PRIVATE_KEY_FILE`

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
- `tests/benchmarks/`: benchmark harnesses and benchmark spec

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
