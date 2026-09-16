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
| 3 | Feed & replay protocol redesign | us (design) → us/Stephen (impl) | **internal read foundation implemented; consumer API open** — the [Track 3 ordered handoff](2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff) owns the accepted workflow and remaining cutover |
| 4 | Storage decode policy | us | **done** — fail-loud for contract-impossible values; the named `saturating_query_bound` only where clamping preserves the predicate (policy lives in `storage/convert.rs` + the invariants check policy) |
| 5 | Fee exponentiation LUT | us | **deferred** — decided exact-floor if built (the table *is* the spec, algorithm-free; replay continuity across the upgrade explicitly not preserved); a separate pending design decision may make log-space fees defunct — revisit after syncing with Bart |
| 6 | Dump / `Application` API redesign | us + Bart | **revised interface implemented** — [Application contract](../protocol/application-contract.md); native bridge conformance is a separate integration branch |
| 7 | LLM context-engineering review | us | **done** — skills/agents/settings homed in-tree; the docs-practice rules live in AGENTS.md |
| 8 | Runtime ownership and terminal stop | us | **done** — owned by the [authority-boundary ADR](2026-08-authority-boundary-adr.md) |

**Current campaign order:**

1. Review the Track 3 internal history-read and snapshot-metadata foundation.
2. Implement the coordinated Track 3 HTTP/WS/SDK cutover on its successor branch.
3. Validate Track 6 against the reference C bridge, then the private DEX engine when shared.
4. Track 5 (fee LUT) only after the log-space-fees decision.

Deferred (revisit with libdex rollout): multi-file/tar snapshot serving
(`docs/snapshots/lifecycle.md` known limitation), pending-snapshot-pool cap.

## Track 3 — Feed & replay protocol redesign

Infrastructure subscribers download an application-defined snapshot over HTTP,
restore their application, and use one WS stream for both canonical backlog and
live inputs. The [design](2026-07-track3-feed-replay-design.md) owns the history
claims, typed refusals, resource bounds, and fresh-snapshot recovery workflow.
Raw `/inputs` and separate HTTP transaction replay are outside this feature.
The watchdog retains its independent trusted-state/L1 comparison workflow.

The internal foundation provides typed history claims and policy errors,
coherent history-bound reads, inclusive canonical pagination, and history
identity captured with a snapshot's lease and count. Existing HTTP/WS responses
still expose physical cursors. The consumer cutover updates snapshot metadata,
WS admission/replay, SDK, and harness together; it must also remove the total
catch-up cap while retaining bounded pages, queues, and subscriber counts.

Close the consumer invalidation finding only after that cutover and its
recovery/bootstrap acceptance tests. The next PR must demonstrate cold start,
ordinary resume, fresh-snapshot recovery, and gap-free backlog-to-tip delivery.

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
