# AGENTS.md

This file tells AI coding agents how to work effectively in this repository.

## Mission

Build and evolve a **sequencer prototype** for a future DeFi stack.

Current scope is intentionally small: a **dummy wallet app** that supports:
- `Transfer`
- `Withdrawal`

Primary objective in this phase: make sequencer behavior, safety checks, and persistence reliable before adding "real world" execution logic.

## Project Snapshot

- Language: Rust (`edition = 2024`)
- API: Axum
- Queueing: Tokio MPSC
- Commit path: single blocking inclusion lane (hot path)
- Storage: SQLite (`rusqlite`, WAL mode)
- Signing: EIP-712 (`alloy`)
- Method payload encoding: SSZ

## Glossary

- `chunk`: small bounded list of user ops processed/executed and persisted together to amortize SQLite cost and keep low-latency ack behavior.
- `frame`: canonical ordering boundary that commits a `safe_block` plus a list of user ops; canonical execution drains all direct inputs safe at that block before executing the frame’s user ops.
- `batch`: list of frames that will be posted on-chain as one unit.
- `inclusion lane`: the hot-path single-lane loop that dequeues user ops, executes app logic, persists ordering, and rotates frame/batch boundaries.

## Architecture Map

- `sequencer/src/main.rs`: thin binary entrypoint.
- `sequencer/src/lib.rs`: public sequencer API (`run`, `RunConfig`).
- `sequencer/src/config.rs`: runtime input parsing and EIP-712 domain construction.
- `sequencer/src/runtime.rs`: bootstrap and runtime wiring.
- `sequencer/src/api/mod.rs`: `POST /tx` and `GET /ws/subscribe` endpoints (tx ingress + replay feed).
- `sequencer/src/api/error.rs`: API error model + HTTP mapping.
- `sequencer/src/inclusion_lane/mod.rs`: inclusion-lane exports and public surface.
- `sequencer/src/inclusion_lane/lane.rs`: batched execution/commit loop (single lane).
- `sequencer/src/inclusion_lane/types.rs`: inclusion-lane queue item and pipeline error types.
- `sequencer/src/inclusion_lane/error.rs`: inclusion-lane runtime and catch-up error types.
- `sequencer/src/input_reader/`: safe-input ingestion from InputBox into SQLite.
- `sequencer/src/l2_tx_feed/mod.rs`: DB-backed ordered-L2Tx feed used by WS subscriptions.
- `sequencer/src/storage/mod.rs`: DB open, migrations, frame persistence, and direct-input broker APIs.
- `sequencer/src/storage/migrations/`: DB schema/bootstrapping (`0001`).
- `sequencer-core/src/`: shared domain types/interfaces (`Application`, `SignedUserOp`, `SequencedL2Tx`, broadcast message model).
- `examples/app-core/src/application/mod.rs`: wallet prototype implementing `Application`.
- `tests/benchmarks/src/`: benchmark harnesses and self-contained benchmark runtime.

## Domain Truths (Important)

- This is a **sequencer prototype**, not a full DeFi stack yet.
- API validates signature and enqueues signed `UserOp`; method decoding happens during application execution.
- Deposits are direct-input-only (L1 -> L2) and must not be represented as user ops.
- Rejections (`InvalidNonce`, fee cap too low, insufficient gas balance) produce no state mutation and are not persisted.
- Included txs are persisted as frame/batch data in `batches`, `frames`, `user_ops`, `direct_inputs`, and `sequenced_l2_txs`.
- Frame fee is persisted in `frames.fee` and is fixed for the lifetime of that frame.
- The next frame fee is sampled from `batch_policy_derived.recommended_fee` when rotating to a new frame (defaults follow `batch_policy` bootstrap rows; tune `gas_price` / `alpha` via SQLite if needed).
- `/ws/subscribe` currently has internal guardrails: subscriber cap `64`, catch-up cap `50000`.
- When that catch-up window is exceeded, `/ws/subscribe` upgrades and then closes with websocket close code `1008` (`POLICY`) and reason `catch-up window exceeded`.
- Wallet state (balances/nonces) is in-memory right now (not persisted).
- EIP-712 domain name/version are fixed in code; chain ID and verifying contract come from `SEQ_CHAIN_ID` and `SEQ_APP_ADDRESS` (validated against the RPC chain id at startup).

## Hot-Path Invariants

- API ack is tied to chunk durability, not frame/batch closure.
- Chunk commit and ack remain low-latency; frame closure is orthogonal and can happen less frequently.
- API overload for `POST /tx` is currently defined by inclusion-lane queue admission: if `try_send` hits a full queue, the handler returns `429 OVERLOADED` with message `queue full`.
- Frame closure happens when direct inputs are drained, and also whenever batch closure happens.
- Batch closure is controlled by batch policy (size and/or deadline).
- Preserve single-lane deterministic ordering; do not introduce extra concurrency in hot-path ordering logic without explicit approval.

## Storage Invariants

