# Coordination Tracks — 2026-07

**Status:** active plan of record. Eight tracks are now recorded; the campaign
began with six agreed on 2026-07-23. Tick / annotate as work lands; when a track
completes, move its durable outcomes into the normative docs
(`docs/protocol/`, `docs/invariants.md`, `docs/snapshots/`) and mark it done
here. Multiple tracks may fold into one PR when practical.

Context: Bart is building **libdex**, a native (non-CM) app whose backing
storage is an mmap'd flat buffer, and will reimplement the scheduler in C++.
Two of his needs shaped this plan: bit-exact dapp-state mirroring from the WS
feed, and an ergonomic/efficient `Application` dump story. We break APIs freely
at this stage — no backward-compatibility constraints.

| # | Track | Owner | Status |
|---|-------|-------|--------|
| 1 | PR #26 WS context fields + L1 provenance | Stephen | **done — merged to main** |
| 2 | Restore `docs/review/` ledger + this plan | us | this PR |
| 3 | Feed & replay protocol redesign (type-first; history version; historical replay) | us (design) → us/Stephen (impl) | **history/canonical-offset storage foundation landed; API and consumer questions open** — follow the [Track 3 ordered handoff](2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff) |
| 4 | Storage decode policy (behavior + docs) | us | landed with this PR |
| 5 | Fee exponentiation LUT | us | **deferred** — decided exact-floor if built; log-space fees may become defunct (pending a separate design decision, syncing with Bart) |
| 6 | Dump / `Application` API redesign | us + Bart | **design drafted** — [`2026-07-track6-dump-api-design.md`](2026-07-track6-dump-api-design.md), under review |
| 7 | LLM context-engineering review (CLAUDE/AGENTS/docs/READMEs) | us | queued; run in a fresh session |
| 8 | Terminal-containment structural consolidation | us | **superseded; SQLite-centered successor landed through step 5, with step 4 benchmarked and step-6 storage/execution foundation landed** — [`2026-08-authority-boundary-adr.md`](2026-08-authority-boundary-adr.md). Structured runtime ownership, durable lifecycle, and the verified normal-`run` reducer plus `AdmittedRuntime` boundary landed first. Step 4 bounds each fast turn by one dequeue chunk (including rejects), makes poisoned-frontier refusal typed at the lane's existing read while `DangerDetector` owns prompt shutdown, and advances the frame clock once per five newly-safe blocks without synthetic catch-up frames. Its same-host release sweeps found no material regression. Step 5 retained distinct setup/rebuild/maintenance/acknowledgement protocols, moved genesis construction behind setup admission, made maintenance recovery-preserving and retryable, and landed the terminal-only two-second abort bound. Step 6 now has durable era/generation/base metadata, typed application progress, canonical per-input attribution, and snapshot/catch-up verification; API projection remains Track 3 work. Its repeated same-host ACK sweep found no material hot-path regression and zero rejections; round-trip measurement remains paired with the public API projection ([ledger](../review/2026-08-01-containment-adr-review.md)). |

**Current campaign order (updated 2026-08-18).** One focused branch/session per
bullet, roughly in order; design docs are reviewed by the team + Bart where
their consumer or application contracts are involved:

1. Land the current authority-boundary + durable-history-foundation branch
   after its documentation/status review. The 2026-08-18 over-engineering
   review ([ledger](../review/2026-08-18-over-engineering-review.md)) is that
   review; its wave queue (defects → deletions → collapses → `Authorized`
   token → lifecycle re-platform) gates landing.
2. Implement Track 3's public protocol on a focused successor branch. The
   [Track 3 ordered handoff](2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff)
   exclusively owns its sequence and decision gates; the durable
   era/generation/cockroach-base and canonical-offset DB slice is already
   landed.
3. Track 6 implementation after the design settles with Bart.
4. Track 7 — fresh session, docs-only campaign (see its section).
5. Track 5 (fee LUT) — **deferred**: a separate pending design decision may
   make log-space fees defunct entirely; revisit after syncing with Bart.

Deferred (revisit with libdex rollout): multi-file/tar snapshot serving
(`docs/snapshots/lifecycle.md` known limitation), pending-snapshot-pool cap.

---

