# AGENTS.md

This file tells AI coding agents and human contributors how to work effectively in this repository. Start here.

## Mission

Build and evolve a **DeFi sequencer** — the off-chain component that gives users low-latency soft confirmations while preserving the on-chain scheduler's canonical authority.

This is **security-critical infrastructure**. Treat every change with the care that financial systems demand. Correctness, determinism, and safety come before features.

The current application (`examples/app-core/`) is a **hardcoded placeholder** (deposit, transfer, withdrawal). It will be replaced by a production DeFi application. The sequencer itself is the product; the app is a stand-in for development.

## Requirements

In order of importance:

1. **Low latency** — `POST /tx` ack under 500 ms.
2. **Financially sustainable** — the system must pay for itself through fees.
3. **Low cost transactions** — cheaper than native L1.

## Invariants

- **Dispute compatibility** — the design already accounts for rollup dispute resolution. Preserve it.
- **Wallet-compatible signing** — users sign with standard wallets via EIP-712. Never introduce custom signing schemes.
- **Deposit availability < 10 minutes** — happy path. The censorship-resistance backstop (`MAX_WAIT_BLOCKS`, ~4h) is the worst case.

## Design Principles

- **App-specific sequencer.** The sequencer may link against the application, enabling validation and execution at ingress time. This is a deliberate design choice.
- **Soft confirmations may be invalidated.** Under adversarial conditions (network, infrastructure, provider, or L1 outages), soft confirmations can be rolled back via recovery. This is by design, not a bug — it is what makes the sequencer sound in the face of liveness failures.
- **App UX may depend on the sequencer.** Without the sequencer, user experience may degrade substantially. This is an acceptable tradeoff: the on-chain scheduler remains the canonical source of truth; the sequencer only accelerates the UX.

## Sequencer / Scheduler Duality

The system has two components in an asymmetric relationship:

### Scheduler — on-chain canonical authority

The scheduler runs inside the rollup and **defines the canonical transaction ordering**. For each batch read from L1 safe inputs, it processes frames in order: drain all pending direct inputs whose block number is ≤ `safe_block`, then execute the frame's user ops. **The scheduler treats the sequencer as potentially Byzantine** — it enforces ordering and staleness rules regardless of what the sequencer claims.

### Sequencer — off-chain predictor

The sequencer knows the scheduler's algorithm. It uses that knowledge to **predict** what the canonical ordering will be once its batches land on L1, and issues soft confirmations to users ahead of time. The sequencer has **write priority on the execution queue**: as long as it keeps advancing `safe_block` and submitting batches, it controls ordering.

### The `safe_block` synchronization primitive

Each frame carries a `safe_block` chosen by the sequencer. It serves two purposes:

- It tells the scheduler how far to drain direct inputs before executing the frame's user ops.
- It is the sequencer's commitment that it has accounted for all direct inputs up to that block.

The sequencer must advance `safe_block` honestly. If it freezes `safe_block` (to censor deposits) or stops submitting batches, the staleness mechanism detects this and forces recovery.

### When soft confirmations match canonical order

Under honest sequencer operation and no infrastructure outages, soft confirmations match the canonical order. This is an **optimistic guarantee** — the sequencer is predicting a future the scheduler has not yet computed. When the sequencer goes offline, submits stale batches, or tries to censor direct inputs, the scheduler's force-drain backstop kicks in and the affected soft confirmations become invalid.

## Batch Staleness and Recovery

### Staleness

A batch is **stale** when `inclusion_block - first_frame.safe_block >= MAX_WAIT_BLOCKS` (1200 blocks, ~4h). Staleness catches two failure modes:

1. **Liveness failure** — the sequencer went offline and failed to submit batches in time.
2. **Censorship** — the sequencer kept submitting batches but froze `safe_block` to hold back direct inputs.

When the scheduler encounters a stale batch, it **skips it entirely** — no nonce consumed, no state change. This is the **censorship-resistance backstop**: the sequencer cannot hold write priority indefinitely without advancing the drain cursor. Direct inputs are force-drained at `MAX_WAIT_BLOCKS`, guaranteeing deposit availability within ~4h even under adversarial conditions.