- Storage model is append-oriented; avoid mutable status flags for open/closed entities.
- Open batch/frame are derived by “latest row” convention.
- A frame’s leading direct-input prefix is derivable from `sequenced_l2_txs` plus `frames.safe_block`.
- `direct_inputs` contains only L1 app direct input **bodies**. InputBox payload first byte: **0x00** = direct input (tag stripped, body stored and executed), **0x01** = batch submission (for scheduler, not stored), **others** = discarded (invalid/garbage). The input reader only accepts 0x00-tagged payloads and stores `payload[1..]`.
- Safe cursor/head values should be derived from persisted facts when possible, not duplicated as mutable fields.
- Replay/catch-up must use persisted ordering plus persisted frame fee (`frames.fee`) to mirror inclusion semantics.
- Included user-op identity is constrained by `UNIQUE(sender, nonce)`.

## Type Boundaries

- `SignedUserOp`: ingress/API signature domain.
- `ValidUserOp`: app execution domain after validation boundary.
- `SequencedL2Tx`: ordered replay/fanout domain (`UserOp | DirectInput`).
- Keep private DB-only helper/intermediary types private to storage modules; prefer shared domain types at module boundaries.

## Agent Priorities

When making changes, optimize for:
1. Deterministic sequencing semantics.
2. Safety and correctness of transaction validation/execution.
3. Clear, testable boundaries between API, application logic, and storage.
4. Backward-compatible, explicit error handling.
5. Minimal, focused diffs.

## Fast Start Commands

Run from repo root:

```bash
cargo check
cargo test
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

Run server:

```bash
SEQ_ETH_RPC_URL=http://127.0.0.1:8545 \
SEQ_CHAIN_ID=31337 \
SEQ_APP_ADDRESS=0x1111111111111111111111111111111111111111 \
SEQ_BATCH_SUBMITTER_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
cargo run -p sequencer
```

Optional env vars:
- `SEQ_HTTP_ADDR`
- `SEQ_DATA_DIR` (default `sequencer-data`; DB file `sequencer.db` inside it)
- `SEQ_LONG_BLOCK_RANGE_ERROR_CODES`
- `SEQ_BATCH_SUBMITTER_PRIVATE_KEY_FILE` (alternative to `SEQ_BATCH_SUBMITTER_PRIVATE_KEY`)
- `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`, `SEQ_BATCH_SUBMITTER_CONFIRMATION_DEPTH`

Required env vars:
- `SEQ_ETH_RPC_URL`
- `SEQ_CHAIN_ID`
- `SEQ_APP_ADDRESS`
- `SEQ_BATCH_SUBMITTER_PRIVATE_KEY` or `SEQ_BATCH_SUBMITTER_PRIVATE_KEY_FILE`

## Always / Ask First / Never

### Always

- Keep behavior explicit for transaction inclusion vs rejection.
- Preserve API error shape and status code mapping unless intentionally changing API contract.
- Add or update tests when logic changes.
- Run at least `cargo check` before finishing.

### Ask First

- Changing tx wire format (`UserOp`, SSZ payload layout, EIP-712 domain fields).
- Changing DB schema or migration strategy.
- Altering rejection semantics (what consumes nonce/gas vs what is rejected).
- Introducing concurrency changes to commit ordering guarantees.
- Changing chunk/frame/batch closure or ack semantics.

### Never

- Silently weaken signature validation.
- Merge behavioral changes with unrelated refactors in one patch.
- Rely on implicit defaults for consensus-relevant values.
- Remove guardrails around queue backpressure or inclusion-lane error reporting.

## Coding Conventions for This Repo

- Prefer small, composable functions at module boundaries (`api` -> `application` -> `storage`).
- Keep application validation/execution deterministic for a given input/state.
- Surface user-facing errors via `ApiError`; keep internal failures descriptive but safe.
- Avoid introducing heavy dependencies without strong reason.

## Testing Guidance

Focus tests on:
- signature + sender validation edge cases
- nonce progression rules
- fee/rejection behavior
- included vs rejected commit behavior
- storage batch atomicity and uniqueness constraints

If adding integration tests, prefer black-box tests around `POST /tx` and commit outcomes.

Some `sequencer` tests use Anvil and are opt-in locally:

```bash
RUN_ANVIL_TESTS=1 cargo test -p sequencer --lib
```

## Definition of Done for Agent Changes

Before finishing, ensure:
1. Code compiles (`cargo check`).
2. Changed behavior is covered by tests (or explain why tests are pending).
3. Formatting/lints are clean (or list any unresolved warnings explicitly).
4. PR summary includes:
   - what changed
   - why it changed
   - risk/compatibility notes

## Near-Term Roadmap Hints

Expected future evolution areas:
- stronger typing around tx metadata
- persistence for app state or deterministic replay
- explicit L1 block progression input

## Migration Policy

- Current prototype stage: it is acceptable to rewrite baseline migrations for clarity.
- Once environments are shared/deployed: switch to append-only forward migrations.
- Keep schema bootstrap (initial open rows/invariants) explicit and deterministic.
