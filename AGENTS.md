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
- **SQLite-centered local coordination.** Components publish and consume durable local facts through their owned SQLite tables. The on-chain scheduler remains canonical authority; SQLite is the sequencer's local coordination plane. HTTP ingress ↔ inclusion lane MPSC/oneshot is the deliberate exception because low-latency request/response over the lane's in-memory application is unwieldy through SQLite. Do not turn that exception into a general in-memory component bus. Full statement: [ADR mechanism 4](docs/plans/2026-08-authority-boundary-adr.md).
- **Append-oriented storage.** Avoid mutable status flags for open/closed entities; prefer write-once NULL→value transitions with one owner each, and derive cursors and heads from persisted facts rather than duplicating them as mutable fields.
- **Assumption-driven robustness.** Every hardening mechanism must name the invariant it protects, the assumptions under which it is needed, and a trigger for revisiting them. Machinery for failures outside the supported model enlarges the state surface developers must audit and can make the system less robust rather than more.
- **The complexity budget belongs to concurrency, mutual exclusion, durability, and hostile-L1 robustness.** This sequencer is not algorithmically complex. A large file is a smell: an invariant we're not seeing, a reasonable assumption we're not taking, or plain over-engineering. Judge every mechanism — current or proposed — against its weight.

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

### Where the duality lives in code (change one, check all)

The precise acceptance algorithm — decode → sender → nonce → structural →
staleness → frame execution order → nonce advance — is **owned by
[`docs/protocol/scheduler-semantics.md`](docs/protocol/scheduler-semantics.md)**.
This section is the map.

Scheduler-acceptance semantics exist in exactly three implementations that must agree:

1. the canonical fold — `Scheduler<A>` ([`sequencer-core/src/scheduler/mod.rs`](sequencer-core/src/scheduler/mod.rs)), the same source compiled into the on-chain machine and driven bare-metal by the recovery fold;
2. the off-chain acceptance predicate — `ProtocolTiming::scheduler_accepts` ([`sequencer-core/src/protocol.rs`](sequencer-core/src/protocol.rs)), which feeds `safe_accepted_batches`;
3. the inclusion lane's live prediction (drain + execution order).

The expected-nonce fold is homed next to `scheduler_accepts` as `advance_expected_batch_nonce` (same file); the submitter's `decide_submit_start` consumes it, and `populate_safe_accepted_batches` keeps a deliberate inline copy (its advance is interleaved with storage-only side effects — the content-identity check and the divergence freeze — that can't move below the protocol layer). Touching any of these means re-checking the others — their agreement is the system's most load-bearing invariant (see [`docs/invariants.md`](docs/invariants.md)).

Two mechanical facts the agreement rests on:

- **Drain attribution.** Accumulated newly-safe directs land in the clock-advanced frame — frame K reads "directs ≤ S_K, then user ops validated on top", exactly the scheduler's drain-before-ops rule ([I2](docs/invariants.md)).
- **Empty batches are never stale and consume the nonce** (no first frame to measure staleness against). Consistent across all implementations, test-pinned.

## Batch Staleness and Recovery

### Staleness

A batch is **stale** when `inclusion_block - first_frame.safe_block >= MAX_WAIT_BLOCKS` (1200 blocks, ~4h). Staleness catches two failure modes:

1. **Liveness failure** — the sequencer went offline and failed to submit batches in time.
2. **Censorship** — the sequencer kept submitting batches but froze `safe_block` to hold back direct inputs.

When the scheduler encounters a stale batch, it **skips it entirely** — no nonce consumed, no state change. This is the **censorship-resistance backstop**: the sequencer cannot hold write priority indefinitely without advancing the drain cursor. Direct inputs are force-drained at `MAX_WAIT_BLOCKS`, guaranteeing deposit availability within ~4h even under adversarial conditions.

### Cascading invalidation