## Track 1 — PR #26 (done)

What landed: `OrderedL2TxRow` enum on the egress read; user-op
`nonce`/`safe_block`/`batch_nonce` and direct-input
`input_index`/`batch_nonce`/`block_timestamp`/`transaction_hash` on the WS
feed; `live_start_offset` appended to the catch-up-exceeded close reason
(stable-prefix contract on `WS_CATCHUP_WINDOW_EXCEEDED_REASON`); direct-input
L1 provenance persisted on `safe_inputs` (timestamp sourced from the
`EvmAdvance` payload — what the CM guest sees); mirror-clock and nonce
cross-check assertions in the replay harness.

Explicitly **not** addressed there (moved to Track 3): the recovery
discontinuity contract — see F7 in
[`../review/2026-06-10-correctness-review.md`](../review/2026-06-10-correctness-review.md).

## Track 2 — Ledger restore (this PR)

`docs/review/` was dropped by the `8ce41cf` squash; restored from `0561d53`
with F7/WP5 statuses updated. The ledger is again the place PR reviews cite.

## Track 3 — Feed & replay protocol redesign

The current protocol grew ad hoc and is due a type-first redesign rather than
further field patches. Scope grew 2026-07-28: Bart needs **historical
replay** — the subscription's catch-up depth is shallow, so a consumer today
cannot bootstrap from genesis. The redesign covers the whole consumer
data-access story: paginated finalized-history endpoints + the live
subscription, composable without races.

**Accepted coordinate refinement (2026-08-02):** feed `offset` is
`Application::executed_input_count()`, not SQLite rowid. An application at
count `X` subscribes at `X` and consumes history entry `X`, which advances it
to `X + 1`. Standard recovery may reuse suffix offsets under a new generation.
Cockroach recovery records the folded application's absolute count `K` as the
new era's available-history base; requests below `K` fail with the bootstrap
recipe instead of pretending the lost prefix is available.

**Implementation status:** the DB/recovery and canonical-offset storage
foundation is landed. Baseline
creation atomically mints a UUIDv4 `EraId`, initializes generation zero, and
pre-arms the initial lifecycle `Active`; standard recovery increments the
generation in the same transaction iff it invalidates at least one valid
batch. Rebuild starts with `base_executed_input_count = NULL`, binds folded `K`
atomically with the initial finalized snapshot, and cannot complete setup while
the base is NULL or the snapshot is absent. `K` is distinct from physical
`l2_tx_index` cursor padding. Shared typed execution now emits the input's
pre-execution count; SQLite stores that attribution atomically with valid
physical rows, invalidation rewinds the derived suffix, snapshots store both
coordinates, and catch-up verifies both before execution.
The current rowid feed is unchanged. `GET /history-version`, replay routes,
gold-boundary projection, and WS v2 remain deferred.

**Historical replay requirements (from Bart, 2026-07-28):**

- Two paginated, finalized-only replay endpoints:
  1. **Input-box order** — the raw L1 sequence from `safe_inputs`: direct
     inputs and our own batches, original order, with the PR #26 provenance
     (block number/timestamp, tx hash) available per row.
  2. **Ordered L2-tx feed order** — the same coordinate space as the WS feed,
     capped at the recovery-stable boundary: rows covered by
     scheduler-accepted (gold) batches cannot be invalidated by standard
     recovery, so entries before that exclusive boundary are immutable and
     generation-free **within one era**. Every page carries `era_id`; clients
     must not splice pages across a cockroach rebuild.
- **Race-free handoff:** client replays history to boundary G, then
  `subscribe(era_id, recovery_generation, from_offset = G)`. The accepted v2
  contract makes the complete soft-history suffix `[G, H)` serveable over WS,
  and a client at boundary `H` can subscribe and wait for future inputs. A
  static catch-up cap cannot provide that guarantee because the soft suffix can
  grow without bound during an L1 outage; the v2 cutover therefore retires the
  old cap/prose-close contract rather than adding a replay-again race.
- A standard recovery between replay and subscribe is handled by the
  generation coordinate: finalized pages in that era stay valid, the
  subscribe is rejected as stale, and the client re-fetches only the suffix.
  An era mismatch instead forces current-era snapshot/bootstrap acquisition.

