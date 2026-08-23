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
| 6 | Dump / `Application` API redesign | us + Bart | **design drafted, under review with Bart** — [`2026-07-track6-dump-api-design.md`](2026-07-track6-dump-api-design.md); see the constraints below |
| 7 | LLM context-engineering review | us | **done** — skills/agents/settings homed in-tree; the docs-practice rules live in AGENTS.md |
| 8 | Terminal-containment structural consolidation | us | **done** — superseded by and landed through the [authority-boundary ADR](2026-08-authority-boundary-adr.md) |

**Current campaign order:**

1. Land the authority-boundary + durable-history-foundation branch (squashed,
   review complete — ready for its PR against main).
2. Implement Track 3's public protocol on a focused successor branch.
3. Track 6 implementation after the design settles with Bart.
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

The `create_dump` / `from_dump` / `delete_dump` / `state_file_in_dump`
surface is a leaky projection of what the inclusion lane needs; the design
doc inventories the real requirements (atomic crash-durable checkpoint,
startup reconstruct, disposal, serve-canonical-bytes-without-instantiating).
Constraints established for the design review with Bart:

- The CM emulator has **no commit/revert** — its API is
  `load / store / clone_stored / remove_stored`; commit/revert stay
  sequencer-side (DB row as commit point; older-dump+replay as revert). The
  genuinely missing primitive is **cheap clone** (reflink with graceful
  full-copy fallback; hardlink suitability for a mutable working image is
  disputed — settle in the design review).
- **Durability postures are opposite and must not be silently inherited:**
  the sequencer mandates fsync inside the checkpoint (I13); CM/Dave fsync
  nothing and compensate with hash-on-load. Keep the app-fsync posture; a
  CoW implementation must fsync what reflink leaves unsynced. (CM PR #398's
  durable `rename_stored`/`remove_stored` narrows this gap — re-check before
  finalizing the crash-safety section.)
- **libdex layout constraint to communicate early:** the served canonical
  file must byte-match the canonical machine's `inspect_state` output (the
  watchdog byte-compares). A raw mmap buffer with allocator padding or
  pointer-valued fields breaks that: either the buffer layout is itself
  canonical, or libdex needs a separate canonical projection.

Deliverable: design settled jointly with the Track 3 doc, in front of Bart
together — his on-disk layout decision depends on both.
