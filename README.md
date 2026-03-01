# Sequencer Prototype

Prototype sequencer, currently backed by a dummy wallet app (`Transfer`, `Deposit`, `Withdrawal`).

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
- **Ordering** is deterministic and persisted. Replay/catch-up reads `sequenced_l2_txs` (joined with `user_ops` / `direct_inputs`).
- **Frame fee** is fixed per frame (`frames.fee`):
  - users sign `max_fee`
  - inclusion validates `max_fee >= current_frame_fee`
  - execution charges `current_frame_fee` (not signed max)
  - next frame fee is sampled from `recommended_fees` when rotating to a new frame

## Quick Start

From repo root:

```bash
cargo check
cargo test
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

Run server with defaults:

```bash
SEQ_HTTP_ADDR=127.0.0.1:3000 \
SEQ_DB_PATH=sequencer.db \
cargo run -p sequencer
```

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

POST notes:

- `signature` must be 65 bytes.
- `sender` is optional; if provided, it must match recovered signer.
- `message.data` is SSZ-encoded method payload bytes.
- payload size is bounded at ingress; oversized requests are rejected before they enter hot path.

### `GET /ws/subscribe?from_offset=<u64>`

WebSocket stream of sequenced L2 transactions from persisted order.

Notes:

- `from_offset` is optional (defaults to `0`).
- messages are JSON text frames.
- binary fields are hex-encoded (`0x`-prefixed).

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

## Configuration

Main environment variables:

- `SEQ_HTTP_ADDR`
- `SEQ_DB_PATH`
- `SEQ_QUEUE_CAP`
- `SEQ_QUEUE_TIMEOUT_MS`
- `SEQ_MAX_USER_OPS_PER_CHUNK` (`SEQ_MAX_BATCH` is legacy alias)
- `SEQ_SAFE_DIRECT_BUFFER_CAPACITY`
- `SEQ_MAX_BATCH_OPEN_MS`
- `SEQ_MAX_BATCH_USER_OP_BYTES`
- `SEQ_INCLUSION_LANE_IDLE_POLL_INTERVAL_MS`
- `SEQ_INCLUSION_LANE_TICK_INTERVAL_MS` (legacy alias)
- `SEQ_COMMIT_LANE_TICK_INTERVAL_MS` (legacy alias)
- `SEQ_BROADCASTER_IDLE_POLL_INTERVAL_MS`
- `SEQ_BROADCASTER_PAGE_SIZE`
- `SEQ_BROADCASTER_SUBSCRIBER_BUFFER_CAPACITY`
- `SEQ_MAX_BODY_BYTES`
- `SEQ_SQLITE_SYNCHRONOUS`
- `SEQ_DOMAIN_NAME`
- `SEQ_DOMAIN_VERSION`
- `SEQ_DOMAIN_CHAIN_ID`
- `SEQ_DOMAIN_VERIFYING_CONTRACT`

## Storage Model (high level)

- `batches`: batch metadata
- `frames`: frame boundaries within each batch
- `frames.fee`: committed fee for each frame
- `user_ops`: included user operations
- `direct_inputs`: direct-input payload stream
- `sequenced_l2_txs`: append-only ordered replay rows (`UserOp` xor `DirectInput`)
- `recommended_fees`: singleton mutable recommendation for next frame fee

No SQL views are required in the current prototype schema.

## Project Layout

- `sequencer/src/main.rs`: bootstrap, env config, HTTP server + lane lifecycle
- `sequencer/src/api/`: HTTP API and error mapping
- `sequencer/src/inclusion_lane/`: hot-path inclusion loop, chunk/frame/batch rotation, catch-up
- `sequencer/src/l2_tx_broadcaster.rs`: centralized ordered-L2Tx poller + subscriber fanout
- `sequencer/src/storage/`: schema, migrations, SQLite persistence and replay reads
- `app-core/src/application/`: app execution/validation interfaces + wallet prototype
- `app-core/src/user_op.rs`: signed user-op and EIP-712 payload model
- `app-core/src/l2_tx.rs`: replay/fanout transaction domain types
- `canonical-app/src/main.rs`: placeholder canonical runtime entrypoint

## Prototype Limits

- Wallet state is in-memory (not persisted).
- Direct-input ingestion from chain is not implemented yet (currently append via storage APIs).
- Schema/migrations are still in prototype mode and may change.

## License

Apache-2.0. See `LICENSE`.

Authors are listed in `AUTHORS`.