Current shape: `GET /ws/subscribe?from_offset=N` → upgrade → unframed infinite
stream of a two-variant enum (`BroadcastTxMessage`), with all session-level
signaling smuggled into transport close frames. No handshake, no control
plane, no catch-up/live marker, no recovery signal.

Design requirements:

- **History version `(EraId, RecoveryGeneration)`**. `EraId` is a UUIDv4,
  write-once-per-era setup/rebuild identity; `RecoveryGeneration` increments
  exactly once in the standard-recovery transaction iff at least one valid
  batch is invalidated. Clean restart changes neither. Cockroach
  recovery creates a new era because the old ordered history is unavailable;
  a raw DB clone copies the identifier, and operating copied state as a new era
  requires explicit fresh/wiped-directory setup/rebuild. The sequencer adds no
  automated DB replacement, clone detection, distributed fencing, or
  partial-fill resume machinery.
  Crucial constraint: recovery follows a crash or a danger-detector process
  exit, so a farewell "bad things happened" frame **cannot be load-bearing** —
  the dying process isn't there to send it. Discontinuity detection must be
  pull-based: a required subscription claim carrying
  `{era_id, recovery_generation, offset}`, plus a dedicated endpoint returning
  the current pair. WS responses carry `recovery_generation`, not `era_id`.
  An in-band discontinuity event is welcome as best-effort for graceful paths.
- Reconnect carries `(era_id, recovery_generation, offset)`. A stale
  generation rejects with soft-suffix replay; a changed era rejects with the
  current-era bootstrap recipe. Within the current era, an offset below its
  `base_executed_input_count` rejects with `available_from` plus that recipe.
- **Contract confirmed with Bart (2026-07-28).** His paraphrase, from the
  one-sentence pitch: subscribe passes the generation to resume from and the
  server replies with its current generation; a stale generation is rejected
  with an error; if the stream state becomes invalid the server disconnects
  with an error saying "roll back and recover" and bumps its generation.
  That is the intended contract, with one caveat to keep explicit in the
  design doc: the disconnect-with-error is **best-effort only** (a crash or
  danger-detector exit cannot send a farewell frame), so the load-bearing
  detection remains stale-generation rejection at resubscribe plus the
  generation endpoint. The later accepted `EraId` amendment generalizes this
  scalar contract to a pair; changed-era bootstrap behavior still needs Bart's
  consumer review and is not attributed to his earlier confirmation.
- Structured session errors (e.g. catch-up-window-exceeded as a typed message
  carrying `live_start_offset` and the snapshot-resync recipe) instead of
  close-reason string smuggling.
- **Event framing (settled for v2):** retain the per-row denormalized context
  PR #26 shipped. `FrameSealed`/`BatchSealed` boundary events remain excluded
  unless a consumer demonstrates that the row context cannot express its need.
- **Clock decision (settled):** application time is safe-block based, not an L1
  wall-clock timestamp. Direct inputs execute at their exact inclusion block;
  user ops execute at their frame's safe block. Empty clock frames improve
  resolution but do not mutate application state by themselves. The WS may
  still carry `block_timestamp` as provenance, but it is not an application
  transition input. See the authoritative application contract.
- **Scheduler count pass (landed):** the reference scheduler now follows the
  complete checked count transition table and shares its execution boundary
  with the live lane, replay, and both recoveries. `AppError` is fatal and
  overdue-direct ordering is test-pinned. Other small scheduler changes remain
  queued; keep them as a separate reviewed change set and preserve this shared
  boundary.

Closes F7/WP5 when done.

## Track 4 — Storage decode policy

`sequencer/src/storage/convert.rs` module docs promise saturating decodes so
corrupted/malicious DB rows can't crash the process; `docs/invariants.md`'s
fail-loud check policy mandates the opposite for contract-impossible values;
actual call sites mix both (e.g. `decode_l2_tx_row` expects/asserts, adjacent
width conversions saturate).

Agreed direction: **fail-loud wins** for contract-impossible values — a
saturated fabricated value flowing to a bit-exact mirror is precisely the harm
the policy exists to prevent. Saturating helpers remain only where the full
numeric range is legal (pure width conversions).

