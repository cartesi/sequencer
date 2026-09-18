# Sequencer

A sequencer for Cartesi app-specific rollups. Provides low-latency soft confirmations for user operations, posts them to L1 in batches, and exposes its current application execution order for replica replay.

**Security-critical infrastructure.** Handle every change with the care financial systems demand.

## What It Does

Rollup applications need fast transaction confirmations. Waiting for L1 finality on every user action (minutes) makes interactive applications impractical. The sequencer bridges this gap: it accepts signed user operations, immediately confirms them (soft confirmation), and asynchronously posts batches to L1. The application sees these batches posted on chain.

The protocol objective is that, under supported honest operation, **the
off-chain sequencer predicts the same execution order the rollup's on-chain
scheduler later produces.** Soft confirmations are optimistic and may be
invalidated by the recovery cases below; L1 remains canonical truth.

## Two Chains Synchronizing

The sequencer maintains an optimistic chain of batches — a tree that normally degenerates into a list. Each batch contains frames, and each frame contains user operations plus a `safe_block` reference. The `safe_block` is the synchronization primitive: it tells the on-chain scheduler "drain all direct inputs (deposits) up to this L1 block, then execute these user ops." Both sides follow the rule, producing identical state.

```
Sequencer (off-chain)              Scheduler (on-chain)
  frame: safe_block=100               drain directs up to block 100
         user_ops=[A, B, C]           execute A, B, C
  frame: safe_block=105               drain directs up to block 105
         user_ops=[D]                 execute D
```

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

During normal operation the sequencer advances logical frame time after five
newly-safe blocks have accumulated. That clock tick drains every covered direct
before later user ops and may also create an empty-direct frame to improve the
application-visible clock. Safe-head publication is best effort: if the node
exposes a multi-block jump, the sequencer creates one frame at the observed tip
and never fabricates intermediate frames.

Soft confirmations are an **optimistic prediction**: the sequencer also
cross-checks every at/above-anchor batch its off-chain scheduler simulation
accepts on L1 against the batch it sealed locally (a content-identity check).
When a foreign or byte-different landing reaches L1 *safe* finality and the
input reader ingests it, the same transaction records canonical divergence and
freezes the accepted frontier; the runtime stops when it next observes that
fact, and every later boot refuses until an operator performs cockroach
recovery. User-op chunks committed before runtime observation may still
acknowledge and be rolled back. This check is a narrow zombie/foreign-batch
backstop, not proof that arbitrary application or scheduler divergence cannot
exist, and it does not replace the watchdog. The mechanism and its bounds are
recorded in [`docs/invariants.md`](docs/invariants.md) (I9 and I15).

## Recovery

**[Standard recovery](docs/recovery/README.md)** runs automatically at startup
using the existing database. It handles liveness failures such as outages and
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

