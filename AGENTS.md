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

Top-level layout follows the system's data flow. Each module corresponds to a
writer role; see also the matching `storage/<role>.rs` for the storage half.

- `sequencer/src/main.rs`: thin binary entrypoint.
- `sequencer/src/lib.rs`: public sequencer API (`run`, `RunConfig`).
- `sequencer/src/http.rs`: shared HTTP error type, JSON `ErrorResponse` shape, `ApiConfig`, and the `axum::serve` orchestration that today merges ingress + egress routers onto one listener.
- `sequencer/src/runtime/`: process orchestration.
  - `mod.rs`: bootstrap (`run`), wiring, error type.
  - `config.rs`: CLI / env input parsing, `L1Config`, `RunConfig`, EIP-712 domain.
  - `shutdown.rs`: `ShutdownSignal` shared across components.
- `sequencer/src/ingress/`: write path from external clients.
  - `api.rs`: `POST /tx` handler, `SubmitState`, JSON-rejection mapping.
  - `inclusion_lane/`: hot-path single-lane loop (`mod.rs`), catch-up replay (`catch_up.rs`), `InclusionLaneConfig`, error/types.
- `sequencer/src/egress/`: read path to internal indexers.
  - `api/`: subscribe handler (`subscribe.rs`), `SubscribeState`, health probes (`health.rs`), router merge (`mod.rs`).
  - `l2_tx_feed/`: DB-backed ordered-L2-tx feed used by WS subscriptions.
- `sequencer/src/l1/`: L1 client surface.
  - `reader.rs`: safe-input ingestion from InputBox into SQLite.
  - `submitter/`: stateless batch submitter (`worker.rs`) + L1 poster (`poster.rs`) + config.
  - `provider.rs`: alloy provider construction.
  - `partition.rs`: long-block-range retry helper (shared by reader + submitter).
- `sequencer/src/recovery/`: preemptive recovery startup procedure.
  - `mod.rs`: `run_preemptive_recovery`, wall-clock danger estimate.
  - `flusher.rs`: mempool flusher (no-op transactions to resolve pending nonce slots).
- `sequencer/src/storage/`: SQLite-backed persistence, split by writer role.
  - `mod.rs`: shared types (`SafeInputRange`, `WriteHead`, etc.).
  - `open.rs`: `Storage` struct + open / migrations.
  - `ingress.rs`: inclusion-lane writes (batches, frames, user_ops; close/rotate).
  - `egress.rs`: WS feed / catch-up reads (paginated ordered txs).
  - `l1_inputs.rs`: input-reader writes (`safe_inputs`, `l1_safe_head`, bootstrap cache).
  - `l1_submission.rs`: batch-submitter writes (`batch_nonces`, `safe_accepted_batches`) + pending-batch reads.
  - `recovery.rs`: cascade invalidation, recovery-batch open; free fns shared with the submitter.
  - `admin.rs`: operator policy tunables (`set_alpha`, `set_log_gas_price`).
  - `internals.rs`: cross-writer helpers (i64↔u64, time, decode, write-head loaders).
  - `migrations/0001_schema.sql`: schema + `valid_*` views.
- `sequencer-core/src/`: shared domain types/interfaces (`Application`, `SignedUserOp`, `SequencedL2Tx`, broadcast message model).
- `examples/app-core/src/application/mod.rs`: wallet prototype implementing `Application`.
- `tests/benchmarks/src/`: benchmark harnesses and self-contained benchmark runtime.

## Domain Truths (Important)