### Cascading invalidation

If a batch is stale, all existing subsequent batches are also invalid. The scheduler's expected-nonce counter does not advance on a stale skip, so every subsequent batch arrives at an unexpected nonce and is rejected. Invalidation is a suffix operation: marking batch `N` invalid cascades to `N+1`, `N+2`, …, including the open batch. New batches created after recovery are unaffected.

### Preemptive recovery

Rather than waiting for a batch to go stale on L1, the sequencer uses a **danger threshold** (`MAX_WAIT_BLOCKS − MARGIN`). When the frontier batch's staleness reaches this threshold the sequencer:

1. **Goes offline** — stops accepting user ops.
2. **Flushes the mempool** — submits no-op transactions at every pending wallet-nonce slot and waits for safe finality. This consumes all pending slots so adversarially-delayed "zombie" batch submissions cannot land later. The flusher is load-bearing, not defense-in-depth.
3. **Runs recovery** — on fully finalized L1 state: cascade-invalidate stale batches, open a recovery batch, re-drain direct inputs from invalidated batches.
4. **Resumes** — restarts batch submission and user-op acceptance.

### Detection: safe-only, with wall-clock fallback

Staleness is only checked against L1 **safe** state, never latest. Stale batches in latest that haven't reached safe yet will eventually become safe, and the check will fire at that point. This avoids reacting to L1 reorgs.

When L1 is unreachable, the DB-based staleness check sees a frozen `current_safe_block` and may fail to trigger. The batch submitter falls back to **wall-clock estimation**: `estimated_missed_blocks = (now − last_l1_success) / seconds_per_block`, and the danger threshold is adjusted downward by this estimate. Prevents silently issuing doomed soft confirmations during extended L1 outages.

### Formal verification

The preemptive recovery design is verified by bounded TLA+ model checking. See [`docs/recovery/`](docs/recovery/) for the full design, TLA+ specs, and design history. When touching recovery code, read the TLA+ first.

## Threat Model (brief)

See [`docs/threat-model/README.md`](docs/threat-model/README.md) for the full model. Key points when reading or writing code:

- **Trusted:** InputBox contract, our own Ethereum node (fail-stop, not byzantine), operator config, batch-submitter key.
- **Adversarial:** `POST /tx` callers, direct-input senders, the L1 mempool and block builders (zombie transactions are a first-class threat).
- **Semi-trusted, fail-stop:** fallback RPC providers (Infura / Alchemy).
- **Self-trust:** the sequencer trusts its own code is correct. Bugs that emit malformed batches are fault states requiring manual intervention, not threats to defend against at runtime.
- **In scope:** correctness bugs *and* exploitation. Under rollup semantics, a correctness bug that causes scheduler/sequencer state divergence is as severe as direct theft.

Open findings from staged security review live in [`SECURITY_TODO.md`](SECURITY_TODO.md).

## Architecture Map

Top-level layout follows the system's data flow. Each sequencer module corresponds to a writer role; the matching `storage/<role>.rs` holds its storage half.

### Workspace

- `sequencer/` — main sequencer binary and library.
- `sequencer-core/` — shared domain types (`Application`, `SignedUserOp`, `SequencedL2Tx`, `Batch`, `Frame`).
- `examples/app-core/` — placeholder wallet app implementing the `Application` trait.
- `examples/canonical-app/` — on-chain scheduler reference implementation.
- `examples/canonical-test/` — e2e test harness for the canonical app.
- `sdk/rust-client/` — Rust client library for the sequencer API.
- `tests/{benchmarks,e2e,harness}/` — test infrastructure.

### Sequencer module layout

- `sequencer/src/main.rs` — thin binary entrypoint.
- `sequencer/src/lib.rs` — public sequencer API (`run`, `RunConfig`).
- `sequencer/src/http.rs` — shared HTTP error type, JSON `ErrorResponse`, `ApiConfig`, and `axum::serve` orchestration.
- `sequencer/src/runtime/` — process bootstrap, `RunConfig`, EIP-712 domain, `ShutdownSignal`.
- `sequencer/src/ingress/` — public write path.
  - `api.rs` — `POST /tx` handler, JSON-rejection mapping.
  - `inclusion_lane/` — single-lane hot-path loop (`mod.rs`), catch-up replay, config, error types.
