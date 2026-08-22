# Terminal Containment: Structural Consolidation — Proposal

**Status: partially landed; superseded by the authority-boundary ADR**
([`2026-08-authority-boundary-adr.md`](2026-08-authority-boundary-adr.md)).
Phases A and B landed and were reviewed 2026-08-01: four P1 findings showed
the replacement is not yet structural (containment linearization, streaming
cancellation, I15 trigger coverage, a missed subcommand gate). The
frame-independent fixes landed (containment CAS + bit-before-record,
incomplete-marker boot refusal, flush-mempool gate, divergence-before-F2);
the rest is owned by the ADR's cutover — Phase C and the chokepoint hoist
fold into it rather than landing in this frame. Produced by a structural
audit of the containment work (`aee7323`, `9705303`, `9d8543f`) after three
review rounds kept finding the same bug class.

**Historical implementation note (corrected 2026-08-01):** the landed interim
sets the containment bit and requests shutdown **before** best-effort marker
recording, because recording may block. It does not guarantee millisecond
drain or unconditional cross-crash recording. Current behavior is owned by
`docs/invariants.md`; the replacement lifecycle, structured runtime scope,
two-second terminal watchdog, and SQLite-centered lane-reconciliation boundary
are owned by the successor ADR.

## 1. Diagnosis: enforcement by discipline

The policy "persistent invariant failures are terminal and must not
externalize" is currently enforced at, by actual count: **23 raise sites**,
**7+ externalization-guard sites**, **10 classifier functions** (several
non-exhaustive), the `worker_exit_is_terminal` re-derivation, the exit-code
projection, and two hand-placed recovery-path checks. Almost every one is
*discipline*: omitting it compiles, passes tests, and silently degrades the
policy. The audit's concrete asymmetries prove the point:

- The snapshot streaming routes (`/finalized_state`, `/latest_snapshot`,
  `/finalized_state/inclusion_block`) take **no** externalization guard and
  no shutdown rejection — unlike `/subscribe` and `POST /tx`. Nobody decided
  that; nobody was forced to decide.
- `is_persistent_storage_error` is fail-open (`_ => false`) while
  `is_persistent_storage_open_error` is fail-closed — inconsistent defaults
  for the same question.
- `BatchPosterError::is_terminal_invariant` and
  `FlushError::is_terminal_invariant` use non-exhaustive `matches!` — a new
  variant silently classifies non-terminal — while the lane/feed equivalents
  are exhaustive matches.
- The lease-release fault reporter is `Option<…>` defaulting to `None`:
  silence unless the caller remembered to wire it.

Review rounds 1 and 2 each found an instance of one class — "a
mutation/admission point forgot to consult the terminal predicate" — because
the design makes every such point responsible for remembering. The class is
the bug.

## 2. The structural insight: classify at birth, not at death

Most of the machinery exists to carry a verdict *from detection to process
exit through a dying process*: the two-phase gate's publication ordering, the
`fault_cause` OnceLock, the cause strings at every raise site, the
drain-before-classify lease supervision, `FirstExit::StorageInvariantViolation`,
and `worker_exit_is_terminal` re-deriving at the supervisor what the raise
site already knew (an R5 "re-deriving a neighbor's answer" violation).

The repo already contains the alternative, audited and trusted:
`canonical_divergence` persists the verdict **in the detecting transaction**
and the *next boot* classifies by reading it. Classification at birth needs
no supervisor plumbing, no cause carried in RAM, no drain ordering — the two
"flattening" bugs the review rounds found are unreachable by construction.

The deeper unlock (audit finding G1): today **storage cannot raise the fault
itself** — raising means touching runtime shutdown types, hence the injected
reporter callback. A DB/FS-resident marker removes that constraint: fault
recording hoists into the `Storage::read`/`Storage::write` chokepoints, and
a *new call site inherits containment for free*. That single change answers
the reviewers' actual complaint.

## 3. The audit's one disagreement, adjudicated

The marker report recommends marker-primary containment for the whole fault
class; the schema report rejects generalizing the marker beyond
durable-facts-about-durable-state, with three objections. Resolution:

1. *"Most faults mean the DB is broken, so a DB write can't be primary."*
   Answered by ordering the ladder **filesystem-first**: an fsync'd marker
   file beside the DB (+ parent-dir fsync) depends on nothing that just
   failed; the DB row is rung 2, kept as the queryable audit record.
2. *"Read-path detections have no detecting transaction; the marker write is
   a second linearization point."* True — which is why one bit of the gate
   survives: `storage_invariant_closing` is set *before* the marker write,
   and the process **aborts at detection** rather than shutting down
   gracefully. The in-process exposure window collapses from unbounded (a
   path that never checks, serving forever, across restarts) to
   marker-write latency, once.