- This is a **sequencer prototype**, not a full DeFi stack yet.
- API validates signature and enqueues signed `UserOp`; method decoding happens during application execution.
- Deposits are direct-input-only (L1 -> L2) and must not be represented as user ops.
- Rejections (`InvalidNonce`, fee cap too low, insufficient gas balance) produce no state mutation and are not persisted.
- Included txs are persisted as frame/batch data in `batches`, `frames`, `user_ops`, `safe_inputs`, and `sequenced_l2_txs`. Recovery metadata lives in `batch_nonces`, `safe_accepted_batches`, and `invalid_batches`.
- Frame fee is persisted in `frames.fee` and is fixed for the lifetime of that frame.
- The next frame fee is sampled from `batch_policy_derived.recommended_fee` when rotating to a new frame (defaults follow `batch_policy` bootstrap rows; tune `gas_price` / `alpha` via SQLite if needed).
- `/ws/subscribe` currently has internal guardrails: subscriber cap `64`, catch-up cap `50000`.
- When that catch-up window is exceeded, `/ws/subscribe` upgrades and then closes with websocket close code `1008` (`POLICY`) and reason `catch-up window exceeded`.
- Health endpoints (egress side): `GET /livez` (always 200 if process is alive), `GET /readyz` (200 if shutdown not requested AND inclusion lane channel still open, else 503), `GET /healthz` (JSON `{ status, inclusion_lane }` with same 200/503 mirror).
- The api today serves `/tx` (ingress) and `/ws/subscribe` + `/livez` + `/readyz` + `/healthz` (egress) on the **same listener**. The planned api split puts each side on its own port (same binary) so internal probes / subscribers can be firewalled separately from public submit traffic.
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
- `safe_inputs` contains only L1 app direct input **bodies**. InputBox payload first byte: **0x00** = direct input (tag stripped, body stored and executed), **0x01** = batch submission (for scheduler, not stored), **others** = discarded (invalid/garbage). The input reader only accepts 0x00-tagged payloads and stores `payload[1..]`.
- Safe cursor/head values should be derived from persisted facts when possible, not duplicated as mutable fields.
- Replay/catch-up must use persisted ordering plus persisted frame fee (`frames.fee`) to mirror inclusion semantics.
- Cursor pagination for ordered L2 txs uses **SQLite rowid** (`s.offset`), not count-based offsets. This avoids holes in the offset space caused by invalidated batches, which would break count-based pagination.
- Included user-op identity is tracked by application nonce logic (no DB uniqueness constraint — removed to allow resubmission after recovery).
- Reads over batch data go through `valid_batches`, `valid_batch_nonces`, and `valid_sequenced_l2_txs` views (defined in `0001_schema.sql`). The views encapsulate the "exclude `invalid_batches`" filter so individual queries don't repeat it.
- The inclusion lane is the **only writer** of open batch/frame state. `Storage::append_user_ops_chunk` and the `close_*` methods trust the in-memory `WriteHead` without per-write sanity checks; FK + PK constraints catch the dangerous failure modes (write to non-existent frame, duplicate `pos_in_frame`).

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

- Prefer small, composable functions at module boundaries (`ingress::api` → `ingress::inclusion_lane` → `storage::ingress`; `egress::l2_tx_feed` ← `storage::egress`).
- Keep application validation/execution deterministic for a given input/state.
- Surface user-facing errors via `ApiError` (in `http.rs`); keep internal failures descriptive but safe.
- Avoid introducing heavy dependencies without strong reason.
- Documentation style: lean. Module headers (1–4 lines) + docs on public methods only when the contract isn't obvious from name+signature. Use inline comments for **why**, never for **what**.

## Testing Guidance

Focus tests on:
- signature + sender validation edge cases
- nonce progression rules
- fee/rejection behavior
- included vs rejected commit behavior
- storage batch atomicity and uniqueness constraints

If adding integration tests, prefer black-box tests around `POST /tx` and commit outcomes.

Some `sequencer` tests use Anvil (Foundry). They run by default and fail with a clear message if `anvil` is not on PATH. Install Foundry or use `nix develop` to get it.

## Definition of Done for Agent Changes

Before finishing, ensure:
1. Code compiles (`cargo check`).
2. Changed behavior is covered by tests (or explain why tests are pending).
3. Formatting/lints are clean (or list any unresolved warnings explicitly).
4. PR summary includes:
   - what changed
   - why it changed
   - risk/compatibility notes

## Sequencer / Scheduler Duality

The system has two sides that must agree on transaction ordering:

- **Sequencer** (off-chain, low-latency): orders user ops into frames and batches, posts them to L1 via the InputBox contract. Gives "soft confirmations" — the ordered stream visible to WebSocket subscribers.
- **Scheduler** (on-chain, inside the rollup): replays the same ordering by reading batches from L1 safe inputs. Each frame's `safe_block` marker tells the scheduler where to splice direct inputs (deposits) between user ops.

The `safe_block` in each frame is the synchronization primitive. When the scheduler processes a frame, it first drains all pending direct inputs whose block number ≤ `safe_block`, then executes the frame's user ops. This guarantees both sides produce the same execution order.

## Batch Staleness and Recovery

> See `docs/recovery/` for the full conceptual model: the batch tree, coloring, nonce poisoning, uncertainty intervals, Silver-only detection, and the preemptive recovery design.
> See `docs/recovery/preemptive.tla` for the TLA+ spec (157M states verified). See `docs/recovery/history/` for the optimistic alternative and design evolution.

A batch becomes **stale** when `inclusion_block - first_frame.safe_block >= max_wait_blocks` (currently 1200 blocks, ~4 hours). This means the batch sat on L1 too long before the scheduler processed it -- by the time it runs, the direct-input splice points are dangerously far behind.