- `sequencer/src/egress/` — internal read path.
  - `api/` — `/ws/subscribe`, `/livez`, `/readyz`, `/healthz`.
  - `l2_tx_feed/` — DB-backed ordered-tx feed.
- `sequencer/src/l1/` — L1 client surface.
  - `reader.rs` — safe-input ingestion from InputBox into SQLite.
  - `submitter/` — stateless batch submitter (`worker.rs` + `poster.rs`).
  - `provider.rs` — alloy provider construction.
  - `partition.rs` — long-block-range retry helper.
- `sequencer/src/recovery/` — preemptive recovery startup procedure and mempool flusher.
- `sequencer/src/storage/` — SQLite persistence, split by writer role (`ingress`, `egress`, `l1_inputs`, `l1_submission`, `recovery`, `admin`, plus shared `mod`, `open`, `internals`, and `migrations/`).

## Key Concepts

- **Chunk** — bounded list of user ops processed and persisted together to amortize SQLite cost.
- **Frame** — ordering boundary; commits `safe_block` + user ops.
- **Batch** — list of frames posted on-chain as one L1 transaction (SSZ-encoded).
- **Inclusion lane** — hot-path single-lane loop that dequeues, executes, persists, and rotates frame/batch boundaries. The only writer of open batch/frame state.
- **Batch submitter** — stateless worker that assigns nonces, bulk-submits all pending batches each tick.
- **Input reader** — ingests safe inputs from L1 InputBox into SQLite.
- **L2 tx feed** — DB-backed ordered-tx stream used by WS subscribers.
- **Soft confirmation** — sequencer's predicted ordering, emitted before the batch lands on L1.

## Domain Truths

- API validates the EIP-712 signature and enqueues a `SignedUserOp`. Method payload decoding happens during application execution, not at ingress.
- **Deposits are direct-input-only** (L1 → L2) and must not be represented as user ops.
- Rejections (`InvalidNonce`, `InvalidMaxFee`, `InsufficientGasBalance`) produce no state mutation and are not persisted.
- Included txs are persisted as frame/batch data in `batches`, `frames`, `user_ops`, `safe_inputs`, and `sequenced_l2_txs`. Recovery metadata lives in `batch_nonces`, `safe_accepted_batches`, and `invalid_batches`.
- Frame fee is persisted in `frames.fee` and is fixed for the lifetime of that frame. The next frame's fee is sampled from `batch_policy_derived.recommended_fee` at rotation.
- Wallet state (balances, nonces) is in-memory today — not persisted.
- **EIP-712 domain fields:** `name`, `version`, `chainId`, `verifyingContract`. `chainId` and `verifyingContract` come from `SEQ_CHAIN_ID` and `SEQ_APP_ADDRESS` (validated against the RPC chain id at startup). All four fields must be present on both sides — see [`SECURITY_TODO.md`](SECURITY_TODO.md) for the open divergence finding.

### InputBox payload classification

- The input reader ingests every `InputAdded` event from InputBox. Each event carries an authenticated `msg_sender` (delivered by the Cartesi framework from `EvmAdvanceCall`).
- **Classification is by sender address**, not by a tag byte:
  - Sender == batch-submitter address → SSZ-decoded as `Batch` (scheduler side). The sequencer does not ingest its own batch submissions as direct inputs.
  - Any other sender → stored verbatim as a direct input (deposit).
- The payload is opaque to the classification layer. Application-specific decoding happens inside `Application::execute_direct_input`.

## Application Trait Contract

Implementors of the `Application` trait must respect these contracts. The sequencer assumes them without runtime enforcement.

### Replay determinism

The sequencer persists every included user op and every ingested direct input. On restart, catch-up replays them in order against a fresh `Application` instance to rebuild state. **Any input that succeeded live must succeed on replay.**

