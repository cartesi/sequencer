# Coordination Tracks

**Status:** active plan of record. Tick / annotate as work lands; when a
track completes, move its durable outcomes into the normative docs and
remove its entry here. Track numbers retain their existing identities.

Context: Bart is building **libdex**, a native (non-CM) app whose backing
storage is an mmap'd flat buffer, and will reimplement the scheduler in C++.
Two of his needs shape this plan: bit-exact dapp-state mirroring from the WS
feed, and an ergonomic/efficient `Application` dump story. We break APIs
freely at this stage — no backward-compatibility constraints.

| # | Track | Owner | Status |
|---|-------|-------|--------|
| 3 | Feed & replay protocol redesign | us (design) → us/Stephen (impl) | **repository API implemented; post-merge adoption and deployment work remain** — [follow-up sequence](2026-07-track3-feed-replay-design.md#follow-up-sequence) and [ownership](2026-07-track3-feed-replay-design.md#merge-scope-and-follow-up-ownership) |
| 5 | Fee exponentiation LUT | us | **deferred** — decided exact-floor if built (the table *is* the spec, algorithm-free; replay continuity across the upgrade explicitly not preserved); a separate pending design decision may make log-space fees defunct — revisit after syncing with Bart |
| 6 | Dump / `Application` API redesign | us + Bart | **interface and reference C binding implemented** — [Application contract](../protocol/application-contract.md); native-engine integration gates remain |

**Current campaign order:**

1. Merge the implemented egress API after repository review/checks. Bart can then integrate his client; adjust the API from concrete feedback without waiting for downstream completion.
2. Extend reference C-bridge coverage in this repository. Application integrators/operators own private-engine validation, the canonical-to-native exporter and recovery drill, and representative capacity measurements before production use.
3. Track 5 (fee LUT) only after the log-space-fees decision.

Full restore archives now support file and directory application prefixes.
Additional snapshot retention or transport mechanisms require a measured consumer need.

## Track 3 — Feed & replay protocol redesign

The current [history contract](../protocol/application-history.md) owns replica
bootstrap, history identity, replay, and recovery boundaries. The
[API contract](../../README.md#api) owns wire behavior. The wallet's cold replica
and canonical recovery/watchdog gates have a
[validation record](../review/2026-09-16-track3-validation.md).

Readers whose projections contain information absent from the latest application
state can use the implemented fixed-prefix historical L1 API and checkpoint
metadata. The [projection contract](../protocol/projection-replay.md) owns that
workflow; the history contract owns implemented checkpoint compatibility across
standard recoveries. The [integration plan](2026-07-track3-feed-replay-design.md)
owns follow-up requirements and their owners. Native-engine integration and
representative deployment latency remain open after merge; they are not egress
API merge prerequisites. Other transport or retention mechanisms require a
measured need.

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

Remaining checks need the actual consumer:

- Supply the application's versioned canonical-machine-to-native recovery
  exporter and completed operator runbook. Require the
  [non-genesis recovery drill](../recovery/cockroach.md#recovery-readiness-before-deployment)
  for production readiness: the old native state is unavailable, the exported
  bundle restores correctly, and execution after rebuild matches the canonical
  machine. For the DEX, pin the designated state drive/memory region and derive
  resume metadata from canonical execution. Add the integration check to the
  release validation once the actual artifacts are available; no generic trait
  or deployment gate currently enforces this requirement.
- Exercise snapshot-to-live bootstrap and canonical comparison through the C
  host in CI; its current smoke test builds and invokes `--help`. A reusable
  conformance runner needs engine-supplied genesis and meaningful accepted and
  rejected inputs. Compare canonical state files, not recovery-dump layouts.
- Verify the external scheduler's ordering, fee conversion, and recovery
  agreement. Publish independent-port fee vectors for the
  [current arithmetic](../../sequencer-core/src/fee.rs); a deferred LUT is a
  separate semantic change. Watchdog extraction from the DEX's canonical state
  drive remains integration work.
- Decide output storage, checkpoint layout, ABI version negotiation, generated
  bindings, and linker policy from concrete engine requirements. The current
  drain protocol and path callback remain the contract until then.

Additional checkpoint primitives or asynchronous scheduling need a measured
requirement. Any future microbatch priority scheme must preserve per-account
nonce order, serial execution, and the frame-time contract; the current lane
does not promise priority scheduling.