The [threat model](docs/threat-model/README.md#self-trust) explains the boundary
between normal operation's self-trust and manual repair after a bug.

## Failure Modes

The sequencer is designed to handle:

- **L1 provider outages** — workers retry with exponential backoff. The inclusion lane and API continue operating locally. A wall-clock fallback detects when an outage pushes batches into the danger zone.
- **Undiagnosed interruptions (OOM, SIGKILL, reboot)** — restart can recover automatically: every boot derives any required recovery from SQLite and L1 safe state through startup recovery, never assuming the previous exit was clean. Terminal errors returned through a command bracket best-effort record their cause in `terminal_faults`; terminal runtime aborts leave only process diagnostics.
- **Extended downtime** — startup syncs to the current L1 safe head, flushes if needed, and recovers before admission. A terminal exit requires operator investigation; rebuilding untrustworthy state follows the cockroach recovery procedure above.
- **Adversarial L1 mempool** — block builders and private mempools are treated as adversarial. Recovery waits until every covered wallet-nonce slot is consumed at safe depth, whether the original transaction or a flush no-op wins, so delayed "zombie" submissions cannot land later.

## Interfaces

### User Operations

Users submit signed operations via `POST /tx` (JSON). Operations are signed with EIP-712 using the rollup's chain ID and app address. The sequencer validates the signature, executes the operation against the current app state, and returns a soft confirmation. `GET /fee` quotes the live frame fee, the next-frame recommendation, and a suggested `max_fee` a wallet can sign.

### Sequenced Transaction Feed

Subscribers restore an HTTP snapshot, then use one WebSocket stream to replay
application inputs and follow the optimistic tip. Recovery can replace that
history; the snapshot's era, generation, and input count bind a resume request
to the state the consumer actually holds. The endpoint serves a small number of
infrastructure subscribers, which serve users directly. See the
[bootstrap workflow](docs/protocol/application-history.md#replica-bootstrap-and-resume)
and [wire contract](#api).

### Batch Submission

The batch submitter posts closed batches to L1's InputBox contract. Each batch carries a sequential nonce for deduplication; L1 wallet nonces guarantee ordering. The submitter is stateless — it derives pending work from SQLite and L1 state each tick.

## Running

The sequencer runs in two phases. **`setup`** pins the
deployment identity (including the reviewed fee-oracle source), does the initial L1 sync, and registers the genesis
snapshot — run it once. Plain `setup` is L1-read-only: it takes the batch-submitter
*address*, never the signing key. **`run`** boots the sequencer from the
set-up DB, reading identity from it (so chain id / app address are not `run`
arguments); it holds the signing key because it submits.

For rebuilding from a trusted checkpoint, follow the
[cockroach recovery procedure](docs/recovery/cockroach.md#run-a-rebuild).
`setup --recovery` also needs the submitter key because it flushes transactions.

```bash
# Phase A — set up the data dir (run once; idempotent).
CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=http://127.0.0.1:8545 \
CARTESI_SEQUENCER_BLOCKCHAIN_ID=31337 \
CARTESI_SEQUENCER_APP_ADDRESS=0x1111111111111111111111111111111111111111 \
CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
cargo run -p wallet-sequencer -- setup

# Phase B — run the sequencer.
CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=http://127.0.0.1:8545 \
CARTESI_SEQUENCER_AUTH_PRIVATE_KEY=0xac09...f2ff80 \
cargo run -p wallet-sequencer -- run
```

A third subcommand, **`flush-mempool`**, settles the batch-submitter wallet
nonce on demand (keyed operator tool). It is flush-only: it requires a
completed setup and no canonical divergence, and it never performs
Sync/Cascade or launches runtime workers.

`setup` requires: `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`, `CARTESI_SEQUENCER_BLOCKCHAIN_ID`, `CARTESI_SEQUENCER_APP_ADDRESS`, `CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS`.
`run` requires: `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`, `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY` (or `_FILE`); it refuses to boot until `setup` has completed.

Optional: `CARTESI_SEQUENCER_HTTP_ADDR` (default `127.0.0.1:3000`, `run`), `CARTESI_SEQUENCER_DATA_DIR` (default `sequencer-data` — SQLite file is `sequencer.db` inside; created if missing), `CARTESI_SEQUENCER_PREEMPTIVE_MARGIN_BLOCKS` (default `300`), `CARTESI_SEQUENCER_SECONDS_PER_BLOCK` (default `12`), `CARTESI_SEQUENCER_L1_READ_STALE_AFTER_BLOCKS` (default `600`), `CARTESI_SEQUENCER_LONG_BLOCK_RANGE_ERROR_CODES` (default `-32005,-32012,-32600,-32602,-32616`), `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE` (alternative to `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY`; first line of the file is the key), `CARTESI_SEQUENCER_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`, `CARTESI_SEQUENCER_BATCH_SUBMITTER_CONFIRMATION_DEPTH`.

By default the blockchain endpoint must be `https://` unless its host is loopback (`localhost`, `127.0.0.0/8`, `::1`) — a guard against accidentally sending L1 traffic to a public RPC in the clear. Set `CARTESI_SEQUENCER_ALLOW_INSECURE_RPC=true` (or `--allow-insecure-rpc`) to permit plaintext `http://` to a non-loopback host on a **trusted private network** — e.g. a Docker Compose / Kubernetes service name (`http://anvil:8545`), `host.docker.internal`, or a private-VPC IP.

The flag is **per-invocation, not pinned into the DB**: set it on every subcommand that dials L1 (`setup`, `run`, and `flush-mempool`). Setting it only on `setup` and then omitting it on `run` is refused at boot with `remote RPC must use https` — that is by design (each keyed/read path re-validates the endpoint), not a bug. In a container deployment, put it in the shared environment for all sequencer commands. Example (Docker Compose):

```yaml
environment:
  CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT: "http://anvil:8545"
  CARTESI_SEQUENCER_ALLOW_INSECURE_RPC: "true"
```

Process exit codes follow the orchestrator exit-code contract: `0` clean shutdown, `10` restart (expect a recovery boot), `20` transient refusal (retry with backoff), `30` terminal command refusal (operator required — e.g. setup not complete, identity mismatch, canonical divergence, persistent storage/application invariant failure), `40` a previous instance left work past the checkpoint (wipe the data directory and run `setup --recovery`), and `1` for an unclassified operational failure. Diagnosed terminal runtime faults, including worker panics, immediately call `abort()` (SIGABRT, status 134), without worker drain or database settlement. Supervisors must treat SIGABRT as terminal class. Ordinary shutdown, expected recovery, and transient errors still drain gracefully. Startup panics caught by the command harness are projected to `30`; panics after runtime scope creation abort; `101` remains possible before the command harness starts. The constants live in `sequencer/src/commands/error.rs`; supervisor recipes are in [`docs/watchdog/operator-deployment.md`](docs/watchdog/operator-deployment.md).

Fixed protocol identity (EIP-712):

- domain name: `CartesiAppSequencer`
- domain version: `1`
- `chain_id` and `verifying_contract` come from `CARTESI_SEQUENCER_BLOCKCHAIN_ID` and `CARTESI_SEQUENCER_APP_ADDRESS`

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
- Browser wallets can call `POST /tx` and `GET /fee` from any origin with any request headers; preflight permits GET and POST and is cached for one hour. CORS is applied only to ingress. Egress routes remain operator-only and require network access controls.

Success response after inclusion:

```json
{
  "ok": true,
  "sender": "0x...",
  "nonce": 0
}
```

### `GET /fee`

Fee quote for setting signed user-op `max_fee` before `POST /tx`. All three fields are log-space exponents (base 129/128), the same encoding as `max_fee`. Inclusion rejects any op with `max_fee` below the open-frame `fee`.

```json
{ "fee": 1356, "recommended_fee": 1356, "suggested_max_fee": 1409 }
```

Notes:

- `fee` is frozen for the lifetime of the open frame (the live inclusion check).
- `recommended_fee` is what the next frame will sample at rotation (currently after five newly-safe L1 blocks, best-effort).
- `suggested_max_fee` is `max(fee, recommended_fee)` plus 1.5× log-space slack. Wallets can copy this into signed `max_fee`; the user pays the frame fee, not this cap. Clients that want their own policy can ignore it and combine the two facts themselves.
- `200` while an open frame exists (the admitted runtime always has one).
- `503` with code `UNAVAILABLE` during shutdown, or if no open frame exists.

### `GET /ws/subscribe?era_id=<uuid>&recovery_generation=<u64>&next_input=<u64>`

WebSocket stream of the current application history, replaying from the inclusive
`next_input` offset and then following the optimistic tip. Fetch and restore
`/latest_snapshot` first; its headers supply the complete subscription claim.
After each successfully applied input at offset `X`, persist the claim with
`next_input = X + 1` alongside the replica state.

- All three query fields are required. Missing or malformed fields return HTTP `400`.
- An era or generation mismatch, an unavailable prefix, or a position ahead of
  the head returns HTTP `409` before upgrade. The JSON body and `X-History-Error`
  header carry the same typed refusal: `ERA_CHANGED`, `STALE_GENERATION`,
  `HISTORY_UNAVAILABLE`, or `AHEAD_OF_HEAD`. Rebootstrap on a history mismatch.
- A claim exactly at the head waits for the next input. Replay uses bounded
  pages and queues, with no total catch-up limit. The subscriber cap is `64`.
- Messages are JSON text frames; binary fields are `0x`-prefixed hex.
  Direct-input `block_timestamp` values are Unix seconds.
- Batch envelopes are absent. Offsets count executed application inputs,
  including business failures and malformed-direct no-ops.
- Recovery stops the process and disconnects subscribers. A reconnect must
  present its saved claim; offsets alone cannot distinguish a replaced suffix.
- Shutdown or a feed read/send failure may disconnect without a WebSocket
  Close frame. Resume from the saved claim after an unexpected disconnect;
  a clean close is not required for safe replay.

Message shapes:

```json
{ "kind": "user_op", "offset": 10, "sender": "0x...", "nonce": 7, "fee": 1, "data": "0x...", "safe_block": 123, "batch_nonce": 4 }
```

```json
{ "kind": "direct_input", "offset": 11, "sender": "0x...", "block_number": 123, "block_timestamp": 1700000000, "transaction_hash": "0x...", "payload": "0x...", "input_index": 42, "batch_nonce": 4 }
```

### History metadata and historical L1 inputs (internal only)

Readers that maintain additional transfer/order history can reconstruct it from
L1 and then join the application feed. The
[projection replay contract](docs/protocol/projection-replay.md) describes
bootstrap, client checkpoints, pending directs, and terminal drain.

`GET /history` returns one coherent view of the deployment, current application
history, immutable era baseline, and latest accepted checkpoint. Optional
`era_id=<uuid>` requires the selected era; a mismatch returns `409 ERA_CHANGED`.
Example immediately after a rebuild:

```json
{
  "deployment": {
    "chain_id": 31337,
    "app_address": "0x1111111111111111111111111111111111111111",
    "input_box_address": "0x2222222222222222222222222222222222222222",
    "app_deployment_block": 1,
    "batch_submitter_address": "0x3333333333333333333333333333333333333333"
  },
  "history": {
    "version": {
      "era_id": "22222222-2222-4222-8222-222222222222",
      "recovery_generation": 0
    },
    "available_from": 7,
    "head": 7
  },
  "baseline": {
    "l1_stop_block": 1240,
    "l1_end_input_index": 8,
    "next_batch_nonce": 2
  },
  "accepted_checkpoint": null,
  "compatibility": null
}
```

- `history.available_from` is baseline application count `K`; entries `[K,head)`
  are available through WS. Counts include all executed application inputs.
- `baseline` describes the fixed L1 stopping block `C`, exclusive InputBox end
  `R`, and scheduler nonce after recovery's terminal drain. It survives generation
  changes and baseline artifact GC. It is distinct from the moving safe head.
- `accepted_checkpoint`, when available, has `inclusion_block`,
  `executed_input_count`, and `next_batch_nonce`, under `history.version`.
  Genesis supplies the zero checkpoint; a rebuilt baseline is not itself an
  accepted checkpoint. The metadata does not lease or download a native artifact
  and does not certify a client projection. A known divergence returns `503`.
- `compatibility` is `null` unless `from_generation=<u64>` is supplied together
  with `era_id`. It then contains `from_generation` and `preserved_input_count`:
  the prefix that survived every standard recovery since that generation,
  bounded by the current head. A future generation or missing era returns
  `400 BAD_REQUEST`; an era mismatch takes precedence over the generation bound.

For example, `GET /history?era_id=<uuid>&from_generation=0` can return
`"compatibility": {"from_generation": 0, "preserved_input_count": 3}`.
A saved checkpoint from that era/generation is reusable when its count `X`
satisfies `K <= X <= 3`. The boundary is inclusive: the checkpoint has executed
entries before `X`, and resumes at entry `X`. Each checkpoint must be checked
using its own saved generation. With no intervening recovery, the bound is the
current head. The [history contract](docs/protocol/application-history.md#checkpoint-compatibility-after-standard-recovery)
defines the calculation and trust boundary.

Restore an eligible checkpoint, persist the response's current history version
with it, and subscribe using that version and its actual count. A recovery
between lookup and subscription still returns `STALE_GENERATION`; repeat the
lookup using the version associated with the restored state. Compatibility does
not certify the client's application or projection implementation, and cannot
cross a cockroach recovery's new era.

`GET /historical-l1-inputs` requires `era_id` and exactly one starting selector:

- `next_input_index=<u64>`: inclusive per-application InputBox index, starting at 0.
- `after_block=<u64>`: initially seek to the first input strictly after that block;
  continue using the returned `next_input_index`.

The endpoint serves only `[0,R)` through the selected era's `C`. A response to
`next_input_index=5&limit=1` can be:

```json
{
  "era_id": "22222222-2222-4222-8222-222222222222",
  "l1_stop_block": 1240,
  "end_input_index": 8,
  "next_input_index": 6,
  "items": [{
    "input_index": 5,
    "sender": "0x3333333333333333333333333333333333333333",
    "payload": "0x00",
    "block_number": 1230,
    "block_timestamp": 1700014760,
    "transaction_hash": "0x4444444444444444444444444444444444444444444444444444444444444444"
  }]
}
```

Records preserve original inner payloads and authenticated senders, including
malformed/rejected batches; they are not complete `EvmAdvance` envelopes. Indices
are contiguous and ordered. Binary values are hex; timestamps are Unix seconds.
Clients must preserve integer precision. A page may split a block.

Optional `limit` defaults to 256 and accepts 1–256. Pages target 1 MiB of raw
payloads; a larger first input is returned alone, intact. Hex encoding increases
wire size, so this is not a hard response-size limit. Eight historical responses
can be in flight; a permit remains held through body delivery or cancellation.
SQLite read transactions end before network delivery. These limits bound memory
by the page target or largest single input, not total history length.

Only `next_input_index == end_input_index` means EOF; a short page does not.
Requesting `next_input_index=R` or `after_block=C` returns an empty completed page.
Generation changes do not invalidate historical pages; an era change does.

Malformed/unknown query fields, invalid selectors/limits, or positions above
`R`/`C` return the existing `400 BAD_REQUEST` JSON shape. An era mismatch returns
the existing `409 ERA_CHANGED` history-policy body before semantic position
checks. Capacity exhaustion returns `429 OVERLOADED`; shutdown or an operational
read failure returns `503 UNAVAILABLE`. Interrupted bodies are failed pages.
Missing durable rows or other storage invariant failures follow the process's
terminal fault policy, never a successful partial page.

The Rust SDK exposes `history(expected_era, from_generation)` and
`historical_l1_inputs(era, start, limit)` with typed metadata and era refusals.
Both use the configured request deadline, including body transfer; callers may
increase it for large historical inputs. The client owns replay, persistence,
checkpoint selection, and subscription.

### Operator snapshot endpoints (internal only)

These serve application state to the operator's watchdog and indexers.
**They are operator-internal — no auth — and must not be exposed publicly**
(gated by network controls today; bound to a separate internal port once the
api split lands).

- `GET /finalized_state/inclusion_block` — cheap JSON the watchdog polls:
  `{ "inclusion_block": <u64>, "executed_input_count": <u64> }`.
- `GET /finalized_state` — streams the accepted checkpoint's comparison file
  (`application/octet-stream`), with `X-Inclusion-Block`,
  `X-Executed-Input-Count`, and `ETag: "block-<n>"` (`If-None-Match` supports `304`).
  The watchdog compares at the end of that L1 block.
- `GET /latest_snapshot` — streams a restorable tar archive of the newest valid
  batch-close snapshot, or the era baseline. Includes immutable `info.toml`
  and the application's opaque `state` file or directory.
- `GET /finalized_snapshot` — streams the accepted snapshot as a tar archive,
  adding a coherent `checkpoint.toml` receipt with its L1 inclusion block and
  next batch nonce for trusted recovery.

Successful state/archive downloads include `X-History-Era`, `X-Recovery-Generation`,
and `X-Executed-Input-Count`, selected atomically with the artifact lease.
Streaming holds the lease until the response ends or the client disconnects.
The accepted endpoints return `404` until a comparable checkpoint exists:
genesis is comparable at block zero; a rebuilt baseline is restorable but only
a later accepted batch establishes a comparison point. Known divergence makes
all three finalized endpoints return `503 UNAVAILABLE`, including conditional
state requests. The check shares the checkpoint-selection transaction, before
any lease or archive is created. See [snapshot lifecycle](docs/snapshots/lifecycle.md).

## Storage Model

- `batches`: batch metadata
- `frames`: frame boundaries within each batch
- `frames.fee`: committed fee for each frame
- `user_ops`: included user operations
- `application_inputs`: current application sequence keyed by mandatory pre-execution offset; each row references a user op or an external direct input and its owning batch/frame
- `safe_inputs`: every raw InputBox observation, including batch envelopes
- `history_state`: immutable era baseline (application count and accounted L1 block) plus recovery generation
- `snapshots` and `dumps`: immutable batch-close/baseline artifacts and streaming leases; accepted status is derived from `safe_accepted_batches`
- `batch_policy`: singleton knobs and constants for DA-style batch sizing and fee derivation; `batch_policy_derived` exposes `recommended_fee` and `batch_size_target`. A batch closes on whichever fires first: the derived `batch_size_target` byte budget or the `max_batch_open` wall-clock deadline (an inclusion-lane setting, `CARTESI_SEQUENCER_MAX_BATCH_OPEN_SECONDS`, not a `batch_policy` column). Setup writes the first `log_gas_price` (and observation stamp) for both Fixed and Uniswap modes, failing if the initial Uniswap quote cannot be read. Fixed local pricing has no oracle worker; Uniswap starts from the persisted price and refreshes lazily via the setup-pinned WETH/fee-token TWAP source, retaining that price across transient source failures. Host tick-to-price conversion uses approximate floating-point arithmetic before checked integer gas-cost calculation and log encoding; recorded frame fees execute with deterministic integer arithmetic. `log_slack = log(10)` applies the 10× safety margin in log space. Fees are app-token smallest units — initially USDC (6 decimals) for the wallet prototype — not a protocol-level USDC invariant.

## Project Layout

- `sequencer/src/lib.rs`: public crate surface (`run`, `RunConfig`) — the sequencer is a library; app crates build the binary (see `examples/wallet-sequencer/`)
- `examples/wallet-sequencer/`: binary crate composing the sequencer library with the placeholder wallet app
- `sequencer/src/http.rs`: shared HTTP error type, JSON error shape, and `axum::serve` orchestration
- `sequencer/src/runtime/`: process lock and shutdown scope; command bootstrap and config live in `commands/`, the shared clock in `clock.rs`, and EIP-712 domain construction in `sequencer-core/`
- `sequencer/src/ingress/`: public-facing — `POST /tx` and `GET /fee` (`api.rs`) and the inclusion lane (`inclusion_lane/`: hot-path loop, chunk/frame/batch rotation, catch-up, snapshot lifecycle)
- `sequencer/src/egress/`: internal read path — WS subscribe + health probes (`api/`) and the DB-backed ordered-L2Tx feed (`l2_tx_feed/`)
- `sequencer/src/l1/`: L1 client surface — input reader, batch submitter, fee oracle, shared EIP-1559 estimation, provider, partition helper
- `sequencer/src/recovery/`: preemptive recovery startup, runtime danger detector, mempool flusher
- `sequencer/src/storage/`: schema, migrations, SQLite persistence (split per writer role), and replay reads
- `sequencer-core/src/`: shared domain types and interfaces (`Application`, `SignedUserOp`, `SequencedL2Tx`, feed message types)
- `examples/app-core/src/`: wallet prototype implementing `Application`
- [`bindings/c-app-engine/`](bindings/c-app-engine/README.md): reusable C ABI adapter and external static-archive integration guide
- `bindings/c-app-sequencer/`: optional C-engine CLI host and external-archive binary
- `examples/c-wallet-engine/`: reference wallet C ABI exports, genesis tool, and conformance tests
- `examples/c-wallet-sequencer/`: binary composing the C-engine host with the reference wallet engine
- `tests/benchmarks/`: benchmark harnesses and benchmark spec

Related docs:

- C application binding: [`docs/protocol/c-application-binding.md`](docs/protocol/c-application-binding.md)
- App snapshots (format + lifecycle): `docs/snapshots/`
- Watchdog — local dev: [`docs/watchdog/getting-started.md`](docs/watchdog/getting-started.md); Sepolia/mainnet: [`docs/watchdog/operator-deployment.md`](docs/watchdog/operator-deployment.md)

The watchdog ships as a multi-arch container image per release tag `vX`:

```bash
docker pull ghcr.io/cartesi/sequencer-watchdog:vX
# mirror: docker.io/cartesi/sequencer-watchdog:vX
```

## Prototype Limits

- The `Application` trait defines dump/load behavior ([format](docs/snapshots/format.md)). Every batch close registers a durable snapshot atomically with the seal. Restart restores the latest valid snapshot and replays application inputs from its count. Acceptance determines the recovery checkpoint and garbage-collection frontier without mutating artifact metadata. [Snapshot lifecycle](docs/snapshots/lifecycle.md) documents leases and crash ordering.
- Schema and migrations are still in prototype mode and may change.

## Local Test Prerequisites

- Some `sequencer` tests spin up `Anvil`; install Foundry locally if you want the full test suite:
- Self-contained benchmarks also spawn `Anvil` from a preloaded rollups state dump.

## Development

The shared [development commands](AGENTS.md#shell-and-commands) cover Rust
toolchain selection, Nix/direnv tooling, compilation, tests, formatting, and
linting. Read the [testing guidance](AGENTS.md#testing-guidance) before choosing
validation for a change; some tests require Anvil or libslirp.

## Further Reading

- [`AGENTS.md`](AGENTS.md) — developer guide: architecture, conventions, duality, recovery, invariants, rules.
- [`CLAUDE.md`](CLAUDE.md) — Claude entrypoint to the shared agent guide.
- [`docs/threat-model/README.md`](docs/threat-model/README.md) — trust boundaries, in-scope and out-of-scope threats.
- [`docs/recovery/README.md`](docs/recovery/README.md) — automatic recovery, TLA+ formal verification, design history.
- [`docs/recovery/cockroach.md`](docs/recovery/cockroach.md) — manual rebuild after lost state or a sequencer bug.
- [Application history and replay](docs/protocol/application-history.md) — progress, history identity, and replica bootstrap/resume.
- [Snapshots](docs/snapshots/README.md) — engine checkpoints, lifecycle, accepted comparison, and wallet encoding.
- [`docs/watchdog/getting-started.md`](docs/watchdog/getting-started.md) — step-by-step: run the watchdog with a local sequencer.
- [`docs/watchdog/operator-deployment.md`](docs/watchdog/operator-deployment.md) — watchdog on live L1 (Sepolia staging, mainnet production).
- [`docs/watchdog/README.md`](docs/watchdog/README.md) — watchdog architecture, modules, and test commands.
- [`sequencer-core/`](sequencer-core/) — shared domain types (`Application`, `SignedUserOp`, `Batch`, `Frame`).
- [`examples/app-core/`](examples/app-core/) — placeholder wallet app implementing the `Application` trait.

## License

Apache-2.0. See [`LICENSE`](LICENSE). Authors in [`AUTHORS`](AUTHORS).