If a batch is stale, all existing subsequent batches are also invalid. The scheduler's expected-nonce counter does not advance on a stale skip, so every subsequent batch arrives at an unexpected nonce and is rejected. Invalidation is a suffix operation: marking batch `N` invalid cascades to `N+1`, `N+2`, …, including the open batch. New batches created after recovery are unaffected.

### Preemptive recovery

Rather than waiting for a batch to go stale on L1, the sequencer uses a **danger threshold** (`MAX_WAIT_BLOCKS − MARGIN`). The threshold is *only a trigger*: it tells the system "stop running, hand off to recovery." It does not encode "this batch is doomed" — that decision belongs to the post-flush cascade.

The cycle crosses a process boundary by design: the in-process
[`DangerDetector`](sequencer/src/recovery/detector.rs) polls
`Storage::check_danger` on a cadence and returns a non-`Safe` worker exit; the
runtime closes intake and drains before the command returns non-zero (stopping
the process is how the sequencer goes offline). Terminal containment has a
hard abort fallback; expected-recovery and retryable exits are graceful; the
orchestrator respawns; on every boot the startup reducer re-derives the
response from local facts.

The authoritative dispatch table, phase ordering, boot-local witnesses, the
admission sequence, the "everything past gold is doomed" model, and the
per-path rationale live in
[`docs/recovery/README.md`](docs/recovery/README.md) — that document **owns**
the recovery design; this section is only the map. Do not restate dispatch
details here.

### Detection: safe-only, with wall-clock fallback

Staleness is only checked against L1 **safe** state, never latest. Stale batches in latest that haven't reached safe yet will eventually become safe, and the check will fire at that point. This avoids reacting to L1 reorgs.

When the sequencer's view of L1 stops advancing — most often because the RPC gateway is stalled or returning stale reads, occasionally because L1 itself is unhealthy — the DB-based staleness check sees a frozen `current_safe_block` and may fail to trigger. The danger detector uses two wall-clock signals: the recorded L1 safe block timestamp must remain younger than `CARTESI_SEQUENCER_L1_READ_STALE_AFTER_BLOCKS`, and unresolved batches are also checked with `estimated_missed_blocks = (now − last_safe_progress_ms) / seconds_per_block` by adjusting the danger threshold downward. This prevents silently issuing doomed soft confirmations during stale-provider periods or L1 outages.

### Formal verification

The recovery design is verified by two bounded TLA+ models;
[`docs/recovery/README.md`](docs/recovery/README.md) "Formal Verification"
says what each proves. When touching recovery code, read both current models
first.

## Threat Model (brief)

See [`docs/threat-model/README.md`](docs/threat-model/README.md) for the full model. Key points when reading or writing code:

