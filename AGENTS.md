# AGENTS.md

Start here for the repository's mental model and working rules. Read this baseline
once, then use [Reading Routes](#reading-routes) to follow the contracts relevant
to the work. Those documents own detailed behavior; this guide explains why it
matters and where to look. Every fact has one home
([Documentation Practice](#documentation-practice)).

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
- **SQLite-centered local coordination.** Components publish and consume durable local facts through their owned SQLite tables, and the on-chain scheduler remains canonical authority. HTTP ingress ↔ inclusion lane is the one deliberate in-memory exception, not a precedent for a component bus ([ADR mechanism 4](docs/plans/2026-08-authority-boundary-adr.md)).
- **Append-oriented storage.** Avoid mutable status flags for open/closed entities; prefer write-once NULL→value transitions with one owner each, and derive cursors and heads from persisted facts rather than duplicating them as mutable fields.
- **Assumption-driven robustness.** Every hardening mechanism must name the invariant it protects, the assumptions under which it is needed, and a trigger for revisiting them. Machinery for failures outside the supported model enlarges the state surface developers must audit and can make the system less robust rather than more.
- **The complexity budget belongs to concurrency, mutual exclusion, durability, and hostile-L1 robustness.** This sequencer is not algorithmically complex. A large file is a smell: an invariant we're not seeing, a reasonable assumption we're not taking, or plain over-engineering. Judge every mechanism — current or proposed — against its weight.

## Sequencer / Scheduler Duality

The **scheduler** runs inside the rollup and **defines the canonical transaction
ordering**. For each batch in L1 safe inputs, it processes frames in order:
drain every pending direct input whose block number is ≤ `safe_block`, then
execute the frame's user ops. **It treats the sequencer as potentially
Byzantine** and enforces ordering and staleness regardless of what the
sequencer claims.

The **sequencer** knows that algorithm and **predicts** the canonical ordering,
issuing soft confirmations before its batches land. It has **write priority on
the execution queue** while it keeps advancing `safe_block` and submitting
batches. A frame's `safe_block` tells the scheduler how far to drain directs
before the frame's user ops, and commits the sequencer to having accounted for
every direct input up to that block. Under honest operation without outages,
soft confirmations match the canonical order; if the sequencer goes offline or
freezes `safe_block`, staleness forces recovery and invalidates them.

### Where the duality lives in code (change one, check all)

The precise acceptance algorithm — decode → sender → nonce → structural →
staleness → frame execution order → nonce advance — is **owned by
[`docs/protocol/scheduler-semantics.md`](docs/protocol/scheduler-semantics.md)**.
This section is the map.

Scheduler-acceptance semantics exist in exactly three implementations that must agree:

1. the canonical fold — `Scheduler<A>` ([`sequencer-core/src/scheduler/mod.rs`](sequencer-core/src/scheduler/mod.rs)), the same source compiled into the on-chain machine and driven bare-metal by the recovery fold;
2. the off-chain acceptance predicate — `ProtocolTiming::scheduler_accepts` ([`sequencer-core/src/protocol.rs`](sequencer-core/src/protocol.rs)), which feeds `safe_accepted_batches`;
3. the inclusion lane's live prediction (drain + execution order).

[I1](docs/invariants.md#i1-scheduler-acceptance-semantics-agree-across-all-implementations)
maps them and their one deliberate difference. Changing one requires checking
the others.

Two mechanical facts the agreement rests on:

- **Drain attribution.** Accumulated newly-safe directs land in the clock-advanced frame — frame K reads "directs ≤ S_K, then user ops validated on top", exactly the scheduler's drain-before-ops rule ([I2](docs/invariants.md)).
- **Empty batches are never stale and consume the nonce** (no first frame to measure staleness against). Consistent across all implementations, test-pinned.

## Batch Staleness and Recovery

A batch is **stale** when `inclusion_block - first_frame.safe_block >= MAX_WAIT_BLOCKS`
(1200 blocks, ~4h): the sequencer went offline, or kept submitting while
freezing `safe_block` to hold back directs. The scheduler skips a stale batch
without consuming its nonce, and overdue directs are force-drained anyway, which
bounds censorship at ~4h. Every later batch then arrives at an unexpected nonce,
so invalidation cascades through the suffix, open batch included
([scheduler semantics](docs/protocol/scheduler-semantics.md)).

- **Automatic recovery** repairs optimistic history after liveness failures: the
  danger detector stops the process at a threshold before staleness, and startup
  settles outstanding submissions and replaces the invalid suffix
  ([automatic recovery](docs/recovery/README.md)).
- **Cockroach recovery** (`setup --recovery`) rebuilds a fresh data directory from
  a trusted canonical checkpoint and L1 history, including after a sequencer bug
  ([cockroach recovery](docs/recovery/cockroach.md)).

## Threat Model (brief)

See [`docs/threat-model/README.md`](docs/threat-model/README.md) for the full model. Key points when reading or writing code:

- **Trusted:** InputBox contract, our own Ethereum node (fail-stop, not byzantine), operator config, batch-submitter key.
- **Adversarial:** `POST /tx` callers, direct-input senders, the L1 mempool and block builders (zombie transactions are a first-class threat).
- **RPC endpoint:** single (`CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`), trusted fail-stop, **must be one consistent node** — no fallback tier exists yet ([RPC endpoint](docs/threat-model/README.md#rpc-endpoint)).
- **Self-trust:** normal operation assumes the sequencer's own code is correct; a bug needs a fix and, when state cannot be trusted, cockroach recovery. Automatic recovery does not repair software defects.
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
- `sdk/rust-client/` — Rust client library for the sequencer API.
- `sdk/guest/` — canonical-guest SDK: `trolley` (guest rollup API over `libcmt-sys`), `rollups-types` (rollups ABI encodings), and `testsi` (host-side guest image tests); provenance in its README.
- `watchdog/` — Lua watchdog: re-derives canonical state from L1 and compares it with the sequencer's accepted checkpoint ([watchdog README](docs/watchdog/README.md)).
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
- **Tip** — the open batch at the end of the valid branch; recovery invalidates a suffix and opens a new Tip ([automatic recovery](docs/recovery/README.md#the-state-being-repaired)).
- **Inclusion lane** — the single ordering lane ([ADR mechanism 4](docs/plans/2026-08-authority-boundary-adr.md)); the only writer of open batch/frame state ([I17](docs/invariants.md)) and the system's execution bottleneck.
- **Soft confirmation** — sequencer's predicted ordering, emitted before the batch lands on L1.
- **Snapshot** — immutable artifact registered at every batch close; acceptance selects the comparison and recovery checkpoint ([snapshot lifecycle](docs/snapshots/lifecycle.md)).

## Domain Truths

- API validates the EIP-712 signature and enqueues a `SignedUserOp`. Method payload decoding happens during application execution, not at ingress.
- **Deposits are direct-input-only** (L1 → L2) and must not be represented as user ops.
- **InputBox payload classification** is by authenticated sender, never by a tag byte: the batch submitter's inputs are batches; any other sender's are direct inputs, stored verbatim ([scheduler semantics](docs/protocol/scheduler-semantics.md#the-one-stream-the-one-classification)).
- A rejected user op changes no state and is not persisted; the [application contract](docs/protocol/application-contract.md) owns which layer produces each rejection. The charge is a "fee", not "gas": it tracks DA.
- **The user-nonce rule is contract, not wallet detail** ([user nonces](docs/protocol/application-contract.md#user-nonces)); `GET /nonce` relies on it instead of asking the app.
- **EIP-712 domain:** both sides build it with `sequencer_core::build_input_domain` ([user-op signatures](docs/protocol/scheduler-semantics.md#user-op-signatures)); setup pins the chain id and app address, validating the chain id against RPC.

## Application Trait Contract

The [application contract](docs/protocol/application-contract.md) owns the `Application` trait; the [C binding guide](docs/protocol/c-application-binding.md) maps it to native engines. Two rules carry the duality: **any input that succeeded live must succeed on replay** (`AppError` is fatal and defines no canonical successor), and the inclusion lane, canonical scheduler, catch-up, and recovery fold execute inputs only through the shared boundary in `sequencer_core::application`, which checks the engine's reported progress.

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

Only the ingress routes (`POST /tx`, `GET /fee`, `GET /nonce`, `GET /domain`) are
public; every other route, snapshot and state downloads included, is internal,
unauthenticated, and **must not be exposed publicly**. The [README API contract](README.md#api)
owns routes, listeners, shapes, caps, and codes; [`openapi.yaml`](openapi.yaml)
renders it for client generation and documentation tools, so change both in the
same patch. CI lints it; run `direnv exec . redocly lint openapi.yaml` locally.

## Command Configuration

Plain `setup` is L1-read-only and never signs; `run` and `flush-mempool` take the
signing key and read the pinned identity from the database. [Running](README.md#running)
owns invocation and why the submitter address must be dedicated (the devnet uses
Anvil account 9). [`commands/config.rs`](sequencer/src/commands/config.rs) owns
every variable name, default, and validation; check it before documenting or
changing configuration.

## Coding Conventions

- Prefer small, composable functions at module boundaries (`ingress::api` → `ingress::inclusion_lane` → `storage::ingress`; `egress::l2_tx_feed` ← `storage::history`).
- Keep application validation and execution deterministic for a given input/state. No `SystemTime::now()`, `HashMap` iteration order, or floating-point in consensus paths.
- Surface user-facing errors via `ApiError` (in `http.rs`); keep internal failures descriptive but safe. An error that wraps a cause ends its Display with `: {source}`, because log sinks print Display only. A dependency error whose Display omits its cause (reqwest names only the request kind) is rendered with `l1::provider::error_chain` where it becomes text, in a `String` variant or a log field; it appends only causes the line does not already hold, so it is also safe on our own wrappers of such an error. A failure logs at its verdict's level: ERROR when the cause is a fault or unknown (terminal, unclassified), WARN when the verdict names a planned procedure (a recovery boot, a retry, `setup --recovery`).
- Avoid introducing heavy dependencies without strong reason.
- **Impossible states fail loud; they are never handled or absorbed.** The [check policy](docs/invariants.md#the-check-policy-fail-loud) owns the decision test and its reasons; [Self-trust](docs/threat-model/README.md#self-trust) owns the trust boundary.

## Documentation Practice

Write for a reader building a mental model. Start with the purpose, input,
result, and governing constraint; introduce implementation detail when it
explains a necessary behavior. Keep an intentional baseline here so important
concepts are discoverable before anyone knows to ask about them.

- **One fact, one home.** Elsewhere, name the fact in a clause and link its owner; never restate its conditions, orderings, or lists.
- **Surprising facts go into the owning doc**, so the next reader does not rediscover them.
- **Size caps.** A module header (Rust `//!`, Lua `---`) is 1–4 lines and a rustdoc contract about 6; public methods get docs only when name and signature leave the contract unclear. Longer reasoning goes to the owning doc or invariant, linked.
- **Comment the non-obvious, not the self-evident.** Explain the why, edge cases, invariants, and subtle behavior the code cannot show; do not restate what it expresses.
- **State a non-guarantee once**, at its owner, and only where a reader would otherwise assume the guarantee.
- **Table cells hold one sentence.**
- **Record deliberate absence once**, at the seam where someone would re-add the mechanism, phrased as a positive design statement with its reason. Avoid removal notices scattered across documents.
- **No review codenames** (finding or decision ids from past review ledgers) in code comments or living docs: state the reason, or point at the invariant register entry that owns it. Invariant ids (`I1`–`I20`) are stable references and are fine.

Keep three kinds of material distinct:

- **Current contracts and designs** describe what holds now and why, in present
  tense, without amendment banners or "previously/no longer" narration. Some
  current architecture documents live in `docs/plans/`; the directory name does
  not make their established contracts optional.
- **Active plans** name open decisions, dependencies, and remaining work, and
  distinguish proposed behavior from implemented contracts. On completion, put
  the durable design in its owner and reduce the plan to remaining work and links.
- **Review notes** are temporary working memory. The [review lifecycle](docs/review/README.md)
  owns where their conclusions go: unresolved work to the [register](docs/review/register.md),
  durable reasoning to its owner, completed history to Git.

## Testing Guidance

Concentrate tests where consensus risk lives: signature and sender validation,
nonce progression, fee and rejection behavior, included-vs-rejected commits, and
storage batch atomicity and uniqueness. Any invariant the scheduler and
sequencer share gets at least one test that exercises both sides. Prefer
black-box tests around `POST /tx` and commit outcomes for integration.

Test placement: unit tests are a child module of the code under test, inline
`#[cfg(test)] mod tests` or `<module>/tests.rs` (or `<topic>_tests.rs`) once
large, never `#[path]`. Cross-module in-process tests go in
`sequencer/src/integration_tests/`; devnet, machine and binary tests go in the
`tests/*` crates. Shared fixtures live in `crate::test_support`;
`storage::test_helpers` keeps the storage-domain ones (seeding, the recovery
fixture trait, divergence markers); cross-crate signing helpers live in
`sequencer_core::test_support` behind its `test-support` feature.

Some `sequencer` tests use Anvil (Foundry). They run by default and fail with a
clear message if `anvil` is not on PATH. Use the configured Nix/direnv environment
or install Foundry. `canonical-test` additionally needs the Cartesi Machine
library named by `LIBCARTESI_PATH`/`INCLUDECARTESI_PATH` (the devshell exports
both) and libslirp.

## Shell and Commands

Use the configured Nix/direnv environment for Foundry, TLA+, and other project
tools. For noninteractive commands, prefer `direnv exec . <command>`. Rust is
pinned in [`rust-toolchain.toml`](rust-toolchain.toml), the only Rust pin (CI and
release install from it); verify both `cargo --version` and `rustc --version` in
the environment you use. A Nix-provided
Cargo or rustc may bypass rustup, so direnv alone does not guarantee the pinned
compiler is selected. Correct the toolchain selection before interpreting build
or dependency errors. `cross` builds the guest with the channel's
`x86_64-unknown-linux-gnu` toolchain on every host; the guest recipes install
it, with the riscv64 target and `rust-src`, before each build
(`just canonical cross-toolchain`).

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
- Check [`docs/invariants.md`](docs/invariants.md) before changing anything it lists as load-bearing (storage changes cross writer boundaries even when the SQL looks local), the owning design for its assumptions, and [`docs/review/register.md`](docs/review/register.md) for unresolved work in the code you're about to touch. Verify review claims against current code.

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
| API, subscriber replay, or application-history coordinates | [README API contract](README.md#api) — wire behavior, with [`openapi.yaml`](openapi.yaml) changed alongside it; [application history](docs/protocol/application-history.md) — era, generation, offsets, and recovery boundaries. |
| Snapshots, restart, export, retention, or watchdog checkpoints | [Snapshot lifecycle](docs/snapshots/lifecycle.md) — durable publication, accepted comparison points, leases, and GC; [wallet format](docs/snapshots/format.md) when changing wallet bytes. |
| Trust boundaries, provider behavior, or hostile L1 input | [Threat model](docs/threat-model/README.md) — actor assumptions, supported failures, and residual risks. |
| Submission fees or oracle pricing | [L1 fee policy](docs/l1-fee-policy.md) — estimation and replacement limits; [threat model's fee oracle section](docs/threat-model/README.md#fee-oracle) — oracle source and outage assumptions. |
| Command setup or deployment configuration | [Running](README.md#running) and [config.rs](sequencer/src/commands/config.rs) — invocation, identity pinning, defaults, and validation. |
| Watchdog development or operation | [Watchdog README](docs/watchdog/README.md) — contract, state sources, commands, and state; [incident runbook](docs/watchdog/incident-runbook.md) — what a latched divergence requires; [local dev](docs/watchdog/getting-started.md) for Anvil; [operator deployment](docs/watchdog/operator-deployment.md) for Sepolia/mainnet. |
| Canonical guest, machine images, or a Cartesi Machine bump | [Cartesi Machine facts](docs/cartesi-machine.md) — version pinning, host semantics, storage, memory ranges, and the guest side. |
| A release, or adding or bumping a GitHub Action | [`release.yml`](.github/workflows/release.yml) header — what to run before tagging; [`verify-action-pins.sh`](scripts/verify-action-pins.sh) — pin form, upstream audit, and why bumps are manual. |
| A new mechanism, simplification, or work spanning an active track | Owning design and [invariants](docs/invariants.md) — reasons and assumptions; [review register](docs/review/register.md) — unresolved work; [coordination tracks](docs/plans/2026-07-coordination-tracks.md) — priorities and dependencies. |
