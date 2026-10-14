# Whole-repo review, 2026-10-13

Read-only review of `1a9615d` (= `origin/main`) through five lenses:
over-engineering, abstractions, correctness, simplification, and taste. The
maintainer's backlog notes were triaged against the code, and the three consumer
surfaces were judged from the consumer's seat: the `Application` trait and C ABI,
the scheduler spec, and the egress API.

Method: 14 area reviewers, one adversarial skeptic per area, independent traces
for medium- and high-severity correctness claims, a future-direction mapper,
and a synthesizer. No builds or tests were run. Of 182 findings, 69 were
confirmed, 68 partially confirmed, 5 refuted and 4 disputed. The orchestrator
re-checked K1, BS1, CS1, L2, LS1, N6, N9, N10, N13, N14 and FEE1 in the code.
Every finding id resolves in the [findings appendix](2026-10-13-whole-repo-findings.md).

Retire both files when the waves below land or are re-planned. Open items move
to the [register](register.md).

## Verdict

- **Consensus core is sound.** No high-severity bug turned up in gate order,
  nonce handling, the drain, the fold or the divergence freeze.
- **Risks sit at boundaries, not in algorithms.**
  - The guest's compiled sequencer address is never checked against the pinned submitter (BS1).
  - Signature acceptance is defined by alloy, not by the spec (B1).
  - The fee floor overcharges at sub-gwei gas (CS1).
  - 43 tests pass without running (K1).
  - A release dispatch can mislabel artifacts (L2), and the guest kernel's hash is not pinned in-tree (LS1).
- **Over-engineering is local, not structural.**
  - Durable leases implement a process-local refcount (I2).
  - Fee inverses in the consensus crate have only test callers (C3).
  - Benchmark verdict code can never evaluate (K6).
  - The poster has a redundant shutdown select (F4).
  - The hostile-L1, durability and concurrency machinery is proportionate.
- **Large files are mostly tests.** Copied fixtures (K2–K4, about 1k lines) and
  duplicated benchmark tooling (K6/K7, about 0.8k) are the main harvest outside
  superseded code.
- **Diagnostics are the weakest lens.**
  - Root causes drop out of exit and abort lines (H1).
  - The RPC URL leaks on both the Rust and Lua sides (register, M1, G10).
  - Retryable conditions log at ERROR on every tick (H3, F2).
- **Docs run 1.5–2× their content.** Facts repeat in 3–7 places, 37 of 80
  module headers break the 1–4 line rule, and AGENTS.md costs about 4.9k words
  per session.
- **The future direction supersedes about 7k lines:** projection replay, the
  WS/replica bootstrap and `setup --recovery`. Freeze them; most polish findings
  there are traps.

## Plan of attack

Progress: W1, W2, W3a and W3b have landed on `claude/repo-code-review-103cc5`.
Each was followed by a devnet and watchdog e2e run and an adversarial review
whose surviving findings were folded into their commits. W3a left the guest
binary byte-identical. W3b moved the template hash once, with behavior pinned by
consensus vectors; its hashes are in the
[guest image record](2026-10-14-guest-image-refresh.md). The input-reader review's commits (compared against the sling
reader, branch `claude/input-reader-review-fda7ad`) are folded in on top, adapted
to this branch:

| Their commit | Here |
|---|---|
| `2bfac83` | `fix(l1): split a failed eth_getLogs range on any error` |
| `15f7d0a` | `fix(l1): resend rate limits briefly and never resend log caps` |
| (new) | `test(l1): pin log order and refusal on both sides` |
| `83b9166`, plus `4532b33`'s redirect refusal | `fix(l1): keep the RPC URL out of errors and logs, keep the cause` |
| `1d3bf98` | dropped: `http.origin` leaked text after an `@` in a path or query, and the watchdog already names no URL |
| `19b667c` | `fix(setup): pin the deployment block only once it is safe` |
| `ad80cab` | `fix(l1): cross-check the input payload's chain and app; sharper reader errors` |
| `e626e96` | `docs: record the reader's L1 assumptions and bounded-ingestion design` |
| `e959fc0` | `feat(storage): make safe_inputs append-only` |
| `4532b33` | `feat(l1): separate read and submit RPC providers; pin HTTP/1.1` |

