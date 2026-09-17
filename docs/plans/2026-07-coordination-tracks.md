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
| 3 | Feed & replay protocol redesign | us (design) → us/Stephen (impl) | **implemented** — canonical application history, snapshot restore archives, mandatory WS claims, typed refusals, and SDK cutover; [remaining integration gates](2026-07-track3-feed-replay-design.md#remaining-integration-gates) |
| 4 | Storage decode policy | us | **done** — fail-loud for contract-impossible values; the named `saturating_query_bound` only where clamping preserves the predicate (policy lives in `storage/convert.rs` + the invariants check policy) |
| 5 | Fee exponentiation LUT | us | **deferred** — decided exact-floor if built (the table *is* the spec, algorithm-free; replay continuity across the upgrade explicitly not preserved); a separate pending design decision may make log-space fees defunct — revisit after syncing with Bart |
| 6 | Dump / `Application` API redesign | us + Bart | **interface and reference C binding implemented** — [Application contract](../protocol/application-contract.md); native-engine integration gates remain |
| 7 | LLM context-engineering review | us | **done** — skills/agents/settings homed in-tree; the docs-practice rules live in AGENTS.md |
| 8 | Runtime ownership and terminal stop | us | **done** — owned by the [authority-boundary ADR](2026-08-authority-boundary-adr.md) |

**Current campaign order:**

1. Validate snapshot-to-live replica bootstrap through the reference C bridge, then the private DEX engine when shared.
2. Remeasure feed latency in the representative environment.
3. Track 5 (fee LUT) only after the log-space-fees decision.

Full restore archives now support file and directory application prefixes.
Additional snapshot retention or transport mechanisms require a measured consumer need.

## Track 3 — Feed & replay protocol redesign

The current [history contract](../protocol/application-history.md) owns replica
bootstrap, history identity, replay, and recovery boundaries. The
[API contract](../../README.md#api) owns wire behavior. The wallet's cold replica
and canonical recovery/watchdog gates have a
[validation record](../review/2026-09-16-track3-validation.md).

Remaining work is native-engine integration and representative deployment
latency, tracked in the [integration plan](2026-07-track3-feed-replay-design.md).
Additional transport or retention mechanisms require a measured consumer need.

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

The [Application contract](../protocol/application-contract.md) owns execution,
engine progress, and checkpoint semantics. The [C binding guide](../protocol/c-application-binding.md)
maps that contract to native engines; its reference conformance suite is
implemented. End-to-end native snapshot-to-live bootstrap remains an integration
gate, alongside the private DEX engine when available. Reference bridge
conformance cannot establish private-engine correctness.

The [July proposal](2026-07-track6-dump-api-design.md) is historical; the
[September review](../review/2026-09-09-application-lane-dex-review.md) records the
accepted simplifications. Additional public checkpoint primitives or asynchronous
scheduling need a measured requirement. Watchdog extraction from the DEX's
canonical state drive remains separate work.
