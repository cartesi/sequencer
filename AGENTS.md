# AGENTS.md

Start here for the repository's mental model and working rules. Read this baseline
once, then use [Reading Routes](#reading-routes) to follow the contracts relevant
to the work. Those documents own detailed behavior; this guide explains why it
matters and where to look.

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

The submitter's expected-nonce scan also depends on this agreement. The
[scheduler contract](docs/protocol/scheduler-semantics.md#the-three-implementations-and-why-they-agree)
maps the implementations and the deliberate storage-local copy. Changing one
requires checking the others.

Two mechanical facts the agreement rests on:

- **Drain attribution.** Accumulated newly-safe directs land in the clock-advanced frame — frame K reads "directs ≤ S_K, then user ops validated on top", exactly the scheduler's drain-before-ops rule ([I2](docs/invariants.md)).
- **Empty batches are never stale and consume the nonce** (no first frame to measure staleness against). Consistent across all implementations, test-pinned.

## Batch Staleness and Recovery

### Staleness

A batch is **stale** when `inclusion_block - first_frame.safe_block >= MAX_WAIT_BLOCKS` (1200 blocks, ~4h). Staleness catches two failure modes:

1. **Liveness failure** — the sequencer went offline and failed to submit batches in time.
2. **Censorship** — the sequencer kept submitting batches but froze `safe_block` to hold back direct inputs.

When the scheduler encounters a stale batch, it skips its frames without
consuming the batch nonce. The overdue-direct backstop still runs. Together,
these rules prevent the sequencer from holding write priority indefinitely
without advancing the drain cursor; direct inputs are force-drained at
`MAX_WAIT_BLOCKS`, giving the ~4h censorship-resistance bound.

### Cascading invalidation

If a batch is stale, all existing subsequent batches are also invalid. The scheduler's expected-nonce counter does not advance on a stale skip, so every subsequent batch arrives at an unexpected nonce and is rejected. Invalidation is a suffix operation: marking batch `N` invalid cascades to `N+1`, `N+2`, …, including the open batch. New batches created after recovery are unaffected.

### Two recovery paths

**Automatic recovery** repairs optimistic history after liveness failures. The
danger detector signals service to stop when a danger check fires; startup recovery
settles outstanding submissions and replaces the invalid suffix using local
SQLite facts and safe L1 history. The danger threshold is a trigger, not proof
that a batch is doomed. Detection uses safe state, with wall-clock checks when
the L1 view stops advancing. Expected recovery exits gracefully; diagnosed
terminal runtime faults abort immediately. The [automatic recovery design](docs/recovery/README.md)
owns detection, startup ordering, dispatch, and admission.

**Manual cockroach recovery** (`setup --recovery`) rebuilds from a trusted
canonical application checkpoint and L1 history into a fresh data directory.
It also applies after sequencer bugs: fix the bug, choose a trusted canonical
checkpoint, and then run recovery. Historical batches,
including malformed ones, receive the canonical scheduler's treatment. The
result accounts for a fixed input prefix from which sequencing can resume; it
does not need to catch the moving L1 tip or preserve prior soft confirmations.
The [cockroach recovery guide](docs/recovery/cockroach.md) owns the checkpoint
requirements, stopping boundary, and rebuild procedure.

Before changing recovery code, read its guide and both current bounded TLA+
models. The automatic recovery guide's [Formal Verification](docs/recovery/README.md#formal-verification)
section explains their scopes; the models are not a proof of every recovery path.

## Threat Model (brief)

See [`docs/threat-model/README.md`](docs/threat-model/README.md) for the full model. Key points when reading or writing code:

- **Trusted:** InputBox contract, our own Ethereum node (fail-stop, not byzantine), operator config, batch-submitter key.
- **Adversarial:** `POST /tx` callers, direct-input senders, the L1 mempool and block builders (zombie transactions are a first-class threat).
- **RPC endpoint:** single (`CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`), trusted fail-stop, **must be one consistent node** — no fallback tier exists yet (see the threat model's actor table).
- **Self-trust:** normal operation assumes the sequencer's own code is correct.
  Invariant violations fail loud. Bug-induced malformed batches require fixing
  the bug and, when rebuilding is necessary, manual cockroach recovery; automatic
  recovery does not repair software defects.
- **In scope:** correctness bugs *and* exploitation. Under rollup semantics, a correctness bug that causes scheduler/sequencer state divergence is as severe as direct theft.

## Architecture Map

Top-level layout follows the system's data flow. Each sequencer module corresponds to a writer role; the matching `storage/<role>.rs` holds its storage half.

The implementation uses Rust edition 2024, Axum, SQLite (rusqlite/WAL), EIP-712
signing, and SSZ batch encoding.

### Workspace

- `sequencer/` — sequencer **library** (no binary). App crates compose it into a binary.
- `sequencer-core/` — shared domain types (`Application`, `SignedUserOp`, `SequencedL2Tx`, `Batch`, `Frame`).
- `examples/app-core/` — placeholder wallet app implementing the `Application` trait.
- `examples/wallet-sequencer/` — binary crate: wallet app + sequencer library. The model for what an app author builds (their `Application` impl ≙ `app-core`; their binary crate ≙ this).
- `bindings/c-app-engine/` — reusable C ABI adapter implementing `Application` for a native engine.
- `bindings/c-app-sequencer/` — optional C-engine CLI host and external-archive binary.
- `examples/c-wallet-engine/` — reference wallet engine exporting the C ABI, plus its genesis tool and conformance tests.
- `examples/c-wallet-sequencer/` — binary composing the C-engine host with the reference wallet engine.
- `examples/canonical-app/` — on-chain scheduler reference implementation.
- `examples/canonical-test/` — e2e test harness for the canonical app.
- `cartesi-tools/` — vendored guest and guest-test tooling (`libcmt-sys`, `trolley`, `testsi`, `types`); provenance in its README.
- `sdk/rust-client/` — Rust client library for the sequencer API.
- `tests/{benchmarks,e2e,harness}/` — test infrastructure.

### Sequencer module layout

Paths below are relative to `sequencer/src/`:

- `lib.rs` and `harness.rs` — public API and shared CLI harness; app binaries supply their genesis-app constructor.
- `commands/` — `setup`, `run` (including worker supervision), and `flush`, with command configuration and exit-code classification.
- `runtime/` — exclusive process ownership and runtime scope/shutdown.
- `ingress/` — public HTTP handlers and the single inclusion lane.
- `egress/` — internal HTTP/WS API and DB-backed application-input feed.
- `l1/` — safe-input reader, batch submitter, fee oracle, shared EIP-1559 estimation, and provider access.
- `recovery/` — automatic startup repair, danger detector, and mempool flusher; the manual rebuild lives in `commands/setup/`.
- `storage/` — each writer role's persistence operations, shared queries, and schema.
- `http.rs` and `clock.rs` — shared HTTP errors/server setup and the crate-wide wall clock. `L1Config` lives in `l1/`.

## Key Concepts

- **Chunk** — bounded list of user ops processed and persisted together to amortize SQLite cost.
- **Frame** — ordering boundary; commits `safe_block` + user ops.
- **Batch** — list of frames posted on-chain as one L1 transaction (SSZ-encoded).
- **Inclusion lane** — the single ordering lane, with a latency-critical user-op regime and a slower L1-reconciliation regime ([ADR mechanism 4](docs/plans/2026-08-authority-boundary-adr.md)); the only writer of open batch/frame state ([I17](docs/invariants.md)) and the system's execution bottleneck.
- **Batch submitter** — stateless worker that bulk-submits all pending batches each tick. Storage assigns each batch's scheduler nonce at creation (`parent.nonce + 1`, or the deployment anchor for a root); the submitter reads it and selects L1 wallet nonces for submission.
- **Danger detector** — polls `Storage::check_danger` and signals the process to stop so startup can recover or refuse. It reads local facts; it never writes the DB or talks to L1.
- **Fee oracle** — setup pins and bootstraps a fixed price or Uniswap V3 TWAP source. The price informs future frame fees; an oracle-only outage is an accepted economic risk. The [threat model's actor table](docs/threat-model/README.md#actors-and-trust) owns the source assumptions and failure policy.
- **Input reader** — ingests safe inputs from L1 InputBox and maintains the durable safe head, accepted-batch projection, and divergence marker in one atomic transaction (`sequencer/src/storage/l1_inputs.rs`); it hands the lane no in-memory cursor.
- **L2 tx feed** — DB-backed application-input stream. HTTP snapshot headers
  provide `(EraId, RecoveryGeneration, ExecutedInputCount)`; WS validates that
  claim and replays inclusively before following the tip.
- **Application progress** — engine-owned `(ExecutedInputCount,
  last_executed_safe_block)`, embedded in every dump. Shared execution verifies
  the transition and returns the pre-execution offset; storage commits that
  mandatory offset with its source in `application_inputs`.
- **History version** — `(EraId, RecoveryGeneration)`. Setup publishes a complete
  baseline with a fresh era; recovery increments the generation exactly once
  iff it invalidates at least one valid batch and records the preserved-prefix
  cut in the same transaction. `/history` can check a saved checkpoint across
  intervening generations; subscription claims still enforce both identifiers.
  The [history contract](docs/protocol/application-history.md) owns compatibility.
- **Soft confirmation** — sequencer's predicted ordering, emitted before the batch lands on L1.
- **Snapshot** — immutable artifact at every batch close, registered with its
  local batch identity and application count. Acceptance facts select the
  recovery/watchdog checkpoint; the era baseline supplies the initial restore
  point. Lifecycle and leases: [`docs/snapshots/lifecycle.md`](docs/snapshots/lifecycle.md).

## Domain Truths

- API validates the EIP-712 signature and enqueues a `SignedUserOp`. Method payload decoding happens during application execution, not at ingress.
- **Deposits are direct-input-only** (L1 → L2) and must not be represented as user ops.
- Rejections (`InvalidNonce`, `InvalidMaxFee`, `InsufficientFeeBalance`) produce no state mutation and are not persisted. These are protocol-level rejection semantics every app must implement: nonces prevent user-op replay, fees prevent spam against the sequencer's DA budget. ("Fee", not "gas" — the fee tracks DA; compute metering, if it ever exists, is a separate future concept.)
- Included txs are persisted as frame/batch data in `batches`, `frames`, `user_ops`, `safe_inputs`, and `application_inputs`. Recovery metadata lives in `safe_accepted_batches`; batch lifecycle state (sealed/invalidated) lives on the `batches` row itself as write-once timestamps.
- Frame fee is persisted in `frames.fee` and is fixed for the lifetime of that frame. The next frame's fee is currently sampled from `batch_policy_derived.recommended_fee` at rotation; oracle bootstrap writes the price before any Tip can sample it, and `log_slack` applies the 10× margin in log space. This is present behavior, not a reason for the five-block clock policy; hoisting fee to the batch is a later design with its own trade-offs.
- Wallet balances and nonces live in memory between checkpoints; restart restores
  a dump and replays persisted application inputs.
- **EIP-712 domain fields:** `name`, `version`, `chainId`, `verifyingContract`.
  Setup pins the chain id and app address from `CARTESI_SEQUENCER_BLOCKCHAIN_ID`
  and `CARTESI_SEQUENCER_APP_ADDRESS`, validating the chain id against RPC. All
  four fields must be present on both sides; the sequencer and canonical
  scheduler share `sequencer_core::build_input_domain`.

### InputBox payload classification

- The input reader ingests every `InputAdded` event from InputBox. Each event carries an authenticated `msg_sender` (delivered by the Cartesi framework from `EvmAdvanceCall`).
- **Classification is by sender address**, not by a tag byte:
  - Sender == batch-submitter address → SSZ-decoded as `Batch` (scheduler side). The sequencer does not ingest its own batch submissions as direct inputs.
  - Any other sender → stored verbatim as a direct input (deposit).
- The payload is opaque to the classification layer. Application-specific decoding happens inside `Application::apply_direct_input`, reached only through the shared `execute_direct_input` boundary.

## Application Trait Contract

Implementors of the `Application` trait must respect these contracts. The shared execution boundary verifies application-owned count/clock progress; application-specific determinism and mutation remain self-trusted. The full, code-grounded contract — method table, dump round-trip durability, the safe-block clock — is **owned by [`docs/protocol/application-contract.md`](docs/protocol/application-contract.md)**; the essentials follow.

### Replay determinism

The sequencer persists every included user op and every ingested direct input. On restart, catch-up replays them in order against a fresh `Application` instance to rebuild state. **Any input that succeeded live must succeed on replay.**

- `apply_direct_input` and `apply_valid_user_op` must not return `AppError::Internal` for any byte sequence that previously executed successfully. The canonical scheduler, catch-up, and recovery fold treat `Internal` as fatal: no canonical successor is defined.
- Validation returns `Accept`, `Reject(InvalidReason)`, or fatal `AppError`. A rejected op changes no state. Included business failures and malformed direct-input no-ops still advance progress; do not turn them into validation rejections. See the application contract for the wallet's nonce/fee semantics.
- `validate_user_op` must be pure over the current app state. No side effects, no time dependence, no randomness.

### No implicit state

Logical state changes, including `ApplicationProgress`, flow through the `apply_valid_user_op` and `apply_direct_input` hooks. The engine reports progress by value. Mutating state from `validate_user_op` breaks replay determinism; mutable checkpoint creation may replace backing resources while preserving logical state.

### One execution entry point

User ops are executed only through `sequencer_core::application::validate_and_execute_user_op`; already-validated user ops and directs use `execute_valid_user_op` / `execute_direct_input`. The shared boundary preflights the checked successor, then verifies the engine's progress after a successful hook and returns its pre-execution offset. Count zero implies clock zero. Validation purity and native mutation remain self-trusted. `AppError` is fatal and defines no canonical successor; callers discard the instance rather than resume it. The inclusion lane, canonical scheduler, catch-up, and recovery fold all use this boundary — part of the duality agreement.

`Application` requires `Send`, with neither `Clone` nor `Sync`. Dumps must be durable and immutable, and restored engines must remain independent after source deletion. The opaque app prefix may be a file or directory; checkpoint disposal uses ordinary recursive filesystem deletion. Canonical inspection belongs to the separate `CanonicalState` trait; the native sequencer serves the comparison file in the checkpoint. The [C binding guide](docs/protocol/c-application-binding.md) maps the contract to native engines.

## Ordering and Storage

Preserve single-lane deterministic ordering. Do not introduce extra concurrency
in hot-path ordering logic without explicit approval.

The inclusion lane combines a latency-critical user-op regime with complete L1
reconciliation. The [authority-boundary ADR](docs/plans/2026-08-authority-boundary-adr.md)
owns that split, acknowledgement rules, runtime ownership, and command admission.
The [scheduler contract](docs/protocol/scheduler-semantics.md#sequencer-frame-clock-policy)
owns the five-safe-block frame clock; the [Application contract](docs/protocol/application-contract.md#5-operational-capacity-for-l1-reconciliation)
owns the assumption that accumulated directs are digestible without preemption.

Storage changes cross writer boundaries even when the SQL looks local. Read the
[invariant register](docs/invariants.md) for writer ownership, `valid_*` reads,
drain attribution, the content-identity/divergence freeze, `WriteHead` coherence,
and application-history offsets. The [schema](sequencer/src/storage/migrations/0001_schema.sql)
enforces write-once batch lifecycle, Tip uniqueness, and user-op identity.

## Type Boundaries

- `SignedUserOp` — ingress/API signature domain (post-validation, pre-execution).
- `ValidUserOp` — application execution domain (after validation boundary).
- `SequencedL2Tx` — application input payload sum (`UserOp | DirectInput`).
- `ExecutedInputCount` — canonical application-history boundary (`X` means the
  next input is entry `X`), never a SQLite cursor. Checked arithmetic only.
- `ApplicationInputRow` — crate-private pairing of a mandatory application offset
  and source context. Every row executes; batch envelopes stay in `safe_inputs`.
- Keep DB-only helper types private to storage modules; prefer shared domain types at module boundaries.

## HTTP Endpoints

- **Ingress** (public-facing): `POST /tx`, `GET /fee`.
- **Egress** (internal indexers/watchdog): application-input subscriptions,
  snapshot/state downloads, and health probes. Snapshot/state endpoints have no
  authentication and **must not be exposed publicly**. Downloads hold a GC lease
  for their response lifetime ([snapshot lifecycle](docs/snapshots/lifecycle.md)).

Today both sides serve from one listener; the planned API split puts each side on its own port (same binary) so internal probes and subscribers can be firewalled from public submit traffic.

The [README API contract](README.md#api) owns routes, message shapes, caps,
close codes, and health semantics.

## Command Configuration

Configuration follows the command phases:

- Plain **`setup`** is L1-read-only and never signs. It pins chain/app/submitter
  identity and the fee source. **`setup --recovery`** also needs the submitter
  key to flush its outstanding transactions; see the [rebuild guide](docs/recovery/cockroach.md).
- **`run`** takes the RPC endpoint and signing key (or key file). It reads the
  pinned chain id, app address, and submitter address from the database.

**Use a dedicated submitter address.** Plain setup refuses an unsettled wallet
nonce, so sharing a busy contract-deployer address can trip the detection gate
while its deployment transactions are not yet safe. The devnet uses Anvil
account 9 (`DEVNET_SEQUENCER_ADDRESS`), separate from the account-0 deployer.

The [Running guide](README.md#running) gives invocation examples;
[`commands/config.rs`](sequencer/src/commands/config.rs) owns environment-variable
names, defaults, and validation. Check configuration there before documenting or
changing it, including checkpoint and fee-source selection.

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

Write for a reader building a mental model. Start with the purpose, input,
result, and governing constraint; introduce implementation detail when it
explains a necessary behavior. Keep an intentional baseline here so important
concepts are discoverable before anyone knows to ask about them. Summaries
should point to the owner of a contract rather than becoming a second copy.

Keep three kinds of material distinct:

- **Current contracts and designs** describe what holds now and why. Use present
  tense and reasoning inline, without amendment banners, review codenames, or
  "previously/no longer" narration. Each topic has one owner. Some current
  architecture documents live in `docs/plans/`; the directory name does not
  make their established contracts optional.
- **Active plans** name open decisions, dependencies, and remaining work. They
  must distinguish proposed behavior from implemented contracts. On completion,
  put the durable design in its owner and reduce the plan to its remaining work
  and links.
- **Review notes** are temporary working memory. Commit them when they help an
  active review or handoff, then distill and delete them when that work ends.
  The [review lifecycle](docs/review/README.md) owns the policy: unresolved work
  stays in one register or active plan, durable reasoning in its current owner,
  completed history in Git. Keep dated evidence only for a named ongoing use.

**Record deliberate absence once**, at the seam where someone would re-add the
mechanism, phrased as a positive design statement with its reason. Avoid removal
notices scattered across documents.

## Testing Guidance

Focus tests on:

- Signature + sender-validation edge cases.
- Nonce progression rules.
- Fee and rejection behavior.
- Included-vs-rejected commit behavior.
- Storage batch atomicity and uniqueness constraints.
- Scheduler/sequencer agreement — any invariant the two sides share should have at least one test that exercises both.

Prefer black-box tests around `POST /tx` and commit outcomes for integration.

Some `sequencer` tests use Anvil (Foundry). They run by default and fail with a
clear message if `anvil` is not on PATH. Use the configured Nix/direnv environment
or install Foundry. `canonical-test` additionally needs the Cartesi Machine
library named by `LIBCARTESI_PATH`/`INCLUDECARTESI_PATH` (the devshell exports
both) and libslirp.

## Shell and Commands

Use the configured Nix/direnv environment for Foundry, TLA+, and other project
tools. For noninteractive commands, prefer `direnv exec . <command>`. Rust is
pinned to **1.95.0** in [`rust-toolchain.toml`](rust-toolchain.toml); verify both
`cargo --version` and `rustc --version` in the environment you use. A Nix-provided
Cargo or rustc may bypass rustup, so direnv alone does not guarantee the pinned
compiler is selected. Correct the toolchain selection before interpreting build
or dependency errors.

```bash
direnv exec . cargo check
direnv exec . cargo test --workspace --exclude canonical-test
direnv exec . cargo test -p sequencer --lib  # includes Anvil-backed tests
direnv exec . cargo fmt --all
direnv exec . cargo clippy --all-targets --all-features -- -D warnings
```

The shared command harness is in [`sequencer/src/harness.rs`](sequencer/src/harness.rs).
See [Running](README.md#running) for the two-phase `setup` / `run` workflow.

## Always / Ask First / Never

### Always

- Keep inclusion-vs-rejection semantics explicit for transaction handling.
- Preserve API error shape and status code mapping unless intentionally changing the API contract.
- Add or update tests when logic changes.
- Run at least `cargo check` before finishing.
- Read the relevant recovery guide and both current TLA+ models before touching
  recovery code, and the threat model before touching trust-boundary code.
- Check [`docs/invariants.md`](docs/invariants.md) before changing anything it lists as load-bearing, the owning design for its assumptions, and [`docs/review/register.md`](docs/review/register.md) for unresolved work in the code you're about to touch. Verify review claims against current code.

### Ask First

- Changing tx wire format (`UserOp`, SSZ payload layout, EIP-712 domain fields).
- Changing DB schema or migration strategy.
- Altering rejection semantics (what consumes nonce/fee vs what is rejected).
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

## Reading Routes

Follow the rows that intersect the change. Each destination explains the
cross-module consequences to check before editing; follow its links when the
work reaches another boundary.

| Work | Read first and why |
|---|---|
| Scheduler acceptance, batch nonces, direct-input ordering, or frame clock | [Scheduler semantics](docs/protocol/scheduler-semantics.md) — canonical algorithm and the implementations that must agree. |
| Inclusion lane, storage writes, or runtime concurrency | [Invariant register](docs/invariants.md) — enforcement and consumers; [authority-boundary ADR](docs/plans/2026-08-authority-boundary-adr.md) — ownership, acknowledgement, admission, and terminal stop. |
| Application implementation, execution, or native integration | [Application contract](docs/protocol/application-contract.md) — determinism, progress, failure, capacity, and checkpoints; [C binding](docs/protocol/c-application-binding.md) for native engines. |
| Automatic recovery or danger detection | [Automatic recovery](docs/recovery/README.md), then [preemptive.tla](docs/recovery/preemptive.tla) and [admission.tla](docs/recovery/admission.tla) — repair ordering and the models' bounded guarantees. |
| Manual rebuild after lost state or a sequencer bug | [Cockroach recovery](docs/recovery/cockroach.md) — trusted checkpoint, fixed input boundary, and fresh baseline. |
| API, subscriber replay, or application-history coordinates | [README API contract](README.md#api) — wire behavior; [application history](docs/protocol/application-history.md) — era, generation, offsets, and recovery boundaries. |
| Snapshots, restart, export, retention, or watchdog checkpoints | [Snapshot lifecycle](docs/snapshots/lifecycle.md) — durable publication, accepted comparison points, leases, and GC; [wallet format](docs/snapshots/format.md) when changing wallet bytes. |
| Trust boundaries, provider behavior, or hostile L1 input | [Threat model](docs/threat-model/README.md) — actor assumptions, supported failures, and residual risks. |
| Submission fees or oracle pricing | [L1 fee policy](docs/l1-fee-policy.md) — estimation and replacement limits; [threat-model actor table](docs/threat-model/README.md#actors-and-trust) — oracle source and outage assumptions. |
| Command setup or deployment configuration | [Running](README.md#running) and [config.rs](sequencer/src/commands/config.rs) — invocation, identity pinning, defaults, and validation. |
| Watchdog development or operation | [Architecture](docs/watchdog/README.md); [local dev](docs/watchdog/getting-started.md) for Anvil; [operator deployment](docs/watchdog/operator-deployment.md) for Sepolia/mainnet. |
| A new mechanism, simplification, or work spanning an active track | Owning design and [invariants](docs/invariants.md) — reasons and assumptions; [review register](docs/review/register.md) — unresolved work; [coordination tracks](docs/plans/2026-07-coordination-tracks.md) — priorities and dependencies. Follow the [review lifecycle](docs/review/README.md) when recording conclusions. |