- **Trusted:** InputBox contract, our own Ethereum node (fail-stop, not byzantine), operator config, batch-submitter key.
- **Adversarial:** `POST /tx` callers, direct-input senders, the L1 mempool and block builders (zombie transactions are a first-class threat).
- **RPC endpoint:** single (`CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`), trusted fail-stop, **must be one consistent node** — no fallback tier exists yet (see the threat model's actor table).
- **Self-trust:** the sequencer trusts its own code is correct. Bugs that emit malformed batches are fault states requiring manual intervention, not threats to defend against at runtime.
- **In scope:** correctness bugs *and* exploitation. Under rollup semantics, a correctness bug that causes scheduler/sequencer state divergence is as severe as direct theft.

## Architecture Map

Top-level layout follows the system's data flow. Each sequencer module corresponds to a writer role; the matching `storage/<role>.rs` holds its storage half.

### Workspace

- `sequencer/` — sequencer **library** (no binary). App crates compose it into a binary.
- `sequencer-core/` — shared domain types (`Application`, `SignedUserOp`, `SequencedL2Tx`, `Batch`, `Frame`).
- `examples/app-core/` — placeholder wallet app implementing the `Application` trait.
- `examples/wallet-sequencer/` — binary crate: wallet app + sequencer library. The model for what an app author builds (their `Application` impl ≙ `app-core`; their binary crate ≙ this).
- `examples/canonical-app/` — on-chain scheduler reference implementation.
- `examples/canonical-test/` — e2e test harness for the canonical app.
- `sdk/rust-client/` — Rust client library for the sequencer API.
- `tests/{benchmarks,e2e,harness}/` — test infrastructure.

### Sequencer module layout

- `sequencer/src/lib.rs` — public sequencer API. The thin binary entrypoints live in `examples/wallet-sequencer/`.
- `sequencer/src/harness.rs` — CLI harness: the `setup`/`run`/`flush-mempool` subcommand parser, `dispatch`, and the exit-code projection. An app's `main` is ~5 lines (`run_main` + a genesis-app closure).
- `sequencer/src/http.rs` — shared HTTP error type, JSON `ErrorResponse`, `ApiConfig`, and `axum::serve` orchestration.
- `sequencer/src/commands/` — the operator command brackets: `setup` (phase A — pin identity, initial sync, genesis snapshot, atomic `setup_complete` fact), `run` (phase B — recover, prepare, admit, and boot workers; its `workers` supervisor lives beside it), and `flush` (`flush-mempool`). `sequencer/src/commands/` also owns the command-scoped `config` and `error` taxonomy (incl. the exit-code projection); `sequencer/src/runtime/` is exactly the runtime authority capabilities — the process lock and `shutdown` (runtime scope/containment) — consumed crate-wide. `L1Config` lives in `sequencer/src/l1/`; the crate-wide wall clock is `sequencer/src/clock.rs`.
- `sequencer/src/ingress/` — public write path.
  - `api.rs` — `POST /tx` handler, JSON-rejection mapping.
  - `inclusion_lane/` — single-lane hot-path loop (`mod.rs`), catch-up replay, config, error types.
- `sequencer/src/egress/` — internal read path.
  - `api/` — `/ws/subscribe`, `/livez`, `/readyz`, `/healthz`.
  - `l2_tx_feed/` — DB-backed ordered-tx feed.
- `sequencer/src/l1/` — L1 client surface.
  - `reader.rs` — safe-input ingestion from InputBox into SQLite.
  - `submitter/` — stateless batch submitter (`worker.rs` + `poster.rs`).
  - `fee_oracle/` — setup-pinned L1 Uniswap V3 TWAP → `batch_policy.log_gas_price` (+ `log_gas_price_updated_at_ms`); fixed mode writes once at setup and has no worker.
  - `eip1559.rs` — shared EIP-1559 fee estimation (poster + oracle).
  - `provider.rs` — alloy provider construction.
  - `partition.rs` — long-block-range retry helper.
- `sequencer/src/recovery/` — preemptive recovery startup procedure (`mod.rs`), runtime danger detector (`detector.rs`), and mempool flusher (`flusher.rs`).
- `sequencer/src/storage/` — SQLite persistence, split by writer role (`ingress`, `egress`, `l1_inputs`, `l1_submission`, `recovery`, `admin`, `safe_accepted_batches`, `snapshot_dumps`, plus shared `history`, `mod`, `open`, `convert`, `queries`, `mutations`, and `migrations/`).

## Key Concepts

- **Chunk** — bounded list of user ops processed and persisted together to amortize SQLite cost.
- **Frame** — ordering boundary; commits `safe_block` + user ops.
- **Batch** — list of frames posted on-chain as one L1 transaction (SSZ-encoded).
- **Inclusion lane** — the single ordering lane, with a latency-critical user-op regime and a slower L1-reconciliation regime ([ADR mechanism 4](docs/plans/2026-08-authority-boundary-adr.md)); the only writer of open batch/frame state ([I17](docs/invariants.md)) and the system's execution bottleneck.
- **Batch submitter** — stateless worker that bulk-submits all pending batches each tick. Nonces are assigned by storage (structural `parent.nonce + 1`) when batches are closed; the submitter just reads them.
- **Danger detector** — background worker that polls `Storage::check_danger` on a fixed cadence and exits with `RecoveryRequired` when any non-`Safe` danger status fires. Never writes to the DB; never talks to L1. Crashes the process so startup recovery or refusal can run.
- **Fee oracle** — setup pins either a fixed exponent or a reviewed Uniswap V3 WETH/X TWAP tuple into deployment identity, and writes the first `log_gas_price` (+ observation stamp) in both modes. Setup requires a successful live quote; `run` performs no fee-source read before recovery/admission. Fixed mode has no worker; Uniswap launches a lazy refresher that immediately attempts a quote, persists successes, and retains the last price while logging and retrying transient source failures. The stamp is telemetry, not a runtime-admission or expiry gate. A shared-endpoint outage/stale view is already detected from L1 safe-head progress; a fee-source-only outage is an accepted economic residual (stale-low may subsidize DA, stale-high may reject users), not a canonical-correctness fault. Deterministic source misconfiguration, fatal arithmetic, and persistent storage faults remain terminal. The 10× margin lives in `batch_policy.log_slack`; it is a buffer rather than a bound on market movement, and frame fees stay immutable until the next frame opens.
- **Input reader** — ingests safe inputs from L1 InputBox and maintains the durable safe head, accepted-batch projection, and divergence marker in one atomic transaction (`sequencer/src/storage/l1_inputs.rs`); it hands the lane no in-memory cursor.
- **L2 tx feed** — DB-backed ordered-tx stream used by WS subscribers. The
  existing endpoint still paginates by the physical SQLite rowid cursor.
  SQLite now also stores the canonical `ExecutedInputCount` attribution for
  every application input; switching the public feed and history-version
  handshake to that coordinate remains Track 3 API work.
- **Application progress** — scheduler-owned
  `(ExecutedInputCount, last_executed_safe_block)` embedded in every
  application dump. Shared execution functions advance it and return the
  input's pre-execution offset. SQLite records that offset atomically with the
  corresponding valid replay row; only the WebSocket/HTTP projection remains
  Track 3 work.
- **History version** — `(EraId, RecoveryGeneration)`. The durable metadata
  foundation is landed: a new baseline mints an immutable UUIDv4 era and starts
  generation zero; standard recovery increments it exactly once iff its
  transaction invalidates at least one valid batch. The current feed does not
  expose or enforce the pair yet.
- **Soft confirmation** — sequencer's predicted ordering, emitted before the batch lands on L1.
- **Snapshot** — durable copy of the app's canonical state at one physical
  replay cursor and one canonical `ExecutedInputCount`; *pending* at batch
  close, *promoted* to finalized on L1 observation (per-range, atomically with
  the drain), garbage-collected when superseded. Catch-up refuses if the
  loaded app count, stored snapshot count, or per-row execution attributions
  disagree. Lifecycle + rationale (incl. the promote/drain crash-safety):
  [`docs/snapshots/lifecycle.md`](docs/snapshots/lifecycle.md).

## Domain Truths

- API validates the EIP-712 signature and enqueues a `SignedUserOp`. Method payload decoding happens during application execution, not at ingress.
- **Deposits are direct-input-only** (L1 → L2) and must not be represented as user ops.
- Rejections (`InvalidNonce`, `InvalidMaxFee`, `InsufficientFeeBalance`) produce no state mutation and are not persisted. These are protocol-level rejection semantics every app must implement: nonces prevent user-op replay, fees prevent spam against the sequencer's DA budget. ("Fee", not "gas" — the fee tracks DA; compute metering, if it ever exists, is a separate future concept.)
- Included txs are persisted as frame/batch data in `batches`, `frames`, `user_ops`, `safe_inputs`, and `sequenced_l2_txs`. Recovery metadata lives in `safe_accepted_batches`; batch lifecycle state (sealed/invalidated) lives on the `batches` row itself as write-once timestamps.
- Frame fee is persisted in `frames.fee` and is fixed for the lifetime of that frame. The next frame's fee is currently sampled from `batch_policy_derived.recommended_fee` at rotation; oracle bootstrap writes the price before any Tip can sample it, and `log_slack` applies the 10× margin in log space. This is present behavior, not a reason for the five-block clock policy; hoisting fee to the batch is a later design with its own trade-offs.
- Wallet state (balances, nonces) is in-memory today — not persisted.
- **EIP-712 domain fields:** `name`, `version`, `chainId`, `verifyingContract`. `chainId` and `verifyingContract` come from `CARTESI_SEQUENCER_BLOCKCHAIN_ID` and `CARTESI_SEQUENCER_APP_ADDRESS` (validated against the RPC chain id at startup). All four fields must be present on both sides — both the sequencer and the on-chain scheduler construct the domain via `sequencer_core::build_input_domain`, the canonical shared constructor.

### InputBox payload classification

- The input reader ingests every `InputAdded` event from InputBox. Each event carries an authenticated `msg_sender` (delivered by the Cartesi framework from `EvmAdvanceCall`).
- **Classification is by sender address**, not by a tag byte:
  - Sender == batch-submitter address → SSZ-decoded as `Batch` (scheduler side). The sequencer does not ingest its own batch submissions as direct inputs.
  - Any other sender → stored verbatim as a direct input (deposit).
- The payload is opaque to the classification layer. Application-specific decoding happens inside `Application::apply_direct_input`, reached only through the shared `execute_direct_input` boundary.

## Application Trait Contract

Implementors of the `Application` trait must respect these contracts. The shared execution boundary enforces scheduler-owned count/clock progress; application-specific determinism and mutation remain self-trusted. The full, code-grounded contract — method table, dump round-trip durability, the safe-block clock — is **owned by [`docs/protocol/application-contract.md`](docs/protocol/application-contract.md)**; the essentials follow.

### Replay determinism

The sequencer persists every included user op and every ingested direct input. On restart, catch-up replays them in order against a fresh `Application` instance to rebuild state. **Any input that succeeded live must succeed on replay.**

- `apply_direct_input` and `apply_valid_user_op` must not return `AppError::Internal` for any byte sequence that previously executed successfully. The canonical scheduler, catch-up, and recovery fold treat `Internal` as fatal: no canonical successor is defined.
- Prefer `ExecutionOutcome::Invalid` for malformed or ill-typed input caught at the app level. Reserve `AppError::Internal` for genuine invariant violations ("validated user op cannot pay fee") — real bugs, not adversarial inputs. `Invalid` is replay-safe; `Internal` is not.
- `validate_user_op` must be pure over the current app state. No side effects, no time dependence, no randomness.

### No implicit state

Application-specific state changes flow exclusively through the `apply_valid_user_op` and `apply_direct_input` hooks. Scheduler-owned `ApplicationProgress` (executed-input count plus safe-block clock) changes only through the shared free execution functions. Mutating state from `validate_user_op` breaks replay determinism.

### One execution entry point

User ops are executed only through `sequencer_core::application::validate_and_execute_user_op`; already-validated user ops and directs use the shared `execute_valid_user_op` / `execute_direct_input` free functions. Raw hooks and mutable progress access require distinct borrowed opaque capabilities whose constructors are private to this boundary. It preflights the checked successor, verifies progress stayed unchanged after validation and after the hook on both `Ok` and `Err`, commits only after `Ok`, then re-reads the getter to assert accessor coherence. Count zero implies clock zero. `AppError` is fatal and defines no canonical successor; callers discard the application instance rather than resume it. The inclusion lane, canonical scheduler, catch-up, and recovery fold all use this boundary — part of the duality agreement.

## Hot-Path Invariants

The hot-path rules are owned elsewhere; this section is only the map.

- Drain attribution, frame-clock monotonicity, the
  content-identity check and the divergence freeze, history metadata, the
  `WriteHead` cache, and the executed-inputs projection are registered in
  [`docs/invariants.md`](docs/invariants.md) (the fail-loud check policy plus
  I2, I3, I9, I10, I12–I18, I20) — that register owns them; do not restate
  them here.
- Command admission, containment, the two-regime lane and its
  acknowledgement rule, and role-local authority are owned by the
  [authority-boundary ADR](docs/plans/2026-08-authority-boundary-adr.md)
  (mechanisms 1, 2, and 4); the recovery reducer by
  [`docs/recovery/README.md`](docs/recovery/README.md).
- The frame-clock policy (five newly-safe blocks, one frame at the observed
  tip, never interpolated, and its revisit trigger) is owned by
  [`docs/protocol/scheduler-semantics.md`](docs/protocol/scheduler-semantics.md);
  the no-preemption digestibility assumption by
  [`docs/protocol/application-contract.md` §5](docs/protocol/application-contract.md#5-operational-capacity-for-l1-reconciliation).
- Queue admission (`429 OVERLOADED`), batch closure, and every other API or
  storage-model shape are owned by [`README.md`](README.md).

One rule lives here because nothing else owns it: preserve single-lane
deterministic ordering. Do not introduce extra concurrency in hot-path
ordering logic without explicit approval.

## Storage Invariants

Owned by [`docs/invariants.md`](docs/invariants.md): the writer-role table
(one writer role per fact), the `valid_*` view rule, `WriteHead` coherence
(I17), history metadata (I18), the replay cursor and the executed-inputs
projection (I10, I20). The schema
(`sequencer/src/storage/migrations/0001_schema.sql`) owns the write-once
batch lifecycle, the Tip's uniqueness, and the user-op identity rule. Do not
restate them here.

## Type Boundaries

- `SignedUserOp` — ingress/API signature domain (post-validation, pre-execution).
- `ValidUserOp` — application execution domain (after validation boundary).
- `SequencedL2Tx` — ordered replay/fanout domain (`UserOp | DirectInput`).
- `ExecutedInputCount` — canonical application-history boundary (`X` means the
  next input is entry `X`), never a SQLite cursor. Checked arithmetic only.
- `ReplayL2TxRow` — crate-private named pairing of a physical DB cursor,
  `SequencedL2Tx`, frame clock, and optional canonical attribution; do not
  collapse these coordinates back into a positional tuple.
- Keep DB-only helper types private to storage modules; prefer shared domain types at module boundaries.

## HTTP Endpoints

- **Ingress** (public-facing): `POST /tx`.
- **Egress** (internal indexers/watchdog): `GET /ws/subscribe`, `GET /finalized_state`, `GET /finalized_state/inclusion_block`, `GET /latest_snapshot`, `GET /livez`, `GET /readyz`, `GET /healthz`. The snapshot/state endpoints are **operator-only** (no auth) and must not be exposed publicly; the streaming routes hold a GC lease for the response lifetime ([`docs/snapshots/lifecycle.md`](docs/snapshots/lifecycle.md)).

Today both sides serve from one listener; the planned API split puts each side on its own port (same binary) so internal probes and subscribers can be firewalled from public submit traffic.

Message shapes, caps, close codes, and health semantics are **owned by [`README.md`](README.md)** (the API contract) — do not restate them here.

## Environment Variables

Split by subcommand (the phase split). **`setup`** (required):

- `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`
- `CARTESI_SEQUENCER_BLOCKCHAIN_ID`
- `CARTESI_SEQUENCER_APP_ADDRESS`
- `CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS` (the submitter address — `setup` is L1-read-only and never signs). **Must be a dedicated address**: `setup`'s detection gate refuses if the submitter's wallet nonce is unsettled, so reusing a busy address (e.g. the contract deployer, whose deploy-tx tail isn't safe at setup time) false-positives. The devnet uses anvil account 9 (`DEVNET_SEQUENCER_ADDRESS`), distinct from the account-0 deployer.
- `CARTESI_SEQUENCER_CHECKPOINT_BLOCK` (optional, default `0` = genesis) — the trusted checkpoint machine's L1 inclusion block. `setup` refuses (typed `SetupRefuse`, exit 40 = run `setup --recovery`) if a previous instance left work past it; plain `setup` detects only, and loading a non-genesis checkpoint machine is `setup --recovery`.

**`run`** (required) — chain id / app address / submitter address are read from the DB `setup` pinned, not from args:

- `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`
- `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY` or `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE`

**Optional** (names only — defaults and semantics are **owned by
[`sequencer/src/commands/config.rs`](sequencer/src/commands/config.rs)**; a
defaults list here drifted once already): `CARTESI_SEQUENCER_HTTP_ADDR`, `CARTESI_SEQUENCER_DATA_DIR`,
`CARTESI_SEQUENCER_LONG_BLOCK_RANGE_ERROR_CODES`, `CARTESI_SEQUENCER_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`,
`CARTESI_SEQUENCER_BATCH_SUBMITTER_CONFIRMATION_DEPTH`, `CARTESI_SEQUENCER_PREEMPTIVE_MARGIN_BLOCKS`,
`CARTESI_SEQUENCER_L1_READ_STALE_AFTER_BLOCKS` (fixed default, independent of the margin;
must be strictly below the danger threshold or startup refuses),
`CARTESI_SEQUENCER_SECONDS_PER_BLOCK`, and the runtime-only
`CARTESI_SEQUENCER_FEE_ORACLE_POLL_INTERVAL_MS`. Setup-only fee-oracle source knobs are
(`CARTESI_SEQUENCER_FEE_ORACLE_FIXED_LOG_GAS_PRICE`,
`CARTESI_SEQUENCER_FEE_TOKEN_ADDRESS`,
`CARTESI_SEQUENCER_WETH_ADDRESS`, `CARTESI_SEQUENCER_UNISWAP_V3_POOL`,
`CARTESI_SEQUENCER_FEE_ORACLE_TWAP_WINDOW_SECS` — mainnet/Sepolia default to
 pinned USDC pool presets; other chains require an explicit source).

## Coding Conventions

- Prefer small, composable functions at module boundaries (`ingress::api` → `ingress::inclusion_lane` → `storage::ingress`; `egress::l2_tx_feed` ← `storage::egress`).
- Keep application validation and execution deterministic for a given input/state. No `SystemTime::now()`, `HashMap` iteration order, or floating-point in consensus paths.
- Surface user-facing errors via `ApiError` (in `http.rs`); keep internal failures descriptive but safe.
- Avoid introducing heavy dependencies without strong reason.
- Documentation style: lean. Module headers (1–4 lines) + docs on public methods only when the contract isn't obvious from name+signature.
- **Comment the non-obvious, not the self-evident.** Keep comments concise; avoid redundant and excessive inline commentary. Do not restate what the code already expresses. Explain the why, edge cases, invariants, and subtle behaviors that cannot be inferred from reading the code alone.
- Review-item codenames (finding/decision ids from past review ledgers) never appear in code comments or living docs — state the reason itself, or point at the invariant register entry that owns it. Invariant ids (`I1`–`I20`) are stable register references and are fine.
- **Impossible states fail loud; they are never handled.** Cheap cross-module assertions of *real invariants* are encouraged (assert, trigger `RAISE`, typed error). Failing loud is safety-preserving, not necessarily self-healing: transient failures may clear on restart, while a persistent invariant violation is terminal and may require inspection or cockroach recovery. Silent divergence is never acceptable. Never add graceful fallbacks, neighbor re-validation, or silent absorbers (`INSERT OR IGNORE`, saturating decode of impossible data) for states the contracts rule out; and an assertion must check a real invariant, never an environmental assumption (clock monotonicity is the cautionary tale). Decision test and rationale: [`docs/invariants.md`](docs/invariants.md); trust boundaries: "Self-trust" in [`docs/threat-model/README.md`](docs/threat-model/README.md).

## Documentation Practice

The corpus has two tenses, kept strictly apart:

- **Living docs are timeless.** This file, `README.md`, `docs/protocol/`,
  `docs/invariants.md`, `docs/recovery/`, `docs/snapshots/`,
  `docs/threat-model/`, `docs/watchdog/`, and `docs/plans/` describe what is
  true now and why — present tense, reasoning inline, no dates, no amendment
  banners, no review codenames, no "previously/no longer". Each doc owns its
  topic; others point at it rather than restating it.
- **History lives only in `docs/review/` and commit messages.** A review
  ledger is append-only while its review is open. When it closes, distill it:
  promote conclusions into the living docs, record settled decisions and
  refuted proposals in [`docs/review/register.md`](docs/review/register.md),
  and delete the process narration. Conclusions with reasoning outlive the
  path taken to them.
- **Record deliberate absence once**, at the seam where someone would re-add
  the mechanism, phrased as a positive design statement with its reason —
  never as removal notices scattered across documents.

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

Run server (two phases — `setup` once, then `run`; see `README.md` "Running"):

```bash
# setup (L1-read-only; takes the submitter ADDRESS, not the key)
CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=http://127.0.0.1:8545 \
CARTESI_SEQUENCER_BLOCKCHAIN_ID=31337 \
CARTESI_SEQUENCER_APP_ADDRESS=0x1111111111111111111111111111111111111111 \
CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
cargo run -p wallet-sequencer -- setup

# run (keyed; reads identity from the set-up DB)
CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=http://127.0.0.1:8545 \
CARTESI_SEQUENCER_AUTH_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
cargo run -p wallet-sequencer -- run
```

## Always / Ask First / Never

### Always

- Keep inclusion-vs-rejection semantics explicit for transaction handling.
- Preserve API error shape and status code mapping unless intentionally changing the API contract.
- Add or update tests when logic changes.
- Run at least `cargo check` before finishing.
- Read `docs/recovery/` before touching recovery code, and `docs/threat-model/` before touching trust-boundary code.
- Check [`docs/invariants.md`](docs/invariants.md) before changing anything it lists as load-bearing, and [`docs/review/register.md`](docs/review/register.md) for open findings in the code you're about to touch and for decisions already settled or refuted.

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

- [`README.md`](README.md) — product framing, user-facing trust model, **API contract** (endpoint shapes, caps, close codes, health semantics).
- [`CLAUDE.md`](CLAUDE.md) — shell setup, quick reference, pointer back here.
- [`docs/protocol/`](docs/protocol/) — the authoritative protocol contracts: [`scheduler-semantics.md`](docs/protocol/scheduler-semantics.md) (the canonical acceptance algorithm, I1) and [`application-contract.md`](docs/protocol/application-contract.md) (the `Application` FFI trait contract).
- [`docs/invariants.md`](docs/invariants.md) — register of cross-module invariants (what's load-bearing across files) + the fail-loud check policy.
- [`docs/review/register.md`](docs/review/register.md) — the review register: open findings, settled decisions, and refuted proposals (do-not-re-propose), distilled from the dated ledgers beside it.
- [`docs/plans/`](docs/plans/) — the architecture decision record ([`2026-08-authority-boundary-adr.md`](docs/plans/2026-08-authority-boundary-adr.md)), active coordination tracks, and in-flight design handoffs.
- [`docs/threat-model/README.md`](docs/threat-model/README.md) — trust boundaries, in-scope and out-of-scope threats.
- [`docs/recovery/README.md`](docs/recovery/README.md) — recovery design, TLA+ formal verification, design history.
- [`docs/snapshots/`](docs/snapshots/) — app snapshots: [`format.md`](docs/snapshots/format.md) (dump trait + wire format) and [`lifecycle.md`](docs/snapshots/lifecycle.md) (take/promote/GC/lease design + crash-safety).
- [`docs/watchdog/operator-deployment.md`](docs/watchdog/operator-deployment.md) — production-like watchdog (Sepolia / mainnet; internal snapshot API).
- [`docs/watchdog/getting-started.md`](docs/watchdog/getting-started.md) — local dev: watchdog + `sequencer-devnet` on Anvil.
- [`docs/watchdog/README.md`](docs/watchdog/README.md) — watchdog architecture, compare vs advance modes, test commands.
- [`sequencer-core/`](sequencer-core/) — shared domain types and protocol contracts.
