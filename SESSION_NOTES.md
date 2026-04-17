# Session Handoff — 2026-04-16

A short note for the next agent (or your future self) picking up work in this
worktree. Ephemeral: delete after absorbing.

## TL;DR

The branch is clean, green, and ready to commit. The staged security review
(Parts 1-8) found 4 vulnerabilities + 8 hardening items; all were fixed and
locked in with regression tests. The test harness gained a programmable
TCP proxy and a DB-level wall-clock rewind helper. The zone × outage matrix
has 4 of 7 cells covered end-to-end. A real structural bug in the
danger-check path was caught while writing the wall-clock e2e test and
fixed by splitting zombie-detection from any-unresolved-batch detection.

## State of the tree

- `cargo check` / `cargo fmt --all --check` / `cargo clippy --all-targets --all-features -- -D warnings` — all clean.
- `cargo test --workspace --exclude canonical-test` — all passing (~200 tests).
- `just test-rollups-e2e` — 16/16 passing (~53s).
- Uncommitted changes: `git status --short` shows 13 modified files (the
  refactor + tests) and 2 untracked files (`SECURITY_TODO.md` and
  `feature-recovery-old-origin-markdown-recovery-2026-04-15.md`). Commit when
  ready.
- Untracked files worth reviewing before commit:
  - [`SECURITY_TODO.md`](SECURITY_TODO.md) — **keep**. All findings now have
    action items checked off; the file is living documentation for the
    review.
  - [`feature-recovery-old-origin-markdown-recovery-2026-04-15.md`](feature-recovery-old-origin-markdown-recovery-2026-04-15.md) —
    **safe to delete.** Its content was absorbed into
    [`AGENTS.md`](AGENTS.md) during the docs rewrite.

## What this session did

High level, in order:

1. **Staged security review** (Parts 1-8) — scheduler, sequencer-core, fee
   model, ingress, L1, recovery, storage, egress/runtime/config. Findings
   collected in [`SECURITY_TODO.md`](SECURITY_TODO.md). Threat model
   formalized in [`docs/threat-model/README.md`](docs/threat-model/README.md).
2. **Docs rewrite** — [`AGENTS.md`](AGENTS.md), [`CLAUDE.md`](CLAUDE.md),
   [`README.md`](README.md). Absorbed content from the recovered
   `feature-recovery-...md`. Added the L1 block-time coupling assumption to
   the threat-model doc.
3. **All 12 security findings fixed.** One finding (wall-clock
   `unwrap_or(0)` masking `l1_safe_head` corruption, §7.3 equivalent) led to
   the open-batch staleness gap discovery — another real bug.
4. **Phase 1 regression tests** — 19 new unit/integration tests locking in
   the security fixes. See [`tests/TEST_PLAN.md`](tests/TEST_PLAN.md) for
   the full matrix. One of the H4 tests caught a real latent bug in the
   H4 fix itself (bracket-wrapped IPv6 literal in `host_str()`).
5. **Phase 2 tooling + zone matrix** — built `tests/harness/src/proxy.rs`
   (TCP proxy with `disconnect`/`reconnect`) and
   `ManagedSequencer::rewind_synced_at_ms` (DB-level wall-clock rewind).
   Covered §11.1.1 / §11.1.2 / §11.1.3 / §11.2.3 — 4 of 7 zone × outage
   cells.
6. **Danger-check unification bug** — while writing the wall-clock e2e
   test, discovered that `check_danger_zone` and `detect_and_recover` were
   asymmetric (closed-only vs closed+open). The first unification attempt
   broke the live submitter (restart loop on aging open batches). The
   landed fix splits the public API into two explicit semantics:
   `check_danger_zone` (zombie-only) and `check_any_unresolved_batch_in_danger`
   (unified). See the refactor notes in
   [`tests/TEST_PLAN.md`](tests/TEST_PLAN.md) Phase 2 lessons.

## Where the work stopped

Everything in-scope is documented. Specifically:

- [`tests/TEST_PLAN.md`](tests/TEST_PLAN.md) lists every remaining scenario
  with `[ ]`, `[!]`, `[?]`, or `[-]` status. Phase 1 and Phase 2 open items
  are called out at the top under "Recent regression work."
- [`SECURITY_TODO.md`](SECURITY_TODO.md) has all fixes checked off. No
  outstanding vulnerability work.
- One deferred design review recorded in TEST_PLAN: TLA+ spec alignment
  with the danger-check split — does `preemptive.tla` model the
  zombie-vs-aging distinction, or is it the same unification flaw we just
  fixed in code?

## The one design question worth tackling next

**Aging open batch in the danger zone, during *live* operation (L1
reachable). NOT the same as the wall-clock fallback gap — that one is
fixed.**

What the refactor DID fix:
- `check_any_unresolved_batch_in_danger` (wall-clock fallback) now sees
  open batches. ✓
