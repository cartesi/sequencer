# Containment ADR review — findings and dispositions (2026-08-01)

Two review rounds on the terminal-containment cutover branch, both by the
same P1 reviewer: (1) the code review of `9705303..e9cdf20` that held the
branch and produced the four-abstraction recommendation, and (2) the
ADR review of `7590948` (the frame-independent fixes + the
authority-boundary ADR draft). Round-1 outcomes are summarized in the ADR's
Context; this ledger records round 2's findings and where each landed. The
ADR itself ([`../plans/2026-08-authority-boundary-adr.md`](../plans/2026-08-authority-boundary-adr.md))
carries the amended design and the decisions ratified after that review.

**Verdict (round 2, historical): hold `7590948`.** The architectural turn was
accepted. On 2026-08-01 the maintainer took the cutover branch forward and
initially kept it non-deployable pending the proposed `LiveKernel` authority
cutover. The current decision is recorded in the ADR; this ledger preserves
how it changed after review.

**Cutover update:** ADR step 2 is now implemented. The append-only lifecycle is
the sole boot authority and the marker file plus terminal-fault tables were
deleted. Exact acknowledgement uses a fresh opaque 128-bit command token rather
than a per-database counter, preventing delayed acknowledgements from aliasing
after a database replacement. The A1/A4 marker dispositions below describe the
safe intermediate that existed when this review closed, not mechanisms still
present today.

**Step-3 update (landed and verified):** normal `run` startup now replaces the
monolithic recovery dispatch and separate post-repair gate with one pure
reducer. It performs at most one phase per inspection, carries the flush
safe-block observation as an ephemeral boot-attempt witness, and crosses the
`AdmittedRuntime` prepare/admit/launch boundary only after a fresh clean
decision atomically commits `Active(Live)` and returns its unforgeable witness.
The admission model checked 363 distinct states at depth 13; the existing
preemptive slot model was rechecked with no violations. A production-shaped
E2E also pins warm admission from fresh persisted facts while L1 is offline.
This step does not migrate maintenance flush, initial setup/rebuild, or
`setup --recovery`. At that historical step it did not implement `EraId` or
`RecoveryGeneration`; the later step-6 metadata slice described below now
does.

**2026-08-02 design re-evaluation:** the maintainer rejected the unimplemented
`LiveKernel`/reader-mailbox design after re-examining the actual completeness
and cost boundary. R2 completely detects one narrow predicate—foreign or
byte-different at/above-anchor landings accepted by the mirrored scheduler—but
is not a general canonical/application divergence oracle, and its repair is
manual cockroach recovery. SQLite remains the durable component-coordination
plane; the reader retains the atomic safe-input/head/frontier/R2 transaction.
Revised step 4 keeps prompt process-wide reaction in `DangerDetector`. The
inclusion lane's existing time-gated SQLite read now opportunistically returns
typed poison instead of a usable frontier; it deliberately adds no marker read
to every latency-critical user-op chunk. This supersedes the original A2
disposition and actor-specific amendments below without changing the
reproduced fact or the historical review verdict.

The same re-evaluation found one prerequisite that the original actor review
did not name: the pre-step-4 inner drain stopped on queue-empty or
included-batch bytes, but rejected requests consumed dequeue attempts without
growing that byte target. A continuously replenished rejected queue could
therefore starve the next frontier check. Revised step 4 makes one existing
bounded dequeue chunk the fast-turn limit. This is an outcome-independent count
boundary, not a timer or resumable slow-work scheduler. Step 4 also settles the frame clock:
an open projection reconciles once the observed safe head is five blocks beyond
the open frame, jumping directly to the observed tip and draining zero or more
accumulated directs.