- `execute_direct_input` and `execute_valid_user_op` must not return `AppError::Internal` for any byte sequence that previously executed successfully. Catch-up treats `Internal` as fatal: it aborts startup and leaves the sequencer unable to resume.
- Prefer `ExecutionOutcome::Invalid` for malformed or ill-typed input caught at the app level. Reserve `AppError::Internal` for genuine invariant violations ("validated user op cannot pay fee") — real bugs, not adversarial inputs. `Invalid` is replay-safe; `Internal` is not.
- `validate_user_op` must be pure over the current app state. No side effects, no time dependence, no randomness.

### No implicit state

Application state changes must flow exclusively through `execute_valid_user_op` and `execute_direct_input`. Mutating state from `validate_user_op` or `current_user_nonce` breaks replay determinism.

## Hot-Path Invariants

- API ack is tied to chunk durability, not frame/batch closure.
- Chunk commit and ack remain low-latency; frame closure is orthogonal and can happen less frequently.
- `POST /tx` queue admission: `try_send` on a full queue returns `429 OVERLOADED` with message `queue full`.
- Frame closure happens when direct inputs are drained, and also whenever batch closure happens.
- Batch closure is controlled by batch policy (size and/or deadline).
- Preserve single-lane deterministic ordering. Do not introduce extra concurrency in hot-path ordering logic without explicit approval.

## Storage Invariants

- Storage model is append-oriented; avoid mutable status flags for open/closed entities.
- Open batch/frame are derived by "latest row" convention.
- A frame's leading direct-input prefix is derivable from `sequenced_l2_txs` plus `frames.safe_block`.
- Safe cursor/head values should be derived from persisted facts when possible, not duplicated as mutable fields.
- Replay/catch-up uses persisted ordering plus persisted frame fee (`frames.fee`) to mirror inclusion semantics exactly.
- Cursor pagination for ordered L2 txs uses **SQLite rowid**, not count-based offsets. Holes from invalidated batches would break count-based pagination.
- Included user-op identity is tracked by application nonce logic; no DB uniqueness constraint (removed to allow resubmission after recovery).
- **Reads over batch data go through `valid_batches`, `valid_batch_nonces`, and `valid_sequenced_l2_txs` views.** These encapsulate the "exclude `invalid_batches`" filter so individual queries don't repeat it. Writers go to the base tables.
- The inclusion lane is the **only writer** of open batch/frame state. `Storage::append_user_ops_chunk` and the `close_*` methods trust the in-memory `WriteHead`; FK + PK constraints catch the dangerous failure modes.

## Type Boundaries

- `SignedUserOp` — ingress/API signature domain (post-validation, pre-execution).
- `ValidUserOp` — application execution domain (after validation boundary).
- `SequencedL2Tx` — ordered replay/fanout domain (`UserOp | DirectInput`).
- Keep DB-only helper types private to storage modules; prefer shared domain types at module boundaries.

## HTTP Endpoints

- **Ingress** (public-facing): `POST /tx`.
- **Egress** (internal indexers): `GET /ws/subscribe`, `GET /livez`, `GET /readyz`, `GET /healthz`.

Today both sides serve from one listener; the planned API split puts each side on its own port (same binary) so internal probes and subscribers can be firewalled from public submit traffic.

`/ws/subscribe` internal guardrails: subscriber cap 64, catch-up cap 50000. When the catch-up window is exceeded, the handler upgrades and then closes with WebSocket close code `1008` (`POLICY`), reason `catch-up window exceeded`.

Health semantics: `/livez` — 200 if the process is alive. `/readyz` — 200 if shutdown not requested AND inclusion-lane channel still open, else 503. `/healthz` — JSON `{ status, inclusion_lane }` mirroring the same 200/503.

## Environment Variables

**Required:**

- `SEQ_ETH_RPC_URL`
- `SEQ_CHAIN_ID`
- `SEQ_APP_ADDRESS`
- `SEQ_BATCH_SUBMITTER_PRIVATE_KEY` or `SEQ_BATCH_SUBMITTER_PRIVATE_KEY_FILE`

**Optional:**

