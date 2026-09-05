# Lifecycle simplification review — L3 (2026-08-22)

Fresh-eyes review of what remained of the lifecycle machinery after decision
L2 (admission gating removed, 2026-08-19). Method: two exhaustive read-only
sweeps (a 28-class terminal-fault re-detection map; a journal
weight-and-consumers audit), a six-refuter adversarial verification of every
load-bearing claim before acting, and a fifteen-agent post-landing review of
the diff.

**Findings that grounded the decision:**

- The L2 characterization was true: admission was exactly three facts, and
  the attempt journal had zero production reads for decisions — its ~490
  production lines across ten files bought only what tracing already
  provided, plus the terminal-cause row. In `admission.tla` the journal
  variable was a bijective ghost of the controller (removing it left TLC
  state counts byte-identical).
- The "terminal faults refuse at re-detection, not at boot" trade is far
  narrower than it sounds: everything with durable or deterministically
  re-derivable evidence re-refuses before the first soft confirmation, and
  the batch/frame spine is re-read within seconds of launch. The verified
  residual (cold payload bytes below the lane checkpoint, reachable only via
  the WS catch-up window or a pending batch's re-encode; faults with no
  durable evidence at all) is recorded as an accepted boundary in the threat
  model.

**Decision L3 (landed):** the journal narrowed to the `terminal_faults`
black box — append-only command+cause rows, best-effort. Principle adopted,
now in the invariants check policy: **telemetry writes are verdict-neutral**
— they sat on the brackets' `?` paths and could change exit codes.
`admit_runtime` collapsed to one consistent inspect + reduce; `RunId` and
the event vocabulary went with the settle plumbing. No boot machinery was
added in the journal's place: neither a durable verdict gate (it needs an
acknowledgement to exit, which carries no information the reducer doesn't
re-derive) nor a boot-time full-integrity sweep (expensive, and blind to
semantic violations outside its read set).

**Verdict-integrity defects fixed with it** (each adversarially confirmed
first): settle-masking (a settle-step failure could replace a terminal
verdict with exit 1 — and settle was the *sole* exit-code determinant for
most terminal paths); `CommandError::Lifecycle` classified wholesale
terminal (a transient `SQLITE_BUSY` on a lifecycle write paged); signer
misconfiguration classified as unclassified I/O in all three keyed commands.
The misconfig-poison taxonomy question from the 2026-08-18 review closed
with L3: there is no poison to apply; what remained was exit-code accuracy.

**Post-landing review (8 confirmed / 4 refuted):** one real regression — the
Ok-path divergence refusal had been dropped with `settle_clean`, letting a
clean drain over freshly persisted divergence exit 0 (the one code that
breaks the supervisor's restart-then-refuse rediscovery chain); restored as
an explicit fact check and test-pinned. One missing test pin added; six
doc-staleness items fixed. A claimed re-detection gap at
`finalized_snapshot.inclusion_block` was refuted (the register's L3 block
has the entry). Lesson recorded: when
deleting a mechanism, sweep for its *vocabulary* with review agents, not a
bare grep — one grep pipeline silently returned empty on files that
contained the pattern.

All landed in the squashed branch commit; per-item dispositions in
[`register.md`](register.md).
