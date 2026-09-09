# Coordination Tracks

**Status:** active plan of record. Tick / annotate as work lands; when a
track completes, move its durable outcomes into the normative docs and
collapse its entry here.

Context: Bart is building **libdex**, a native (non-CM) app whose backing
storage is an mmap'd flat buffer, and will reimplement the scheduler in C++.
Two of his needs shape this plan: bit-exact dapp-state mirroring from the WS
feed, and an ergonomic/efficient `Application` dump story. We break APIs
freely at this stage — no backward-compatibility constraints.

| # | Track | Owner | Status |
|---|-------|-------|--------|
| 1 | WS context fields + L1 provenance (PR #26) | Stephen | **done** — merged to main |
| 2 | Restore `docs/review/` ledger + this plan | us | **done** |
| 3 | Feed & replay protocol redesign | us (design) → us/Stephen (impl) | **storage foundation landed; public API open** — the [Track 3 ordered handoff](2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff) exclusively owns its sequence and decision gates |
| 4 | Storage decode policy | us | **done** — fail-loud for contract-impossible values; the named `saturating_query_bound` only where clamping preserves the predicate (policy lives in `storage/convert.rs` + the invariants check policy) |
| 5 | Fee exponentiation LUT | us | **deferred** — decided exact-floor if built (the table *is* the spec, algorithm-free; replay continuity across the upgrade explicitly not preserved); a separate pending design decision may make log-space fees defunct — revisit after syncing with Bart |
| 6 | Dump / `Application` API redesign | us + Bart | **revised interface implemented** — [Application contract](../protocol/application-contract.md); native bridge conformance is a separate integration branch |
| 7 | LLM context-engineering review | us | **done** — skills/agents/settings homed in-tree; the docs-practice rules live in AGENTS.md |
| 8 | Runtime ownership and terminal stop | us | **done** — owned by the [authority-boundary ADR](2026-08-authority-boundary-adr.md) |

**Current campaign order:**

1. Land the authority-boundary + durable-history-foundation branch (squashed,
   review complete — ready for its PR against main).
2. Implement Track 3's public protocol on a focused successor branch.
3. Validate Track 6 against the reference C bridge, then the private DEX engine when shared.
4. Track 5 (fee LUT) only after the log-space-fees decision.

Deferred (revisit with libdex rollout): multi-file/tar snapshot serving
(`docs/snapshots/lifecycle.md` known limitation), pending-snapshot-pool cap.

## Track 3 — Feed & replay protocol redesign

The current protocol grew ad hoc; the redesign is type-first and covers the
whole consumer data-access story: paginated finalized-history endpoints plus
the live subscription, composable without races. The
[design doc](2026-07-track3-feed-replay-design.md) owns the requirements and
the ordered implementation handoff; the storage/recovery foundation
(era/generation metadata, canonical `ExecutedInputCount` attribution,
snapshot/catch-up verification) is landed, while `GET /history-version`,
replay routes, gold-boundary projection, and WS v2 remain open.

Settled decisions the implementation must respect:

- **Feed coordinate:** `Application::executed_input_count()`, not SQLite
  rowid. An application at count `X` subscribes at `X`, consumes entry `X`,
  advances to `X + 1`. Standard recovery may reuse suffix offsets under a new
  generation; cockroach recovery records the folded count `K` as the era's
  available-history base, and requests below `K` fail with `available_from`
  plus the bootstrap recipe.
- **Discontinuity detection is pull-based.** A crash or danger-detector exit
  cannot send a farewell frame, so the load-bearing contract is the required
  subscription claim `{era_id, recovery_generation, offset}` plus a
  current-pair endpoint; in-band disconnect errors are best-effort only.
  Bart confirmed the scalar generation contract (2026-07-28); the `EraId`
  generalization and changed-era bootstrap behavior still need his consumer
  review and are not attributed to that confirmation.
- **Event framing:** per-row denormalized context (as shipped in PR #26);
  no `FrameSealed`/`BatchSealed` boundary events unless a consumer
  demonstrates the row context cannot express its need.
- **Clock:** application time is safe-block based. Direct inputs execute at
  their exact inclusion block; user ops at their frame's safe block.
  `block_timestamp` may ride as provenance but is never an application
  transition input (see the application contract).

## Track 5 — Fee exponentiation LUT (deferred)

`fee_to_linear` is consensus-critical (scheduler fold, guest agreement, app
fee charging) and must be bit-identical across the Rust sequencer, the
RISC-V guest, and Bart's C++ scheduler. Today's implementation shares a
15-entry squares table but also requires reproducing `fixed_mul` exactly —
256×256→512 widening multiply, `>> 64`, truncate, LSB-first accumulation
with floor after each multiply — which is unreasonable to demand of a port.
If built, the shape is decided: a full lookup table of exact
`floor((129/128)^n)` bignum values for every legal exponent (~17k entries ≈
550 KB). The checked-in table *is* the cross-implementation spec artifact
(algorithm-free, golden-hash-tested); `build.rs` verifies rather than
generates; C++ consumes the same file byte-identically; and replay
continuity across the upgrade is explicitly not preserved. Do not implement
until the pending log-space-fees decision lands (with Bart).

## Track 6 — Dump / `Application` API redesign

The accepted boundary keeps checkpoint creation, restore, disposal, and a pure
path to canonical comparison bytes. Creation takes `&mut self`, allowing an
adapter to flush or replace backing mappings while preserving logical state.
Checkpoints are durable before SQLite references them, immutable afterward,
and independently restorable even after source deletion. The application
prefix may be a file or directory.

The engine owns count/clock progress and reports it by value. Successful apply
hooks advance it; the shared boundary verifies the exact successor. Keep
`Send`, remove unused `Clone + Sync`, and place canonical inspection on its
actual consumer. See the [Application contract](../protocol/application-contract.md)
for migration and the [review ledger](../review/2026-09-09-application-lane-dex-review.md)
for the accepted simplifications.

The [July proposal](2026-07-track6-dump-api-design.md) is superseded. CoW,
flush/reopen sequencing, and working-image management belong inside an engine
adapter. Additional public primitives or asynchronous checkpoint scheduling
need a measured requirement. The DEX's private scheduler and bridge have not
been shared; conformance of the reference C bridge cannot establish theirs.
A watchdog comparison against the canonical DEX state drive is separate work.
