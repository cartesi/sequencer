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
| 3 | Feed & replay protocol redesign | us (design) → us/Stephen (impl) | **implemented; client adoption and deployment work remain; frozen until a new client sync** — [scope](#track-3--feed--replay-protocol-redesign) |
| 5 | Fee exponentiation LUT | us | **unblocked, needs Bart** — fees stay in log space; the batch-level alternative is deferred with [batch-scoped replay protection](2026-10-batch-scoped-replay-protection.md). Shape decided: exact-floor table, algorithm-free; a consensus change agreed with Bart before implementation; the log-additive `batch_policy` design is frozen until then |
| 6 | Dump / `Application` API redesign | us + Bart | **interface and reference C binding implemented** — [Application contract](../protocol/application-contract.md); native-engine integration gates remain |

**Current campaign order:**

1. Bart's client integrates the feed and treats cockroach recoveries as out of scope; we fix bugs it finds, and other feedback is input to the new sync. [Sequencer recovery](2026-11-sequencer-recovery/README.md) is built: `setup --base <MACHINE_DIR>` rebuilds the sequencer from a canonical machine's exported state through `from_canonical_state` ([cockroach recovery](../recovery/cockroach.md#what-setup-does)), and each client realigns through the feed from its own count. Its devnet round-trip drills run in CI, and the rest is in its [to do](2026-11-sequencer-recovery/todo.md).
2. Application integrators/operators validate the private engine, its `from_canonical_state` and the recovery drill, and representative capacity before production use. Reference C-host lifecycle coverage is part of repository CI.
3. Track 5 (fee LUT) after a sync with Bart, batched with other guest-image changes.

Full restore archives support file and directory application prefixes.
Additional snapshot retention or transport mechanisms require a measured consumer need.

## Track 3 — Feed & replay protocol redesign

The egress API is implemented. The [history contract](../protocol/application-history.md)
and the [API contract](../../README.md#api) own its behavior; the
[register](../review/register.md#integration-work-owned-elsewhere) owns deployment follow-ups.

[Cockroach recovery](../recovery/cockroach.md) rebuilds the sequencer from a
canonical machine's exported state, and each client realigns through the feed
from its own count, so the sequencer serves no raw L1 inputs for a client to
replay. A new client sync follows that design. Until then, what it
replaces gets bug fixes only, and its docs are not polished:

- Reshaped or retired with the new sync: the WS wire format, the SDK
  snapshot-then-subscribe bootstrap, `/latest_snapshot` and its history headers,
  and `/history`'s baseline and accepted checkpoint. Within an era the
  history only appends
  ([history identity](../protocol/application-history.md#identity-and-available-history)).

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
process scenarios cover snapshot-to-live bootstrap, restart, and the rebuild
from genesis and from a stored machine.
Reference bridge conformance cannot establish private-engine correctness.

Remaining checks need the actual consumer:

- Supply the application's `from_canonical_state` and its `state` inspect
  answer, and the completed operator runbook. Require a recovery drill from a
  stored machine for production readiness: the old native state is
  unavailable, `setup --base` builds the engine from the machine's state, and
  execution after the rebuild matches the canonical machine, with directs
  queued at the stop block. For the DEX, pin the designated state drive/memory
  region. Add the integration check to the release validation once the actual
  artifacts are available; no generic trait or deployment gate currently
  enforces this requirement.
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
  transaction, the user-nonce counterpart of the era's `anchor_batch_nonce`. The point
  query also lets the shared execution boundary check the one-step nonce
  advance per op, as it checks progress. Both need C ABI exports and the
  engine owner's agreement.

Additional checkpoint primitives or asynchronous scheduling need a measured
requirement. Any future microbatch priority scheme must preserve per-account
nonce order, serial execution, and the frame-time contract; the current lane
does not promise priority scheduling.
