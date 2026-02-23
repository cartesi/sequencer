# AGENTS.md

This file tells AI coding agents how to work effectively in this repository.

## Mission

Build and evolve a **sequencer prototype** for a future DeFi stack.

Current scope is intentionally small: a **dummy wallet app** that supports:
- `Transfer`
- `Deposit`
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
- `frame`: canonical ordering boundary that contains user ops plus a `drain_n` decision for direct-input execution.
- `batch`: list of frames that will be posted on-chain as one unit.
- `inclusion lane`: the hot-path single-lane loop that dequeues user ops, executes app logic, persists ordering, and rotates frame/batch boundaries.

## Architecture Map

- `sequencer/src/main.rs`: process bootstrap, env config, queue wiring, HTTP server.
- `sequencer/src/api/mod.rs`: `POST /tx` endpoint, JSON decode, signature recovery, enqueue + wait for commit result.
- `sequencer/src/api/error.rs`: API error model + HTTP mapping.
- `sequencer/src/inclusion_lane/mod.rs`: inclusion-lane exports and public surface.
- `sequencer/src/inclusion_lane/lane.rs`: batched execution/commit loop (single lane).
- `sequencer/src/inclusion_lane/types.rs`: inclusion-lane queue item and pipeline error types.
- `sequencer/src/inclusion_lane/error.rs`: inclusion-lane runtime and catch-up error types.
- `sequencer/src/storage/mod.rs`: DB open, migrations, frame persistence, and direct-input broker APIs.
- `sequencer/src/storage/migrations/`: DB schema/bootstrapping (`0001`) and views (`0002`).
- `app-core/src/application/mod.rs`: app execution interface (`Application`) and wallet prototype.
- `app-core/src/user_op.rs`: signed user-op domain types and EIP-712 payload.
- `app-core/src/l2_tx.rs`: sequenced L2 transaction types used for replay/broadcast boundaries.
- `canonical-app/src/main.rs`: placeholder canonical scheduler binary entrypoint.

## Domain Truths (Important)

- This is a **sequencer prototype**, not a full DeFi stack yet.
- API validates signature and enqueues signed `UserOp`; method decoding happens during application execution.
- Rejections (`InvalidNonce`, fee cap too low, insufficient gas balance) produce no state mutation and are not persisted.
- Included txs are persisted as frame/batch data in `batches`, `frames`, `user_ops`, `direct_inputs`, and `frame_drains`.
- Batch fee is persisted in `batches.fee` and is fixed for the lifetime of that batch.
- The next batch fee is sampled from `recommended_fees` when rotating to a new batch (default bootstrap value is `1`).
- Wallet state (balances/nonces) is in-memory right now (not persisted).

## Hot-Path Invariants

- API ack is tied to chunk durability, not frame/batch closure.
- Chunk commit and ack remain low-latency; frame closure is orthogonal and can happen less frequently.
- Frame closure happens when direct inputs are drained, and also whenever batch closure happens.
- Batch closure is controlled by batch policy (size and/or deadline).
- Preserve single-lane deterministic ordering; do not introduce extra concurrency in hot-path ordering logic without explicit approval.

## Storage Invariants

- Storage model is append-oriented; avoid mutable status flags for open/closed entities.
- Open batch/frame are derived by “latest row” convention.
- `drain_n` is represented by `frame_drains` rows and is derivable from stored data.
- Safe cursor/head values should be derived from persisted facts when possible, not duplicated as mutable fields.
- Replay/catch-up must use persisted ordering plus persisted batch fee (`batches.fee`) to mirror inclusion semantics.

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

Run server (defaults shown):

```bash
SEQ_HTTP_ADDR=127.0.0.1:3000 \
SEQ_DB_PATH=sequencer.db \
cargo run -p sequencer
```

Key env vars:
- `SEQ_HTTP_ADDR`
- `SEQ_DB_PATH`
- `SEQ_QUEUE_CAP`
- `SEQ_QUEUE_TIMEOUT_MS`
- `SEQ_MAX_USER_OPS_PER_CHUNK` (preferred)
- `SEQ_MAX_BATCH` (legacy alias)
- `SEQ_SAFE_DIRECT_BUFFER_CAPACITY`
- `SEQ_MAX_BATCH_OPEN_MS`
- `SEQ_MAX_BATCH_USER_OP_BYTES`
- `SEQ_INCLUSION_LANE_IDLE_POLL_INTERVAL_MS` (preferred)
- `SEQ_INCLUSION_LANE_TICK_INTERVAL_MS` (legacy alias)
- `SEQ_COMMIT_LANE_TICK_INTERVAL_MS` (legacy alias)
- `SEQ_MAX_BODY_BYTES`
- `SEQ_SQLITE_SYNCHRONOUS`
- `SEQ_DOMAIN_NAME`
- `SEQ_DOMAIN_VERSION`
- `SEQ_DOMAIN_CHAIN_ID`
- `SEQ_DOMAIN_VERIFYING_CONTRACT`

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