- `detect_and_recover` at startup cascades open batches that are past
  `MAX_WAIT_BLOCKS`. ✓ (this was the §7.3 security-review fix, now
  subsumed by the unified helper)
- The asymmetry between preemptive-check and cascade-check is gone. ✓

What the refactor did NOT fix — the scenario still open:

- L1 is reachable (so the wall-clock fallback doesn't run).
- Open batch ages past `danger_threshold` (default 1125 blocks).
- Open batch is NOT yet past `MAX_WAIT_BLOCKS` (default 1200).

In that ~75-block window (≈15 min at 12s/block):

- `check_danger_zone` (submitter tick, closed-only by design) returns
  None → no flush, no shutdown.
- `detect_and_recover` only runs at startup, and uses `MAX_WAIT_BLOCKS`
  as the threshold — wouldn't cascade even if it did run.
- The batch continues accepting user ops and issuing soft confirmations
  for a batch that's 15 minutes away from being auto-skipped by the
  scheduler if it doesn't land in time.

When the batch finally closes (via policy) and gets nonced, the next
submitter tick sees closed-batch-in-danger → flush + shutdown → restart →
`detect_and_recover` at `MAX_WAIT_BLOCKS` cascades. By then some of those
window soft confirmations may be doomed.

In practice this window is short or empty under normal batch policy
(`max_open_time ≪ danger_margin`). But it's a real latent issue.

**Three candidate design responses, in increasing invasiveness:**

1. **Accept it.** Under normal batch policy
   (`max_open_time ≪ MAX_WAIT_BLOCKS`) this shouldn't happen; document the
   invariant and rely on it. Simplest, but leaves the latent gap.

2. **Proactively invalidate aging open batches at recovery.** Change
   `detect_and_recover` to invalidate the open batch if it's past
   `danger_threshold` (not just `MAX_WAIT_BLOCKS`). Safe because the open
   batch was never submitted — no zombie risk. Trades off: we invalidate
   soft confirmations earlier than strictly necessary.

3. **Force-close the open batch from the submitter.** When the submitter
   detects open-batch-in-danger, signal the inclusion lane to force-close
   the current batch so it can be submitted. Prevents the gap cleanly
   but needs new cross-component communication.

My instinct is (2) — it's the smallest change that closes the gap and
matches the existing "cascade on restart" pattern. (3) is arguably cleaner
architecturally but much bigger scope.

Before implementing any of them, **read `docs/recovery/preemptive.tla`
with this lens**: does the spec model "open batch aging while L1 is
reachable"? If so, what's the prescribed response? The answer informs
which option to pick.

## Recommended priority order for the next session

1. **TLA+ spec review** — read the spec with the zombie/aging split in
   mind. Confirm or refute the alignment. ~1h. Unlocks the design
   decision for #2.
2. **Aging-open-batch design fix** — pick (1), (2), or (3) above based on
   the spec review, implement, add e2e coverage. Medium scope.
3. **§11.1.4 — closed+submitted batch past-stale** — needs `--no-mining`
   support in the harness (T2). Medium scope. Covers a code path none of
   the current tests exercise (closed-batch zombie + recovery).
4. **§11.2.1 / §11.2.2 — provider outage in pre-danger and danger zones** —
   needs the proxy (already built) plus `--no-mining`. Small scope once
   T2 is in.
5. **§7.8.2 first-boot-with-L1-down** — small harness extension (pre-spawn
   L1 override) + one e2e test.
6. **H1 failpoint** — the one outstanding hardening regression (rusqlite
   error leak). Needs failpoint injection tool. Small scope once the
   mechanism exists.

Everything else in TEST_PLAN is lower-value or already `[x]`/`[!]`/`[?]`
with adequate notes.

## Context a new agent will need

Must-reads before touching anything:

- [`AGENTS.md`](AGENTS.md) — architecture, duality, recovery, invariants.
  Start here if you're unfamiliar.
- [`docs/threat-model/README.md`](docs/threat-model/README.md) — what's in
  and out of scope for security-adjacent work.
- [`docs/recovery/README.md`](docs/recovery/README.md) — recovery design;
  the TLA+ spec lives next to it.
- [`tests/TEST_PLAN.md`](tests/TEST_PLAN.md) — 14-section scenario matrix
  with status markers. Canonical source for "what's tested and what isn't."
- [`SECURITY_TODO.md`](SECURITY_TODO.md) — closed findings; useful as
  reference for the fix patterns.

## Things I'd do differently

- **Run `just test-rollups-e2e` earlier and more often.** Two of my tests
  had bugs that only surfaced at e2e level (nonce-state assumption and
  wall-clock semantic). Desk-checking is a weaker signal than green tests.
- **Surface design questions before implementing fixes.** The danger-check
  unification should have been discussed before the first attempt; the
  naive "just unify" was wrong because the two callers wanted different
  semantics. Would have saved one bad refactor + rework cycle.
