# Sequencer Prototype

Prototype sequencer, currently backed by a dummy wallet app (`Transfer`, `Deposit`, `Withdrawal`).

Current focus is reliability of sequencing, persistence, and replay semantics.

## Status

- Language: Rust (edition 2024)
- API: Axum (`POST /tx`)
- Hot path: single blocking inclusion lane
- Storage: SQLite (`rusqlite`, WAL)
- Signing: EIP-712 (`alloy`)
- Payload encoding: SSZ

## Core Design

- **User ops** arrive through the API, are validated, executed, and persisted by the inclusion lane.
- **Direct inputs** are stored in SQLite (`direct_inputs`) and drained by the inclusion lane into frame boundaries (`frame_drains`).
- **Ordering** is deterministic and persisted. Replay/catch-up reads `ordered_sequenced_l2_txs`.
- **Batch fee** is fixed per batch (`batches.fee`):
  - users sign `max_fee`
  - inclusion validates `max_fee >= current_batch_fee`
  - execution charges `current_batch_fee` (not signed max)
  - next batch fee is sampled from `recommended_fees` when rotating to a new batch

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

Notes:

- `signature` must be 65 bytes.
- `sender` is optional; if provided, it must match recovered signer.
- `message.data` is SSZ-encoded method payload bytes.
- payload size is bounded at ingress; oversized requests are rejected before they enter hot path.

Success response:

```json
{
  "ok": true,
  "tx_hash": "0x...",
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
- `SEQ_MAX_BODY_BYTES`
- `SEQ_SQLITE_SYNCHRONOUS`
- `SEQ_DOMAIN_NAME`
- `SEQ_DOMAIN_VERSION`
- `SEQ_DOMAIN_CHAIN_ID`
- `SEQ_DOMAIN_VERIFYING_CONTRACT`

## Storage Model (high level)

- `batches`: batch metadata + committed batch fee
- `frames`: frame boundaries within each batch
- `user_ops`: included user operations
- `direct_inputs`: direct-input payload stream
- `frame_drains`: per-frame `drain_n`
- `recommended_fees`: singleton mutable recommendation for next batch fee

Views:

- `ordered_sequenced_l2_txs`: canonical ordered replay stream (`UserOp | DirectInput`)
- `frame_drain_ranges`, `batch_user_op_counts`, `frame_user_op_counts`

## Project Layout

- `sequencer/src/main.rs`: bootstrap, env config, HTTP server + lane lifecycle
- `sequencer/src/api/`: HTTP API and error mapping
- `sequencer/src/inclusion_lane/`: hot-path inclusion loop, chunk/frame/batch rotation, catch-up
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