**Step-4 update (landed and benchmarked):** the lane now processes one durability chunk per
fast turn; `SafeFrontierState` withholds an open frontier when R2 poison exists;
and logical frame time advances at a five-safe-block delta with no synthetic
catch-up frames. Focused tests pin a full reject-only chunk leaving queued work,
poison precedence at zero delta with closed intake, below/exact-threshold direct
handling, an empty 5→32 jump, and reset-to-tip behavior. A saturated reject-flood
test additionally proves that the live outer loop reaches a due poisoned
frontier. Same-host 30-second release sweeps against pre-step-4 `02fabb0` found
no material accepted-path regression: current ACK p99 remained at or below
49.253 ms through concurrency 256, concurrency 128 sustained 7,984 tx/s, and all
loads had zero rejections. The ADR records the full method and comparison.

**Step-5 update (landed):** the proposed generic command-controller
consolidation was rejected after auditing the actual fact boundaries.
Setup/rebuild, maintenance flush, and exact acknowledgement remain separate
typed protocols; combining checkpoint/genesis, wallet, and run-danger facts
would enlarge the state machine without closing an enforcement hole. The
concrete seams were fixed instead: genesis construction now occurs only after
plain-setup admission and its refusal gates; maintenance may be retried from a
setup-complete recovery-gated state while preserving the prior verdict; and
lifecycle settlement consumes the semantic failure classifier rather than its
numeric exit code. Terminal containment now arms a process-lock-lifetime-aware
two-second abort watchdog before cancellation or audit recording. Cleanup polls
all workers concurrently so a hung component cannot hide the terminal exit
that must arm the bound. Ordinary operator and expected-recovery shutdown stay
graceful.

**Step-6 storage/execution update (landed):** initial setup/rebuild now creates the
baseline schema, a UUIDv4 `EraId`, `RecoveryGeneration = 0`, and initial
lifecycle `Active(Starting)` atomically. Standard recovery increments the
generation exactly once in its cascade transaction iff at least one valid
batch is invalidated. Rebuild starts with `base_executed_input_count = NULL`;
cockroach fill binds folded `K = S'.executed_input_count()` atomically with the
initial finalized snapshot, and setup completion refuses until both exist.
This intentionally did not implement automated DB replacement: the supported
model is an explicit fresh/wiped-directory rebuild, with no clone detector,
distributed fence, or partial-fill resume state machine. A retained early
incomplete DB reuses its unexposed era; a fail-loud partial-fill refusal
requires wipe/retry and mints another unexposed era. `K` is separate from
physical `l2_tx_index` cursor padding. The scheduler count audit, canonical
per-input SQLite attribution, invalidation rewind/reuse, snapshot count, and
catch-up verification are also landed. `GET /history-version`, replay
endpoints/gold-boundary projection, and WS/API projection remain deferred; the
current public rowid feed is unchanged.
Canonical attribution adds chunk-local SQL inside the existing FULL commit but
no new transaction or fsync. The 2026-08-03 repeat of the step-4 same-host ACK
sweep completed with zero rejections and at most 56.963 ms p99 across
concurrency 1/64/128/256, satisfying the hot-path gate. Round-trip measurement
remains paired with the later public history/API projection.

## Current-code findings

