# Over-engineering review (2026-08-18)

Full-branch review of the authority-boundary + durable-history-foundation
branch against the project's design goals (readable, auditable, every
mechanism judged against its weight): seven parallel subsystem reviews, each
proposal adversarially cross-examined against the invariants register, the
TLA+ models, and git history, plus an independent premise challenge of the
ADR. 141 mechanisms inventoried: 98 keep, 25 simplify, 6 cut, 12 question.
This review adopted the calibration rule now in AGENTS.md (the complexity
budget belongs to concurrency, mutual exclusion, durability, and hostile-L1
robustness).

**Verdict: not over-engineered — unevenly engineered.** The ADR's premise
survived attack; the rejected alternatives left no residue in code. Three
findings cut against "smallest sufficient design": the in-process containment
predicate was still convention at ~11 hand-placed sites (fixed by the
`Authorized` token), ~700 lines of mechanical repetition had accumulated
(harvested), and the lifecycle module implemented the right guarantee with a
heavier representation than needed (re-platformed, then removed — see below).

**Outcomes** (all landed 2026-08-18/19, each wave adversarially re-reviewed
post-commit; per-item dispositions in [`register.md`](register.md)):

- Eleven defects fixed (fail-open classification arms, admission bypasses,
  containment publication window, snapshot-route gating, typed app-boundary
  refusals).
- ~700-line harvest: worker-exit plumbing collapsed with terminality beside
  each error type; a real `RuntimeScope` with `ShutdownSignal` reduced to its
  name; fee-oracle bootstrap behind its module; recovery type-stack trimmed
  to the one `RecoveryProgress` enum; dead surface deleted.
- The `Authorized` externalization token: the containment consult became a
  compile-time obligation of the effect functions.
- Module homing: command *brackets* to `commands/` (with their config/error
  taxonomy, `RunError` → `CommandError`), the capability substrate alone in
  `runtime/`, `L1Config` to `l1/`, the clock to the crate root.
- **Lifecycle arc:** re-platform to singleton + audit trail (decision L1) →
  admission gating removed entirely, facts govern (decision L2, 2026-08-19)
  → journal narrowed to the terminal-fault black box (decision L3, see
  [`2026-08-22-lifecycle-simplification.md`](2026-08-22-lifecycle-simplification.md)).
  The surviving design rationale lives in the ADR and the invariants check
  policy.

Still open from this review: the 500 ms latency contract does no design work
(nothing is shaped by it — decide what it is *for*), and the
catch-up ACK-latency measurement owed to the benchmark harness. Tracked in
the register.