**Resolution (landed with this PR):** the split is by semantic provenance and
failure consequence. Stored contract values use checked width conversions and
fail loud. Legal query-only bounds use the explicitly named
`saturating_query_bound` where clamping preserves the predicate exactly (WS
cursors/limits and setup/recovery block ranges). Observational clock
serialization still clamps environmental representation bounds, while the
safety path refuses — after the observed danger checks — when the clock is
a full block-time or more out of step with either persisted baseline
(sub-block skew is tolerated). Fee-policy
reads are directional: high recommended fees cap, negative batch targets
floor, and the opposite/impossible directions fail loud. Contract-bound
counter advances are checked; the live accepted-batch writer uses plain
`INSERT` and checked nonce advancement; and `safe_accepted_batches.nonce` has
`CHECK (nonce >= 0)`. Boundary and storage-entry tests pin each exception and
enforcement.

## Track 5 — Fee exponentiation LUT

Problem: `fee_to_linear` is consensus-critical (scheduler fold, guest T3
agreement, app charging) and must be bit-identical across the Rust sequencer,
the guest, and Bart's C++ scheduler. Today's implementation
(`sequencer-core/src/fee.rs` + `build.rs`) shares a 15-entry table of
`(129/128)^(2^i)` squares, but a reimplementation must also reproduce
`fixed_mul` exactly: 256×256→512 widening multiply, `>> 64`, truncate, in the
same accumulation order. That is unreasonable to demand of a C++ port.

Plan: replace the arithmetic with a full lookup table — one `U256` per
exponent, `0..=MAX_EXPONENT` (~17k entries ≈ 550 KB; not 64Ki — exponents
above `MAX_EXPONENT` panic today and continue to). The table becomes the
cross-implementation **spec artifact**: checked-in binary/hex file + golden
hash test; `build.rs` verifies rather than generates; C++ consumes the same
file byte-identically. `fee_to_linear` = `TABLE[n]`. `fee_from_linear` /
`log_fee_ratio` are admin-side only (`set_alpha`) — derived binary searches
over the table with a documented tie rule; they need no cross-implementation
guarantee.

Decisions (2026-07-29): **exact-floor** — if built, the table is regenerated
as exact `floor((129/128)^n)` bignum values (the table *is* the spec,
algorithm-free); today's iterated-squaring outputs are not frozen, so replay
continuity across the upgrade is explicitly not preserved. **Deferred** — a
separate pending design decision may make log-space fees defunct entirely;
do not implement until that lands (syncing with Bart).

## Track 6 — Dump / `Application` API redesign

`create_dump` / `from_dump` / `delete_dump` / `state_file_in_dump` is a leaky
projection of what the inclusion lane actually needs. Requirements inventory
(from the 2026-07-22 trace of every call site):

- **R1** checkpoint current state at batch close — atomic, crash-durable
  (fsync-before-DB-row, invariant I13), exactly once per batch close.
- **R2** reconstruct at startup: latest checkpoint + replay.
- **R3** dispose checkpoints (GC + orphan sweep).
- **R4** serve canonical bytes over HTTP **without instantiating the app**
  (all `state_file_in_dump` exists for — it conflates "locate the artifact"
  with "the artifact is one literal file whose bytes are canonical").
- **R5** genesis (already off-trait, stays there).

