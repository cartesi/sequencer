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
| 3 | Feed & replay protocol redesign | us (design) → us/Stephen (impl) | **implemented; client adoption and deployment work remain; frozen until the shared recovery tool and a new client sync** — [scope](#track-3--feed--replay-protocol-redesign) |
| 5 | Fee exponentiation LUT | us | **unblocked, needs Bart** — fees stay in log space; the batch-level alternative is deferred with [batch-scoped replay protection](2026-10-batch-scoped-replay-protection.md). Shape decided: exact-floor table, algorithm-free; a consensus change agreed with Bart before implementation; the log-additive `batch_policy` design is frozen until then |
| 6 | Dump / `Application` API redesign | us + Bart | **interface and reference C binding implemented** — [Application contract](../protocol/application-contract.md); native-engine integration gates remain |

**Current campaign order:**

1. Bart's client integrates the feed and treats cockroach recoveries as out of scope; we fix bugs it finds, and other feedback is input to the new sync. A shared recovery tool, still to be designed, will produce both the sequencer's and the client's startup state from a checkpoint. Until it lands, `setup --recovery`, `fold.rs` and the cockroach reader seams get bug fixes only.
2. Application integrators/operators validate the private engine, canonical-to-native exporter and recovery drill, and representative capacity before production use. Reference C-host lifecycle coverage is part of repository CI.
3. Track 5 (fee LUT) after a sync with Bart, batched with other guest-image changes.

Full restore archives now support file and directory application prefixes.
Additional snapshot retention or transport mechanisms require a measured consumer need.

## Track 3 — Feed & replay protocol redesign

The egress API is implemented. The [history contract](../protocol/application-history.md),
the [projection contract](../protocol/projection-replay.md) and the
[API contract](../../README.md#api) own its behavior; the
[integration plan](2026-07-track3-feed-replay-design.md) owns deployment follow-ups.

A shared recovery tool will produce the sequencer's and a client's startup
state from one checkpoint, and a new client sync follows its design. Until
then, what they replace gets bug fixes only, and its docs are not polished:

- Deleted when the tool lands: projection replay across a rebuild
  (`/historical-l1-inputs`, the SDK's `historical_l1_inputs`, the main
  `historical_bootstrap` test, the projection contract, and the integration
  plan). First move the projection contract's seed/replay rules and worked
  boundary into the tool's design and tests, move the integration plan's
  deployment follow-ups to the register, and repoint the links to both files.
- Reshaped or retired with the new sync: the WS wire format, the SDK
  snapshot-then-subscribe bootstrap, `/latest_snapshot` and its history headers,
  `/history`'s baseline, accepted checkpoint and compatibility query, and the
  era/generation identity types. In-era generation signalling survives in some form.

Not frozen: the ingress routes, `/finalized_state` and its probes,
`/finalized_snapshot`, download leases, and the canonical page reader with `HistoryClaim`.

## Track 5 — Fee exponentiation LUT

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
continuity across the upgrade is explicitly not preserved. Fees stay in log
space; [batch-scoped replay protection](2026-10-batch-scoped-replay-protection.md)
records the deferred design that would move the fee to the batch. Agree the
table with Bart and ship it with the other guest-image changes.

## Track 6 — Dump / `Application` API redesign

The [Application contract](../protocol/application-contract.md) owns execution,
engine progress, and checkpoint semantics. The [C binding guide](../protocol/c-application-binding.md)
maps that contract to native engines. Its reference conformance suite and C-host
process scenarios cover snapshot-to-live bootstrap, restart, standard recovery,
and fresh-era rebuild with canonical comparison. Reference bridge conformance
cannot establish private-engine correctness.

Remaining checks need the actual consumer:

- Supply the application's versioned canonical-machine-to-native recovery
  exporter and completed operator runbook. Require the
  [non-genesis recovery drill](../recovery/cockroach.md#recovery-readiness-before-deployment)
  for production readiness: the old native state is unavailable, the exported
  bundle restores correctly, and execution after rebuild matches the canonical
  machine. For the DEX, pin the designated state drive/memory region and derive
  resume metadata from canonical execution. The exporter must check
  [pending-direct eligibility](../recovery/cockroach.md#checkpoint-eligibility)
  and the drill must exercise refusal and earlier-checkpoint fallback, alongside
  eligible pending-queue recovery. Add the integration check to the
  release validation once the actual artifacts are available; no generic trait
  or deployment gate currently enforces this requirement.
- Repeat snapshot-to-live bootstrap and canonical comparison with the private
  engine's genesis and meaningful accepted/rejected inputs. The reference
  `c_host_` scenarios supply the lifecycle pattern; compare canonical state
  files, not recovery-dump layouts.
- Verify the external scheduler's ordering, fee conversion, and recovery
  agreement. Publish independent-port fee vectors for the
  [current arithmetic](../../sequencer-core/src/fee.rs); a deferred LUT is a
  separate semantic change. Watchdog extraction from the DEX's canonical state
  drive remains integration work.
- Decide output storage, checkpoint layout, ABI version negotiation, generated
  bindings, and linker policy from concrete engine requirements. The current
  drain protocol and path callback remain the contract until then.
- Make `GET /nonce` exact for senders idle since a rebuilt baseline; today
  they read 0 ([application contract](../protocol/application-contract.md#user-nonces)).
  Proposed: an `Application` nonce query plus an enumeration method. Setup
  already holds the baseline engine when it publishes the baseline, so it can
  enumerate it into a write-once `baseline_nonces` table in the same
  transaction, the user-nonce counterpart of `batch_tree_anchor`. The point
  query also lets the shared execution boundary check the one-step nonce
  advance per op, as it checks progress. Both need C ABI exports and the
  engine owner's agreement.

Additional checkpoint primitives or asynchronous scheduling need a measured
requirement. Any future microbatch priority scheme must preserve per-account
nonce order, serial execution, and the frame-time contract; the current lane
does not promise priority scheduling.