- `SEQ_HTTP_ADDR` (default `127.0.0.1:3000`)
- `SEQ_DATA_DIR` (default `sequencer-data`; DB file `sequencer.db` inside it)
- `SEQ_LONG_BLOCK_RANGE_ERROR_CODES`
- `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS` (default 5000)
- `SEQ_BATCH_SUBMITTER_CONFIRMATION_DEPTH` (default 2)
- `SEQ_PREEMPTIVE_MARGIN_BLOCKS` (default 75)
- `SEQ_SECONDS_PER_BLOCK` (default 12)

## Coding Conventions

- Prefer small, composable functions at module boundaries (`ingress::api` → `ingress::inclusion_lane` → `storage::ingress`; `egress::l2_tx_feed` ← `storage::egress`).
- Keep application validation and execution deterministic for a given input/state. No `SystemTime::now()`, `HashMap` iteration order, or floating-point in consensus paths.
- Surface user-facing errors via `ApiError` (in `http.rs`); keep internal failures descriptive but safe.
- Avoid introducing heavy dependencies without strong reason.
- Documentation style: lean. Module headers (1–4 lines) + docs on public methods only when the contract isn't obvious from name+signature. Use inline comments for **why**, never for **what**.
- **Don't layer defense-in-depth checks against sequencer self-bugs.** Correctness is enforced via tests and review. See "Self-trust" in [`docs/threat-model/README.md`](docs/threat-model/README.md).

## Testing Guidance

Focus tests on:

- Signature + sender-validation edge cases.
- Nonce progression rules.
- Fee and rejection behavior.
- Included-vs-rejected commit behavior.
- Storage batch atomicity and uniqueness constraints.
- Scheduler/sequencer agreement — any invariant the two sides share should have at least one test that exercises both.

Prefer black-box tests around `POST /tx` and commit outcomes for integration.

Some `sequencer` tests use Anvil (Foundry). They run by default and fail with a clear message if `anvil` is not on PATH. Install Foundry or use `nix develop`.

## Fast Start Commands

See [`CLAUDE.md`](CLAUDE.md) for shell setup and the full command list. In short:

```bash
cargo check
cargo test --workspace --exclude canonical-test
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

## Always / Ask First / Never

### Always

- Keep inclusion-vs-rejection semantics explicit for transaction handling.
- Preserve API error shape and status code mapping unless intentionally changing the API contract.
- Add or update tests when logic changes.
- Run at least `cargo check` before finishing.
- Read `docs/recovery/` before touching recovery code, and `docs/threat-model/` before touching trust-boundary code.

### Ask First

- Changing tx wire format (`UserOp`, SSZ payload layout, EIP-712 domain fields).
- Changing DB schema or migration strategy.
- Altering rejection semantics (what consumes nonce/gas vs what is rejected).
- Introducing concurrency changes to commit ordering.
- Changing chunk/frame/batch closure or ack semantics.

### Never

- Silently weaken signature validation.
- Merge behavioral changes with unrelated refactors in one patch.
- Rely on implicit defaults for consensus-relevant values.
- Remove guardrails around queue backpressure or inclusion-lane error reporting.

## Migration Policy

At this stage it is acceptable to rewrite baseline migrations for clarity. There are no deployed environments requiring forward-only migrations. Keep schema bootstrap (initial open rows and invariants) explicit and deterministic.

Once environments are shared or deployed, switch to append-only forward migrations.

## Definition of Done

Before finishing a change, ensure:

1. Code compiles (`cargo check`).
2. Changed behavior is covered by tests, or explain why tests are pending.
3. Formatting and lints are clean, or list any unresolved warnings explicitly.
4. PR summary includes **what changed**, **why it changed**, and **risk / compatibility notes**.

## Related Documents

- [`README.md`](README.md) — product framing, user-facing trust model.
- [`CLAUDE.md`](CLAUDE.md) — shell setup, quick reference, pointer back here.
- [`docs/threat-model/README.md`](docs/threat-model/README.md) — trust boundaries, in-scope and out-of-scope threats.
- [`docs/recovery/README.md`](docs/recovery/README.md) — recovery design, TLA+ formal verification, design history.
- [`SECURITY_TODO.md`](SECURITY_TODO.md) — open security findings from staged review.
- [`sequencer-core/`](sequencer-core/) — shared domain types and protocol contracts.