Wished-for: cheap checkpoints (CoW), checkpointing off the lane thread (today
a full-state serialize stalls soft-confirmation acks at every batch close),
and the load-bearing fork — the app running against an mmap'd working image
the lane can flush-then-clone (the Dave sling model; libdex's natural shape).

Findings from the CM/Dave comparison that constrain the design:

- The CM emulator has **no commit/revert** — its API is
  `load / store / clone_stored / remove_stored`; Dave's commit/revert are
  node-level orchestration on top of `clone_stored`. The sequencer already
  has both concepts sequencer-side (DB row as commit point; older-dump+replay
  as revert) and they should stay off the trait.
- The genuinely missing primitive is **cheap clone**
  (reflink/`FICLONE`/`clonefile` with graceful full-copy fallback; hardlink
  suitability for our mutable working image is disputed — see the Track 6
  design §10 review remark). But a bare `clone_dump` patch only pays off after the
  working-image fork above is decided — so evaluate the fuller redesign
  first: e.g. a session-shaped API (app handle opened *on* a working image,
  `checkpoint() → SealedCheckpoint`, `canonical_bytes(checkpoint) → impl
  Read` replacing `state_file_in_dump`).
- **Durability postures are opposite** and must not be silently inherited:
  the sequencer mandates fsync inside the checkpoint (I13); CM/Dave fsync
  nothing and compensate with hash-on-load + older-boundary fallback. Keep
  the app-fsync posture; a CoW implementation must fsync what reflink leaves
  unsynced. Tell Bart explicitly — he wrote the CM side.
- **libdex layout constraint to communicate early:** the served canonical
  file must byte-match the canonical machine's `inspect_state` output (the
  watchdog byte-compares). A raw mmap buffer with allocator padding,
  free-lists, or pointer-valued fields breaks that: either the buffer layout
  is itself canonical, or libdex needs a separate canonical projection
  (which reintroduces serialize cost).
- Who implements what today: all four dump verbs are required app-author
  implementations (no defaults); there is no `restore_dump` — the restore
  half is `from_dump`. The sequencer owns `info.toml`, the
  `dumps/<dir>/{state,info.toml}` wrapper, DB rows, promotion, GC, leases,
  orphan sweep.
- Fold-in from PR #26: the `SafeInputRecord` shim in
  `sequencer/src/storage/l1_inputs.rs` (zero-provenance `StoredSafeInput`
  vs. `IngestedSafeInput`) is churn-avoidance, not design — collapse to one
  honest row model in a dedicated cleanup. The clock question is settled:
  `block_timestamp` is feed provenance, while application execution uses the
  direct's inclusion block.

**New CM development to draw on (2026-07-28, Diego,
machine-emulator PR #398):** running an epoch directly from storage exposed an
API inconvenience, fixed by adding `machine:rename_stored` and making both it
and the existing `remove_stored` **durable (automatically synced)**. Two
implications for us: (a) the durable-rename primitive is exactly the
commit-point idiom the sequencer's dump lifecycle hand-rolls (temp + rename +
fsync ladder) — a redesigned trait could make sealing a sequencer-side
`rename`-based operation the app never implements; (b) the CM's no-fsync
posture we flagged as a divergence is narrowing on the CM side — re-check the
posture comparison against #398 before finalizing the design's crash-safety
section.

Deliverable: design note in `docs/`, written jointly with the Track 3 doc and
put in front of Bart together — his on-disk layout decision depends on both.

## Track 7 — LLM context-engineering review

Review the repo's whole LLM-facing setup — and the human docs, which double
as agent context — against Anthropic's Claude-5-generation context
engineering guidance
(<https://claude.com/blog/the-new-rules-of-context-engineering-for-claude-5-generation-models>).
Distilled rules (kept in-tree so the campaign doesn't depend on the URL):

1. **Unhobble** — remove overconstraining/conflicting instructions; let the
   model use judgment from surrounding context.
2. **Rules → principles** — replace explicit constraints with principles
   ("match surrounding style", not "never do X").
3. **Examples → interfaces** — design expressive file structures and tool
   parameters instead of constraining by example.
4. **Progressive disclosure** — don't front-load; move detailed guidance into
   skills/docs loaded on demand.
5. **No repetition** — eliminate guidance duplicated across files; one home
   per instruction.
6. **Auto-memory over manual memory notes** in CLAUDE.md.
7. **Rich references over simple specs** — point at code, tests, rubrics
   rather than restating them in prose.
8. **CLAUDE.md stays lightweight** — repo purpose + repo-specific gotchas
   only; drop anything the model can discover itself.
9. **Skills as lightweight discovery guides** — split long ones, encode
   team-specific opinion, avoid over-constraint outside critical areas.

Scope: `CLAUDE.md`, `AGENTS.md`, everything under `docs/`, the human
`README.md`s, and any `.claude/` assets. Watch for: duplication between
CLAUDE.md and AGENTS.md, front-loaded detail that belongs in the normative
docs, and stale pointers (the `docs/review/` dangle was one). Run as its own
fresh session (docs-only; benefits from clean context and a fan-out audit).