| # | Sev | Finding | Disposition |
|---|---|---|---|
| A1 | P1 | Marker protocol neither power-loss durable nor fail-closed: temp directory entry never synced before rename (a power cut could leave *neither* name durable); `read_dir`/entry errors silently treated as "no marker"; temp deletion failures ignored on clear; cross-process `exists()`/`rename()` race. | **Fixed, then superseded.** The intermediate marker was made power-loss durable and fail-closed under the OS lock. ADR step 2 then deleted both marker rungs: schema creation + first `Active` are atomic, stale `Active` is unconditional boot refusal, and exact-run acknowledgement is append-only. Structured lock-retention and prepare/launch tests remain. |
| A2 | P1 | Known-divergence hot-path window live: the reader commits `canonical_divergence` and returns success; until the detector's 2 s tick, the lane can persist and acknowledge user ops (hot-path tables not trigger-covered — reproduced). | **Historical policy superseded; revised step 4 landed 2026-08-02.** The reproduced window is accepted because R2 is a narrow content-identity backstop and soft confirmations are rollbackable. Detection stays in the reader's atomic SQLite sync; the persisted frontier/tree/promotion freeze is immediate; `DangerDetector` owns prompt shutdown. The lane's existing time-gated read opportunistically returns typed poison before reconciliation, with no per-user-op query or timing claim. A turn that already read `Open`, or a chunk authorized before runtime observation, may complete. The watchdog does not subsume R2 because promotion freeze normally leaves its finalized head unchanged. P1.2 remains separately reclassified: streams are immutable operator-only reads outside the authority-bearing policy. |
| A3 | P2 | Divergence-over-F2 test non-discriminating: `flush_observed == resynced == 1200`, so an F2-first implementation also passed. | **Fixed 2026-08-01**: `flush_observed = 1201` with persisted divergence — both refusals now apply and the test proves divergence outranks F2 (terminal exit 30, no tree mutation). |
| A4 | P2 | Docs materially inconsistent: `invariants.md` + `shutdown.rs` module docs claimed marker-before-bit and unconditional durable refusal; implementation is (correctly) bit-before-recorder with best-effort rungs. | **Fixed, then superseded.** Current docs retain bit-before-recorder for in-process closure, but durable refusal now comes from pre-armed `Active`; a failed `Poisoned` append leaves that stale authority record intact. Marker-specific boot/clear prose was removed. |
| A5 | P2 | Track 6 recorded false provenance: the hardlink-prohibition promotion claimed an "explicit maintainer instruction"; the actual instruction was remarks-only. | **Fixed 2026-08-01**: §4 `clone_image` text restored to the original design, §10 restored to non-normative remarks with a correction note; the tracks doc's cross-reference now says "disputed", not "rejected". Hardlink validity gets settled in design review with Bart. |

## ADR amendments (2026-08-01, with the dated 2026-08-02 refinement above)

- Unified policy restated around **authority-bearing** mutations/promises;
  previously-authorized effects may complete; containment/audit writes and
  immutable operator reads are out of scope; new commands require exclusive
  process ownership + lifecycle admission.
- **Exclusive data-directory process lock** added (and landed ahead of
  cutover): `Active` is persisted state, not a lease; only an OS-held lock
  distinguishes a live owner from a stale one.
- **Stale `Active` ratified operator-sticky**; acknowledging the exact stale
  record transitions to `NeedsRecovery`, never directly to `Ready`.
- **Retryability ≠ cleanliness**: only proven-clean exits reach `Ready`;
  unknown/side-effecting failures stay `Active` or become `NeedsRecovery`.
- **`NeedsRecovery` distinct**, never a replay permit: recovery re-derives
  from fresh facts and transitions to `Active { run_id, phase: Starting }`
  first; command kind is persisted audit metadata, not lifecycle state.
- **Phase-granular normal-run recovery**: `Flush → inspect → Sync → inspect →
  Cascade`, each mutation asserting phase facts and no divergence. The flush
  observation is an ephemeral witness for one boot attempt, not a persisted
  recovery-phase state machine; a restarted attempt may flush again.
- **Minimality re-evaluation (amended 2026-08-02)**: structured task ownership
  + the OS lock + fresh channels remove the need for a globally threaded
  `RunEpoch`. SQLite-backed, role-local authorization boundaries remove the
  need for both the separate mutex `EffectGate` and the later-proposed
  `LiveKernel` actor.
- **External history accepted as**
  `HistoryVersion = (EraId, RecoveryGeneration)`. `EraId` changes on
  cockroach rebuild; generation changes exactly once iff standard recovery
  invalidates a valid batch; clean restarts change neither coordinate. The DB
  storage/execution foundation is now landed, while protocol projection
  remains open.

## Remaining work

1. Bart: Track 6's dump/image design. Track 3's Bart-dependent `/inputs` and
   consumer-visible current-era bootstrap decisions are incorporated into its
   ordered handoff; Track 6 owns the artifact representation that handoff may
   choose to serve.
2. Execute the
   [Track 3 ordered handoff](../plans/2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff)
   over the landed canonical history foundation.