When the scheduler encounters a stale batch, it **skips it entirely** -- no nonce consumed, no state change, no report. It's a true no-op in nonce space.

### Cascading invalidation via nonce poisoning

If a batch is stale, **all subsequent batches are also invalid**. The primary mechanism is nonce poisoning: the scheduler's expected-nonce counter does not advance when a stale batch is skipped. Every subsequent batch arrives with a nonce the scheduler isn't expecting, so it's rejected regardless of its own staleness. Invalidation is therefore a suffix operation: marking batch N invalid cascades to N+1, N+2, ..., including the open batch.

### Silver-only detection (critical constraint)

Recovery must only be triggered when the frontier batch is **Silver** (safe on L1). Detecting staleness on Pending or Bronze batches is unsafe: TLA+ model checking found a race where wallet-nonce mutual exclusion kills the frontier zombie before the scheduler sees it, allowing non-frontier dead batches to pass the nonce check. See `docs/recovery/` "Why Recovery Must Wait for Silver" for the full counterexample.

### Preemptive recovery

Rather than waiting for a batch to become stale on L1, the sequencer uses a **danger threshold** (`MAX_WAIT_BLOCKS - MARGIN`). When the frontier batch's current staleness reaches this threshold:

1. **Go offline** -- stop accepting user ops
2. **Flush mempool** -- submit no-op transactions at all pending `w_nonce` slots, wait for safe finality. This resolves all mempool uncertainty: every slot is either a batch (Silver) or a no-op (dead).
3. **Run recovery** -- on fully-finalized L1 state: populate gold frontier, detect stale Silver, cascade-invalidate, open recovery batch
4. **Resume** -- restart batch submitter and user-op acceptance

### Recovery tables

Two auxiliary tables support recovery:

- **`batch_nonces`** (`batch_index` PK, `nonce`): Separates nonce assignment (batch submitter's job) from batch creation (sequencer's job). Nonces are NOT unique -- after invalidation and recovery, new batches reuse nonces. Assigned by `assign_batch_nonces()` which finds un-nonced valid closed batches and assigns sequential nonces starting from `MAX(nonce) + 1` over non-invalid batches.

- **`safe_accepted_batches`** (`safe_input_index` PK -> `safe_inputs`, `nonce`, `first_frame_safe_block`, `inclusion_block`): A derived log of batch submissions the scheduler would actually execute. Populated by `populate_safe_accepted_batches()`, which simulates the scheduler's acceptance logic: scans safe inputs in order, skips stale batches, and only records submissions where `nonce == expected_nonce`. Duplicates, out-of-order submissions, and old pre-recovery in-flight transactions are automatically skipped.

### Recovery procedure

1. **Populate accepted frontier**: `populate_safe_accepted_batches()` simulates the scheduler's acceptance logic over safe inputs, building the `safe_accepted_batches` table.

2. **Assign nonces**: `assign_batch_nonces()` assigns contiguous nonces to any valid closed batches that don't have one yet.

3. **Detect and recover (atomic)**: `detect_and_recover(max_wait_blocks)` runs inside a single `Immediate` SQLite transaction:
   - Computes the accepted frontier (how many batches the scheduler has accepted).
   - Finds the valid local batch at that nonce (the first unaccepted batch).
   - If it exists and is stale **by inclusion** (it must be Silver at this point), cascade-invalidates ALL batches with index >= stale batch.
   - Opens a fresh recovery batch (insert batch + frame + re-drain pending directs, including any from invalidated batches).
   - Also handles the edge case where a previous boot invalidated the suffix but crashed before reopening -- if no valid open batch exists, one is created.
   - Commits atomically -- either the entire recovery succeeds or nothing changes.

4. **Filtering**: All storage queries that derive state from batch data (`latest_batch_index`, `ordered_l2_txs`, `drained_direct_count`, `l2_tx_count`) exclude rows from `invalid_batches`. Catch-up replay, lane state initialization, and the L2 tx feed automatically skip invalidated transactions. Direct inputs from invalidated batches are re-drained into the recovery batch.

### Nonce decoupling

The local `batch_index` (monotonic, includes invalid batches) is distinct from the batch `nonce` (contiguous over valid batches, stored in `batch_nonces`). After cascade invalidation and recovery, new batches reuse nonces starting from the first invalid nonce. Among valid batches, nonces are unique -- this is what makes the nonce-to-index mapping unambiguous for the recovery path (L1 works in nonce-space, the sequencer in index-space).

### Stateless batch submitter

The batch submitter derives everything from DB + chain state each tick:

1. Assign nonces and populate safe_accepted_batches (write DB metadata).
2. **Danger threshold check** -- compare the frontier batch's `safe_block` against `current_safe_block`. If `current_safe_block - safe_block >= DANGER_THRESHOLD`, trigger preemptive recovery (shutdown for flush + recovery).
3. Derive next nonce from L1 (safe prefix + observed recent transactions).
4. `load_pending_batches(next_nonce)` -- get all pending valid batches with nonce >= next.
5. **Bulk-submit ALL pending batches** with incrementing wallet nonces. Must use `max(walletNonce, nextL1Slot)` as starting nonce. L1 tx nonce guarantees ordering.

### Detection: safe-only, with wall-clock fallback

Staleness is only checked against L1 **safe** state, never latest. If there are stale batches in latest that haven't reached safe yet, they will eventually become safe, and the staleness check will then trigger recovery. This avoids reacting to L1 reorgs.

When L1 is unreachable, the DB-based danger check sees stale (frozen) `current_safe_block` data and may fail to trigger. The batch submitter falls back to **wall-clock estimation**: `estimated_missed_blocks = (now - last_l1_success) / seconds_per_block`. The danger threshold is adjusted downward by this estimate. At startup, a similar wall-clock check uses the oldest valid batch's `created_at_ms` to decide whether to proceed (before danger zone) or block (in danger zone). See `docs/recovery/` "L1 unreachability" for details.

### Two staleness references

The staleness formula is `reference_block - first_frame_safe_block >= MAX_WAIT_BLOCKS`, but the reference block differs by context:

- **Inclusion staleness** (`inclusion_block`): the scheduler's check. Each batch has its own inclusion block. Not monotonic -- a promptly submitted old batch can be healthy while a late-submitted newer batch is stale. Shapes the gold frontier.
- **Current staleness** (`current_safe_block`): the sequencer's detection check. Same reference for all batches. Monotonic within the valid path (earlier batches have smaller `first_frame_safe_block`). The frontier batch is always the most-stale, so the system only needs to check it.

Cascade invalidation does not rely on staleness being monotonic. It follows from nonce poisoning: once one batch is skipped, all subsequent nonces are unreachable (see `docs/recovery/`).

### Key design choices

- **Silver-only detection** -- recovery is triggered only when the frontier batch is Silver (safe on L1). This is critical for correctness: it guarantees the stale batch is permanently on L1 and the scheduler is poisoned before any recovery batch is processed. TLA+ V2 proved this is necessary (see `docs/recovery/`).
- **Preemptive flush** -- the sequencer goes offline and flushes the mempool with no-op transactions before running recovery. This eliminates mempool uncertainty and dead-batch races.
- **No wallet nonce reset** -- `walletNonce` must NOT be reset during recovery. Recovery batches use `w_nonces` past all dead batch slots. The flush consumes dead batch slots by advancing `nextL1Slot` up to `walletNonce`.
- **Wall-clock fallback** -- when L1 is unreachable, the batch submitter and startup recovery use `elapsed / seconds_per_block` to estimate block progression. This prevents the sequencer from silently issuing doomed soft confirmations during extended L1 outages.
- **Cascading invalidation** -- a single stale batch invalidates the entire suffix of batch space, including the open batch.
- **Append-only `invalid_batches` table** rather than mutating existing rows -- consistent with the storage model's append-oriented philosophy.
- **Atomic crash-safe recovery** -- detection, cascade invalidation, and recovery batch opening all happen in one SQLite transaction. A crash at any point leaves the DB unchanged.
- **Frontier-based stale detection** -- `safe_accepted_batches` simulates the scheduler's acceptance logic, so stale detection compares the local batch chain against the accepted frontier rather than matching individual L1 submissions.
- **Direct input re-draining** -- when a batch is invalidated, its direct inputs (deposits) are re-drained into the recovery batch.
- **Idempotent** -- running detection and nonce assignment multiple times is safe (`INSERT OR IGNORE`).
- **Nonce-0 edge case** -- recovery requires at least one Gold ancestor. The TLA+ model uses a genesis sentinel (Gold at nonce 0) to close this hole. The implementation can handle it however is simplest (see `docs/recovery/` for options).
- **`MAX_WAIT_BLOCKS`** is a shared constant in `sequencer-core` (1200), used by both the scheduler and the sequencer.

## Near-Term Roadmap Hints

Expected future evolution areas:
- stronger typing around tx metadata
- persistence for app state or deterministic replay
- explicit L1 block progression input

## Migration Policy

- Current prototype stage: it is acceptable to rewrite baseline migrations for clarity.
- Once environments are shared/deployed: switch to append-only forward migrations.
- Keep schema bootstrap (initial open rows/invariants) explicit and deterministic.