A log missing its block, transaction or log index is refused on both sides, and
the shared log-sort vector pins it for an absent or null field. A malformed
position is split like a transport failure and reported as `Provider` in Rust,
but refused as transient in Lua; both retry.

Rules for every wave: behavior changes and refactors ship as separate patches,
frozen code (see [Freeze list](#freeze-list)) gets bug fixes only, and
wire/schema/ABI changes and guest behavior changes wait for the contract wave. A
guest-image move that preserves behavior may land earlier when the consensus
vectors pass unchanged (W3b).

### Step 0: decisions (settled 2026-10-13)

1. **Bart: done.** His client treats cockroach recoveries as absent, and it can
   keep doing so once the tool exists. The [Track 3 plan](../plans/2026-07-track3-feed-replay-design.md#follow-up-sequence)
   and the [tracks board](../plans/2026-07-coordination-tracks.md) still describe
   him integrating scheduler replay; correct that text in W1.
2. **Fees stay in log space.** The batch-level fee is deferred together with
   batch-scoped replay protection, which would enable it.
   - The oracle-side numerics become sequencer-local work: C2, then CS1, in W2.
     CS1 relaxes a schema CHECK; approved.
   - Track 5 (the fee table) is unblocked but consensus-relevant, so it moves to
     W5 with Bart, taking L12 (`build.rs` → checked-in table) with it.
   - The NUM note's batch-fee half stays deferred, recorded in
     [batch-scoped replay protection](../plans/2026-10-batch-scoped-replay-protection.md).
3. **N8 typed signing: deferred.** Out of the current scope.
4. **Recovery-tool shape: a later dedicated session.** The direction mapper's
   sketch is under [Consumer surfaces](#consumer-surfaces).
   - Bart's client ignores cockroach recoveries, so a v1 can be the extraction
     of `setup --recovery` into a keyless tool that emits only the sequencer
     bundle; the client output can wait.
   - Whether checkpoints carry scheduler state stays coupled to the fridge bound.

### W1: correctness and safety (about 18 small patches, no decisions)

| Item | Change |
|---|---|
| K1 | Integration helpers panic on bind failure instead of silently passing 43 tests. |
| K9 | Make the 429 test deterministic; poll the txpool instead of sleeping 7 s. |
| L2 | Release checks out `inputs.tag` and fails if the tag is missing. |
| LS1 | Pin the guest kernel SHA in `toolchain-pins.env`. |
| LS2 | Mark `-` tags as prerelease. |
| DS1 | Treat `ChannelClosed` during requested shutdown as graceful (exit 0, not 1). |
| G6 | `TipAlreadyOpen` refuses instead of retrying an impossible state. |
| H11 | Idle poll interval rejects 0 (`range(1..)`). |
| H8 | `run`/`flush` refuse a missing data dir before creating it, naming the path. |
| B3 | `fold_replay` asserts every replay and seed input is at or below C. |
| A6 | C adapter maps lifecycle-only statuses from execute/validate/drain to `Internal`. |
| M4 | Watchdog `init` refuses, instead of deleting, an agreed head beside a missing config. |
| MS1 | Watchdog `clear` never creates the state dir. |
| M3 | Idempotent watchdog `init` validates the existing config schema. |
| E3(5) | SDK `subscribe()` gets the same timeout as its siblings. |
| Docs | AS1 (AGENTS.md:216 rejection list), BS1 (spec sentence and deployment checklist), GS1 wording, `lifecycle.rs:237` comment, Track 3 and tracks-board text updated to match step 0. |
| Register | Close N9, N10, N11, N12, N13, N20, FEE1, AI1, AI3 and the stale testsi entry; fold FEE2 into fee-observation visibility; widen the RPC-URL entry to the reader, poster, flusher, oracle and watchdog. |
| N9 | Drop the `docs/recovery/history` model from `check-all`; it stays as a dev analysis tool. |

Exit: the suite fails on bind error, the release pins the kernel and checks out
the tag, and the register reflects the closures.

### W2: operator surface (about 8 patches, behavior only)

- **Logging, one PR.** Embed `{source}` in Display (H1, D1, E6); put paths in
  I/O errors (H2); log retries at WARN and verdicts at ERROR (H3, F2); add timing
  knobs to the startup event (H10); log post-200 stream failures (I6, M5, C8).
- **RPC URL redaction.** One choke point at the provider boundary (register, M1,
  G10).
- **Listener split.** Separate public and internal listeners (N24, register),
  with D8 folded in.
- **Fee oracle numerics.** One f64 log expression replaces the integer price
  pipeline (C2). Then the signed exponent, so sub-unit gas prices stop flooring
  at 1 unit/gas (CS1, schema CHECK change). Keep the cheap NUM comment.
- **Smaller items.**
  - SDK HTTPS for ingress only, plus the reqwest/thiserror/toml version alignment (L4).
  - Validate batch-open together with the danger timing (register).
  - Fix setup's exit classification for plain-setup sites (register).
  - Have `flush` wait on slot consumption (G11).
  - Record per-batch snapshot timing (I5).
- Exit: a synthetic credential URL never reaches logs or status, every abort line
  names its root cause, and the routes are split.

### W3: harvest (pure refactors, roughly −2.5k lines)

- **W3a tests and benchmarks, first.**
  - Adopt one test-placement convention (N4) with one `test_support` module (K2), one stub app (K3), and recovery_tests fixtures plus the schema tests relocated (K4).
  - Drop the benchmark verdict code (K6), then dedupe the tooling (K7), then add DB size and drain time (K8, N17).
  - Smaller items: K10–K12, J6 with the wallet business-failure test from the register, J11, F3, F5.
- **W3b production.**
  - sequencer-core: B6 (private ProtocolTiming fields), B7, B8, C3 (dead fee inverses), C5.
  - sequencer: D2 (test fixtures out of `storage/ingress.rs`), D4, F4, F7, F10, H6, H7, H12, I7, E4 (pure move).
  - Examples: J1, J5, J10.
  - Build: L1, L3 step 1, L5, L6, L7, L10 (shellcheck, luacheck, `just ci`
    removed), L11, L13, L14 (the readme path; the tar and libtest-mimic
    hoists). L12 moved to Track 5 in W5.
  - Scheduler-code refactors (B6–B8) compile into the guest. Run the canonical
    guest tests and the duality test for each, and expect the template hash to
    move.
  - Renames: rename the submitter's wallet `nonce` to `wallet_nonce` (N14); test literals for MAX_WAIT_BLOCKS (N16).
  - From the input-reader integration: `test-sequencer` runs at the default
    thread count (N18); a 3xx at setup's chain-id check is a configuration
    error (exit 30).
  - Added during W3b: the F1 and J9 decisions, the process-lock test flake's
    cause (a child spawned by a concurrent test briefly holds a copy of the
    lock's descriptor), consensus vectors, the remaining test-only `pub`
    storage items behind `cfg(test)`, a Rust 1.99, dependency and nixpkgs
    refresh, and a reproducible guest build: cross added `rust-src` only on a
    later run, so the template hash depended on build-cache history.
  - Also added: action pins checked offline in CI and audited upstream before
    each release, the release published with the `gh` CLI so no third-party
    code holds `contents: write`, the Node 20 actions bumped, and the runner
    image pinned to `ubuntu-24.04`.
- Exit: no `#[path]` test modules, one `test_support`, a benchmark crate of
  about 3.3k lines, and no `pub` test-only items in production modules. Test
  fixtures live in `test_support` or `storage::test_helpers`; `#[cfg(test)]
  pub(crate)` seams on private state and test constructors or doubles beside
  their type (the worker test spawners, the poster's test hooks and mock,
  `ProcessLock::test` and `acquire_released`, `SafeInputRange::empty_at`,
  `DumpInfo::at_baseline`) stay where they are.

### W4: docs lean pass (docs only, about −1k doc and −350 comment lines)

- **Order.**
  1. Adopt the style rule first (N14: one fact, one home, plus size caps).
  2. Shrink AGENTS.md to about 230 lines (N2), keeping the account-9 and #http-endpoints anchors.
  3. Trim the 24 longest module headers (N5).
  4. Retire the 09-18 and 09-19 records (N8).
  5. Move the threat-model essays out of table cells (N12).
  6. Then the rest of N3–N13, B5, B10, F9, G7, G8, H13, I8, M6.
- **Porter spec (B11).** About 130 lines: SSZ layout, domain, `UserOp` type
  and constants. Describe the signature acceptance set (B1) as currently
  implemented and mark it pending; making it normative binds Bart's port (5c).
- Independent docs patches can land beside any wave; the bulk waits for W3 so
  the text describes final code.

### W5: contract wave (needs a sync with Bart before any change)

The sub-batches double as the agenda for that sync; each ships separately.

- **5a seam v1 (C ABI).** A1, A2, A7, A3 header trim, Track 6 (emit callback,
  path suffix, ABI version getter, linker policy), and L10's
  `undocumented_unsafe_blocks` lint with its SAFETY-comment decision for the
  FFI crates. N8 is deferred.
- **5b batch-format bump (wire).** `[u8; 65]` signature (N7), the full SSZ
  overhead fix (register), A8. Drain sealed batches first and land before the
  first production deploy; an undecodable batch from the submitter key is
  terminal since W3b (F1), so the drain is load-bearing.
- **5c guest-image batch (consensus, with the C++ port).**
  - C1 fee rule; B1 acceptance set (document as-is, or narrow); B9 empty batches.
  - Track 5 fee table, with L12, which Bart's port consumes byte-for-byte.
  - The fridge bound, serializable `SchedulerState` with a "scheduler" inspect
    query, and B13 guest reports.
  - The BS1 setup cross-check and conformance vectors, extending
    `tests/fixtures/consensus_vectors_v1.json` (W3b) as a new versioned file.
  - J5's strict-order snapshot decode, which changes what the guest accepts.

### W6: recovery tool lands (direction; roughly −5k net)

- **Tool.** New `recovery` crate: move `fold.rs` as a pure move first, then add
  an outputs observer and `resume_from`.
- **Import.** `setup --import` keeps the B≤C check, `DeferUntilAnchorSet` and
  the rebuilt-anchor test. Delete `setup --recovery` (G1, H5).
- **Supporting moves.** Move `dump_info.rs` into the bundle crate and drop
  on-disk `info.toml` (I1), since the tool owns the export format; GS1
  old-watermark input; genesis case (N1, G3).
- **Deletions.** Projection replay (E1) and its docs and tests.
- **One baseline-migration rewrite.** Tool-minted era, drop the cut ledger,
  in-memory leases (I2), H14. CS1 lands earlier, in W2, by editing the
  baseline migration in place, which the migration policy allows today.
- **After the new sync is designed.** Retire the WS wire format,
  `/latest_snapshot` and history headers (I3), the `/history` reshape (E2),
  identity types (A4) and `cold_replica`. Keep in-era generation signalling in
  some form.

## Decisions

Step 0 settled the four that blocked planning. The rest, by the wave they gate:

| Wave | Decision | Recommendation |
|---|---|---|
| W1 | Release posture (L1): SHA-pinned actions, job-level tokens, no caches in release | Adopt; dispatch rebuilds existing tags only |
| W2 | Retry log level (H3, F2) | Verdict-driven: retries WARN, terminal ERROR |
| W2 | Keep the `terminal_faults` black box (H9) | Keep; fix its forever-warn (HS1) |
| W3b | Own-key undecodable batch (F1, disputed) | Decided: terminal under self-trust (`UndecodableOwnBatch`; `run` aborts with SIGABRT 134, like any terminal runtime fault), after a refactor patch |
| W3b | C-engine PermissionDenied (J9) | Decided: operational (exit 1), so native and C engines agree |
| W3b | Guest-image move outside the contract wave (U0) | Decided: allowed for behavior-preserving commits behind the consensus vectors, as one contiguous batch |
| W3b | Operator-visible renames and exit codes (U3, U4) | Decided: `wallet_nonce` log keys in their own commit; a 3xx at setup's chain-id check exits 30 |
| W3b | CI supply chain (U5, U6) | Decided: every action pinned by SHA; cargo-deny blocks on sources, reports advisories without blocking, no schedule. Dependabot declined: it moves a pin without verifying the new code, so its PRs are either rubber-stamped, undoing the pin, or reviewed as carefully as a manual bump. Pins are bumped deliberately before each release instead; the procedure and this reason live in `scripts/verify-action-pins.sh`. Revisit if pins go stale unnoticed again, the set of actions grows, or someone can own a weekly review |
| W4 | Contract owner: README or `openapi.yaml` (E9) | Moot for the frozen egress half; decide with the new sync |
| W1 | `docs/recovery/history` TLA+ (N9) | Decided: drop it from `check-all`; it stays as a dev analysis tool |
| W3 | CI double run (L3) | Decided: step 1, CI on pull requests plus pushes to `main` and `v*` tags, superseded PR runs cancelled. Nightly runs declined; revisit when one-off PR failures stop being a signal or someone owns a red nightly |
| W4 | Chain-id failover (FS1) | Out of scope per "one consistent node"; reword with a revisit trigger |
| W5 | Signature acceptance set (B1) | Document now; narrow to v∈{27,28} and low-s in the port's image. The vector table is pinned (W3b): both paths accept the high-s twin and any v ≥ 35 |
| W5 | Out-of-range frame fee (C1) | Structural `FeeAboveMax` rejection |
| W5 | Zero-frame batches (B9) | Reject only if the port forces a new image anyway |
| W5 | `scheduler_accepts` mirrors gate d (B2, disputed) | Keep the omission; the trace found no reachable case |
| W5 | Batch "nonce" naming (N14) | Not "index": it collides with `batches.batch_index`. If renamed at all, "sequence number", inside 5b |
| — | MAX_WAIT_BLOCKS (N16, H4) | Status quo; a setup flag adds unverifiable drift. Revisit only if a deployment needs a different window |
| — | Flashbots or fanout (N22) | Decline Flashbots; defer fanout; record the reason and a revisit trigger |
| W6 | In-memory leases (I2) | Registry, with a "single serving process" assumption; drop `lease_count` in the rewrite |
| W6 | Fee-observation stamp (register) | Keep the per-poll write (it costs under 0.1%) unless the column goes |

## Backlog triage

| Note | Status | Evidence | Next |
|---|---|---|---|
| N1 genesis recovery ergonomics | open | `--recovery` needs a hand-made bundle; the loader already accepts genesis (setup/mod.rs:449-452) | Genesis case in the tool (W6) |
| N2 dump-dir metadata | partially | `next_batch_nonce` is stored 3× (SQLite, `info.toml`, `checkpoint.toml`) | Drop on-disk `info.toml` (I1, W2); one receipt in the tool (W6) |
| N3 snapshot API, mmap/CoW, machine-emulator#398 | partially | The watchdog already uses `rename_stored`/`remove_stored` (machine.lua:114, 233-241); the sequencer's publish point is the SQLite row | Adopt nothing; give Bart a reflink recipe (W4) |
| N4 test layout | done | Seven placement styles; one `#[path]` module (recovery.rs:642) | Rule in AGENTS.md, moves landed (W3a) |
| N5 clippy exclusions | stale premise | CI clippy runs `--workspace --all-targets --all-features` over all 20 members (ci.yml:66) | Close; lint the shell scripts instead (L10) |
| N6 exponentiation/log | partially | TickMath is already f64 (7cfb474); the scheduler table is Track 5 | Log space stays: oracle math in W2 (C2); table in W5 with Bart |
| N7 `[u8; 65]` signature | open | `Vec<u8>` (batch.rs:62-68); the DB already enforces 65 | 5b, with the SSZ overhead fix |
| N8 parametric `UserOp` | deferred | EIP-712 has no sum types; `data` signs as hex | Out of scope (step 0); if revived, typed signing via an app hook over opaque transport |
| N9 safe_block ≤ inclusion_block | fixed | Gate d precedes staleness (scheduler/mod.rs:295-317) | Close |
| N10 time-only frame rotation | fixed | Rotation is safe-block distance only (inclusion_lane/mod.rs:246-250) | Close |
| N11 generation on cascade | fixed | Generation advance plus 409 on reconnect (subscribe.rs:58-70) | Close; in-band notice belongs to the new sync |
| N12 paginated replay and offset C | fixed | Pager, per-page revalidation, base K (canonical.rs:47-71) | Close; the historical half is frozen (E1) |
| N13 nested `.DS_Store` | fixed | The unanchored `.gitignore:11` pattern matches nested paths | Close |
| N14 nonce → index | done | Collides with `batches.batch_index` | The submitter's account nonce is `wallet_nonce` (W3b); the batch nonce stays (see Decisions) |
| N16 MAX_WAIT_BLOCKS configurable | done | Single constant (lib.rs:21); never changed | Status quo; tests take it from `sequencer_core` (W3b) |
| N17 benchmark DB size and drain time | done | Only a memory report exists | Reported per sweep step (W3a) |
| N18 cross-compilation and compile times | done | Guest image built twice in CI; 4 duplicate dependency stacks; no profile tuning | One CI run per push (L3), the guest image built once (L7), line-tables-only debuginfo (L13, `target/debug` −22%), parallel `test-sequencer`, one tungstenite stack (W3b) |
| N19 logs under error | done | ANSI and key redaction done; causes now ride in Display, and `error_chain` renders dependency errors' causes; levels follow the verdict | W2 and the input-reader integration |
| N20 hot-path clones | stale premise | No per-op clone that matters; fsync and checkpoints dominate | Close |
| N21 supply chain | done | Deps already hoisted; one unused dep; duplicates; no advisory gate | L4 (W2); cargo-deny (L11), hoists (L14), and held majors with their revisit triggers at their manifest lines (W3b) |
| N22 Flashbots/fanout | decide | The poster reads from the same node; fanout only helps a >4 h propagation failure | Decline; record the reason (W4) |
| N23 conditional snapshots | decide | Exact per-batch snapshots are load-bearing (snapshot_dumps.rs:325-330) | Keep per-batch; add timing (I5) |
| N24 split servers | open | One merged router (http.rs:331-347) | W2 |
| N25 verbose docs | open | Rules exist; length and repetition drift | W4 |
| AI1 BUSY/LOCKED classifier test | fixed | Classifier tests exist at every layer | Close |
| AI2 watchdog config validation | partially | Residual needs a hand-edited `config.json` | M3 (W1) |
| AI3 watchdog snapshot validity | fixed | `check_source` plus `trusted_start` (bootstrap.lua:51-52) | Close |
| FEE1 TokenHandle latch | fixed | TokenHandle removed (9a3046f) | Close |
| FEE2 oracle writes per poll | stale premise | Max-age no longer exists; cost under 0.1% | Fold into fee-observation visibility |
| NUM numeric stack | partially | Price math is f64; the scheduler consumes only the published fee | Comment and C2/CS1 in W2, C3 in W3; the batch-fee half is deferred with batch-scoped replay protection |

Register entries the review re-checked: the SSZ overhead entry is still open (5b).
The testsi cycle-target entry is fixed (1283765; delete it). The RPC-URL entry is
wider than recorded (W2). The listener entry is N24 (W2). The HTTPS/WSS entry
splits: HTTPS for ingress now, WSS dropped. Admission overlap and the setup
misclassification belong in the setup reshape (W6), apart from the plain-setup
sites (W2).

## Consumer surfaces

**`Application` trait and C ABI.** The trait is sound for execution: progress is
engine-owned and checked at the shared boundary, and the integer enums refuse
unknown values. The friction is at the seam:

- Host-only rejection codes appear in the app's vocabulary (A1).
- Engines must fsync a directory the host owns (A2).
- The stateful drain and path callback, and the missing ABI version, are open Track 6 items.
- The contract is written four times (A3).

Ship one seam-v1 change agreed with Bart. Let the recovery tool, not each engine,
own baseline nonces and pending-direct eligibility; that drops the
nonce-enumeration ABI and the per-engine exporter burden.

**Scheduler spec.** The algorithm prose is clear and the fold is pure. A C++ port
still lacks:

- the SSZ layout, the domain and the `UserOp` type;
- the signature acceptance set (B1);
- the sequencer-address provenance rule (BS1);
- the out-of-range fee rule (C1);
- language-neutral conformance vectors.

Scheduler state (app, queued directs, next nonce) is private, so recovery
re-seeds (A,B] instead of resuming. Direction: a porter spec plus trace vectors
now, and a serializable `SchedulerState` designed together with the fridge bound
in one guest-image change.

**Egress API.** The premise (client = replica of sequencer state) is wrong for
full-database clients:

- Snapshot-plus-subscribe loses the client's projection.
- Cross-era recovery makes every client embed the scheduler.
- Messages carry inputs without outputs or outcomes.

What survives: the ingress routes, `/finalized_state`, `/digest` and
`/inclusion_block` (the watchdog's only dependency), `/finalized_snapshot`, leases,
the canonical page reader with `HistoryClaim` (native restart uses it), the
listener split, and in-era generation signalling.

The recovery-tool sketch (from the direction mapper):

- **Inputs:** deployment identity, a checkpoint (genesis or scheduler state at B),
  the stop block C from `flush`, and an input source over (B,C].
- **Outputs:** written file-first, with `receipt.toml` last as the commit point
  (tool-minted era, B, C, N', K, digest). One output is a sequencer bundle for
  `setup --import`. The other is a versioned client trace of (B,C] with outcomes
  and app outputs.
- **Client rule:** a client imports at an era boundary and never subscribes
  across one.

## Correctness findings

| Id | Sev / status | Where | Scenario | Fix |
|---|---|---|---|---|
| BS1 | medium, single-source, grep-checked | wallet.rs:73,85; setup/mod.rs:580,591 | The guest's compiled `sequencer_address` differs from the pinned submitter, so every batch becomes a direct input. The frontier and fold still report acceptance; only the watchdog notices | Spec sentence and checklist (W1); setup cross-check via inspect (5c) |
| B1 | medium, partially | scheduler/mod.rs:350-358; storage/ingress.rs:723 | A high-s signature is admitted, stored verbatim and accepted by the Rust guest. A spec-following low-s port skips it and diverges | Normative wire/signature section plus vectors (W4); decide narrowing (5c) |
| CS1 | medium, single-source, code-checked | math.rs:33-40; fee.rs:136-138; schema:573 | USDC fee token, ETH at $2,500, gas under 0.4 gwei: floored at 1 unit/gas, so 4× overcharge at 0.1 gwei | Signed exponent plus relaxed CHECK (W2) |
| K1 | medium, confirmed, traced | integration_tests/* (4 helpers) | A sandbox denies bind, 43 tests return early, and the suite shows green | Panic on bind error (W1) |
| L2 | medium, confirmed, traced | release.yml:42-272 | A dispatch from `main` naming a tag publishes main's build under that tag | Check out `inputs.tag` (W1) |
| LS1 | medium, single-source, code-checked | canonical-app/justfile:25-37 | The kernel is verified against a checksum from the same release, so a replaced asset changes the template hash unnoticed | Pin the SHA in-tree (W1) |
| M1 | medium, confirmed | watchdog/http.lua:73; main.lua:146,176-182 | A transport failure writes the RPC URL, with its API key, to stderr, `last_tick.json` and `status` | Redact (W2) |
| C1 | low, confirmed, traced | fee.rs:68-71; scheduler/mod.rs:229-262 | A frame fee above MAX_EXPONENT plus a max_fee=65535 op makes the guest panic deterministically; needs an encoder bug or a stolen key | Structural rejection (5c) |
| A6 | low, confirmed | c-app-engine/src/lib.rs:70-85 | An engine bug returns NOT_FOUND from execute, which becomes non-terminal I/O and a restart loop | W1 |
| G6 | low, confirmed | storage/recovery.rs:115-119 | An impossible lock-violation state is retried and hidden | W1 |
| DS1 | low, single-source | inclusion_lane/mod.rs:143-150,439-443 | SIGTERM races the frontier check, and a clean shutdown exits 1 | W1 |
| GS1 | low, single-source | setup/mod.rs:652; flush.rs:30 | The cockroach flush reads the watermark from the wiped DB, so zombie coverage falls back to the pool view | Docs now; the tool takes the old watermark (W6) |
| B3, H8, H11, M4, MS1, K9, AS1, J9, FS1, FS2, LS2 | low | see appendix | Fail-loud gaps, misconfiguration traps, flaky timing | W1–W2 |
| B2, F1, G4, H4 | disputed | see appendix | A skeptic confirmed each, but an independent trace refuted it | See Decisions |

## Freeze list

Bug fixes only until W6 or the new sync design:

| Item | Size | Fate |
|---|---|---|
| Projection replay: `/historical-l1-inputs`, SDK history, `historical_bootstrap`, `projection-replay.md`, README/OpenAPI sections (E1) | ~1.6k core, ~3.5k with docs and tests | Delete in W6 |
| WS wire format and SDK snapshot→subscribe bootstrap (E3) | ~1.4k | New sync; the page reader and `HistoryClaim` survive |
| `/history` baseline and `accepted_checkpoint` (E2), `/latest_snapshot` and history headers (I3) | ~0.5k | Reshape or delete with the new sync |
| Era/generation identity types (A4), cut ledger and compatibility query | ~0.6k | New sync decides; in-era generation survives |
| `setup --recovery` orchestration, config and errors (G1, H5) | ~0.45k (~1.6k subsystem) | About 150 lines move to the tool; the rest is deleted |
| `fold.rs`; cockroach reader seams (F8); genesis plumbing (J2) | ~0.4k | Move or partially delete in W6 |
| `batch_policy` log-additive design (C6) | ~0.25k | Frozen pending the fee decision |

Not superseded, despite first impressions: the snapshot lease supervisor (D7),
`/finalized_state` and `/digest` with sha2 (L17), `generation_tests.rs`, and the
engine path callback.

## Not doing

Each item was refuted or declined. Re-propose only with new evidence.

- Prior juries: a JoinSet supervisor, collapsing PreparedRuntime, flattening
  RecoveryError, merging BootstrapError variants, a TerminalityOf trait, moving
  phase→progress into drive_recovery, a bool `/tx` gate, AppWithProgress, a
  components-vec Workers, UniswapConfig::pinned, retain-over-expect, and internal
  retry loops.
- This review:
  - Dropping the lane's drain cursor (D3): it is an independent I6 witness.
  - The flusher's two-call TOCTOU (G5): unreachable under the trust model.
  - A reusable dump-contract kit (J7): Track 6 owns it.
  - Renaming nonce to "index" (N14).
  - A `--max-wait-blocks` flag (N16).
  - Flashbots (N22) and sparse snapshots (N23).
  - Hot-path clone work (N20).
  - The max-age/2 write rule (FEE2).
  - Nonce-enumeration ABI exports: the tool owns baseline nonces.
  - A closure-based guarded helper (G12): prefer `begin_guarded`.
  - A two-connection contention test (AI1): it would test SQLite.
- The input-reader review (sling comparison): contiguity-only chunk commits,
  warn-then-error transport logging, a reader bound at `i64::MAX`, HTTP/2 or
  keepalive, per-profile timeouts, 60 s or 20 s timeouts, a hash-pinned safe
  head, a poster count witness, `getInputHash` as a content witness, and tests
  pinning read/submit routing. A separate submit (private relay) URL is
  deferred: `create_keyed_providers` is the seam.
- W3b:
  - Migrating the vendored guest crates (libcmt-sys, trolley, testsi) to edition
    2024: they compile into the guest and are a documented vendored fork.
  - Declaring every internal crate once in `[workspace.dependencies]` (L14b),
    and `publish = false` in `[workspace.package]` (L14): each member would
    opt in for nothing, since nothing here is published.
  - Pointing rollups-types at the workspace alloy entries: a guest SDK crate
    needs neither `serde` nor `k256`, and inheritance cannot remove features.
  - Dependency majors: tower-http 0.7, bindgen 0.73 and tokio-tungstenite 0.30,
    with reasons and revisit triggers at their manifest lines; k256 0.14 and
    sha2 0.11 wait for alloy and RustCrypto.
- Keep, despite looking heavy:
  - Exit-verdict projection, SYNCHRONOUS FULL, and schema triggers and typeof guards.
  - Per-batch exact snapshots, GC beyond the frontier, and the I1 duality test.
  - The reader's double completeness witness, the watermark-before-broadcast rule, and the stateless submitter.
  - The full keep lists are per area in the appendix. Splitting every failed
    log range supersedes the reader keep item on configurable range-error
    codes, and M2 narrows to the watchdog's persisted sequencer URL.