3. *"Over-stickiness: persisting 'terminal' makes restart-recoverable
   conditions permanently unbootable."* Exit-30 conditions are *defined* as
   operator-required (R4); today their stickiness is delegated to
   orchestrator config, which Kubernetes ignores — a serving-path fault
   under k8s currently yields crash-loop-with-service-in-between. The
   marker makes stickiness self-enforced (strictly closer to "authority
   stays with startup's own check") and a `clear-terminal-fault` subcommand
   is the designed exit.

Verdict: **hybrid** — marker-primary for process-level containment, schema
triggers scoped to where the detecting-transaction property genuinely holds
(divergence today; I13 if later promoted), single closing bit retained.

## 4. Proposal, phased

**Phase A — independent hardening (small; do regardless of the decision):**
- I15 becomes structural: `BEFORE INSERT/UPDATE` freeze triggers on
  `batches`, `finalized_snapshot`, `pending_snapshots` gated on the
  divergence marker (T1–T4 in the audit; the existing
  `trg_batch_tree_anchor_write_once` idiom). The hand-written pre-cascade
  check demotes from load-bearing to typed-error surface.
- Close the audit's asymmetries: shutdown/guard posture for the snapshot
  routes (decided, not defaulted); consistent classifier defaults;
  exhaustive matches for `BatchPosterError`/`FlushError`.

**Phase B — the restructure (the decision):**
- `terminal_fault` marker: fsync'd file (rung 1) + DB row (rung 2, archive
  on clear), written at detection; plain `abort()` as rung 3 with an
  unmistakable log line, documented as a bounded weakening.
- Keep only `storage_invariant_closing` + the existing guard-shaped
  early-returns (bool check, no lock). Abort at detection — graceful
  shutdown is what re-created the round-2 bug class.
- Boot refusal *before* identity load, so no code path can mutate ahead of
  it. Exit 30 with the stored cause.
- `clear-terminal-fault --acknowledge <cause-digest>` subcommand; refuses
  while a divergence row exists; archives to `terminal_fault_history`.
- Hoist fault recording into `Storage::read`/`Storage::write` (G1). The ~6
  `catch_unwind`/join sites remain for the panic class.
- Delete: gate phase two (RwLock, publication, `externalize_until_shutdown`,
  the ordering test), drain-before-classify lease supervision (startup
  `reset_dump_leases` is already the crash backstop), cause plumbing,
  `FirstExit::StorageInvariantViolation`, `worker_exit_is_terminal` as
  authority, the defensive admission/exit special cases. Net: roughly
  **−400 production / −700 test lines against +200 / +275**, and the
  audit surface becomes: one marker writer, one boot refusal, one bit.
- `docs/invariants.md`: the containment bullet is rewritten from an
  in-process ordering claim ("terminal publication is an externalization
  barrier") to a cross-restart claim ("a persistent invariant failure is
  durably recorded before the process dies; the next boot refuses on it"),
  plus a new I-entry for the marker.

**Phase C — startup as check→act→re-check:**
- `loop { match check_danger { Safe => admit, Refuse-class => refuse,
  Repairable => run repair } }` with an iteration bound and refuse-on-repeat.
  Subsumes `authorize_worker_admission` and the repair-vs-admission
  two-question split structurally. Post-restructure trust statement, per the
  audit: *the process reaches `Workers::spawn` only via `Admit`, produced
  only by `Safe`, produced only by `check_danger`* — three names, one file.

**Explicitly kept (not puxadinhos):** `is_persistent_storage_error` (the
question is irreducible; only its default gets fixed), `external_u64_to_i64`,
the sub-block clock policy, the lease-guard-arms-after-commit fix, the
catch_unwind sites, and the WS `send_ws_message` chokepoint shape.

**Deferred:** generalizing the freeze-trigger predicate to `terminal_fault`
(revisit if I13 is promoted to a marker); any blanket trigger coverage
(rejected: ~24 triggers, hot-path cost, clear-command has to write through
its own freeze).

## 5. Costs and open questions

- Phase B tears out three rounds of *reviewed, tested* machinery; the churn
  argues for doing it now, before the WS/dump implementation tracks build on
  the current shape, or not at all.
- Lost with the gate's phase two: the graceful drain of in-flight acks/sends
  before publication. The audit's analysis: that is a liveness nicety, not a
  containment property — a truncated ack is indistinguishable from an
  ordinary crash between commit and ack, which the system already tolerates
  by design (durable commit + nonce checks).
- Orchestrator contract: rung 3 exits 134 (SIGABRT) and classifies at the
  *next* boot; R4 already blesses this ("the exit code is an ops hint;
  authority stays with startup's re-check"). Ops docs must note both.
- Open: `clear-terminal-fault` UX (digest acknowledgement?); whether a
  best-effort WS "goodbye" flush before abort is wanted; k8s runbook.
