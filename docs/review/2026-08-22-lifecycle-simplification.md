# Lifecycle simplification review — L3 and the verdict-integrity fixes (2026-08-22)

Fresh-eyes review of what remained of the lifecycle machinery after L2
(2026-08-19, recorded in
[`2026-08-18-over-engineering-review.md`](2026-08-18-over-engineering-review.md)),
answering three maintainer questions: is the "journal plus three facts"
characterization true; is the journal needed; and what is the real scope of
L2's accepted trade "a known-terminal fault refuses at re-detection, not at
boot". Method: two exhaustive sweeps (a 28-class terminal-fault re-detection
map; a journal weight-and-consumers audit), then a six-refuter adversarial
verification pass over every load-bearing claim before acting. All decisions
below were made with the maintainer and landed the same day.

## Verified facts (the assessment's ground)

- **The L2 characterization was true.** Admission was governed by exactly
  three facts (kernel process lock; two-sided `setup_complete`;
  `canonical_divergence`), and the `runtime_lifecycle_journal` had zero
  production reads for decisions — its only two production reads were
  self-referential plumbing (setup's `RunId` read-back after
  `initialize_for_command`; `journal_command_of` deriving a settlement row's
  command from its own begin row). No consumer existed outside
  `sequencer/src`: not the e2e tests, the harness, the watchdog, or the
  SDKs. In `admission.tla` the `journal` variable was a ghost — a bijective
  mirror of `controller` that never independently gated a transition.
- **The journal's true weight** was ~490 production + ~413 test lines across
  ten files: the event vocabulary (`RunId`/`JournalEvent`/`FailureReason`/
  `LifecycleRecord`), five settle functions across three brackets, `run_id`
  threaded through `WorkersConfig`/`PreparedRuntime`/launch, and the
  `admits`-closure + `RunAdmissionOutcome` double-reduce in `admit_runtime`.
  Its unique value over tracing: the terminal-cause row (the runbook's
  postmortem channel) and the unclean-death tombstone.
- **The "refuses at re-detection" trade is far narrower than it sounds.**
  Everything with durable or deterministically re-derivable evidence
  re-refuses at boot before the first soft confirmation: divergence (four
  independent boot gates), misconfig (re-checked every boot; a *fixed*
  config boots cleanly with no residue — the self-healing ideal), boot-path
  corruption (≥9 boot-path opens), incomplete setup. The batch/frame spine
  is re-read by the danger detector within ~2 s of launch and by the
  submitter every tick. `POST /tx` structurally cannot ack before the lane
  completes `from_dump` + full replay + `open_state` (single ack site,
  token-gated). The verified residual: corrupt *payload bytes* at/below the
  lane's resume checkpoint (re-trip only via the WS feed — bounded by its
  ~50k-event catch-up window — or a pending batch's re-encode), and faults
  with no durable evidence at all, where serving again is correct unless
  the trigger recurs. Recorded as an explicit accepted residual in
  [`docs/threat-model/README.md`](../threat-model/README.md) (in-scope
  failure modes).

**Adversarial corrections recorded so they are not re-proposed:**

- "Missing safe head" and "missing open Tip" are **not** boot refusals:
  the first is an `L1ViewStale` retry, the second an `EnsureOpenTip`
  repair. Neither admits a soft confirmation; both are the self-healing
  design working.
- A claimed re-detection gap — `finalized_snapshot.inclusion_block` NULL
  boots to serving — was **refuted**: the column is `NOT NULL` with a type
  CHECK at the engine, both writers pass a `u64`, and boot decodes the
  column anyway. No boot assert is needed; none was added.
- The residual window is *smaller* than the initial fault map claimed:
  `frames`/`batches`/`submitter_frontier` below the checkpoint are re-read
  continuously at runtime (detector every 2 s; submitter frontier every
  tick), and the WS catch-up cap makes rows older than the window
  unreachable by the streaming path.
- During lane bootstrap, `/readyz` reports 200 and the WS feed serves
  committed history — no *new* soft confirmations, but the instance is
  externally visible before a lane-bootstrap refusal can land. Known and
  accepted; "refuses before serving" means before the first ack.

## L3 — journal narrowed to the terminal-fault black box (decided and landed)

**Principle adopted: telemetry writes must be verdict-neutral.** As long as
journal writes sat on the brackets' `?` paths, telemetry could change
verdicts — and the verification pass proved both instances below were real.
The terminal-cause recorder (already best-effort, already "gated on
nothing") is the shape the whole mechanism wanted to be.

Landed:

- `runtime_lifecycle_journal` (began/live/settled_clean/failed/terminal,
  `RunId`-correlated) → `terminal_faults` (command, cause, recorded_at_ms;
  append-only triggers kept). The command is caller-supplied — the old
  `journal_command_of` begin-row lookup is gone.
- All bracket settle plumbing replaced by one shared verdict-neutral
  `record_terminal_fault_best_effort` (commands/mod.rs); a failed record
  warns and never replaces the command's error. Setup's `complete_setup`
  fact write stays a hard `?` on the Ok path only — the completion fact is
  part of the command, not telemetry.
- `admit_runtime` collapsed to inspect (one consistent fact set) + reduce →
  mint a unit `RuntimeAdmission` witness. `admit_lifecycle_run_if`, the
  `admits` closure injection, `RunAdmissionOutcome`, and the rejected-facts
  double-reduce are gone. The linearization argument is now stated where it
  holds: the process lock excludes other processes, prepare is task-free,
  and launch is non-yielding, so no writer exists between the final read
  and worker launch (adversarially verified against every pre-admission
  task: fee-oracle bootstrap and reader syncs are sequential awaits).
- `WorkersConfig`/`PreparedRuntime` shed `run_id`; the fault recorder takes
  no correlation token; `begin_*`/`settle_*`/`mark_*`/`complete_*` and the
  `preflight_maintenance_flush` twin are deleted (flush preflights through
  the same two-sided fact check as every command).
- `initialize_for_command` no longer writes a journal row (its command
  still decides the history bases); `storage/lifecycle.rs` is now the three
  fact checks plus the black box (~240 production lines, from ~537).
- `admission.tla`: the `journal` ghost variable deleted (`AdmitLive` →
  `AdmitRuntime`; `JournalShape` → `ControllerShape`;
  `LiveCreationIsAtomic` → `AdmissionIsAtomic`). TLC re-verified with
  **identical counts** — 860 generated / 266 distinct / depth 13 / 0
  violations — confirming the bijective-mirror analysis.
- Docs synced: `invariants.md` (check policy + I18), `AGENTS.md`,
  `README.md`, `docs/recovery/README.md`,
  `docs/watchdog/operator-deployment.md` (postmortem query now targets
  `terminal_faults`), threat model (stale `RunId`-ack mitigation removed —
  that mechanism died with L2).

**Given up, eyes open:** the unclean-death tombstone (a SIGKILL now leaves
only process logs; a terminal death still leaves its black-box row), durable
non-terminal failure classifications (logs carry them), and the launch-time
"observes committed Live" test observable (replaced by the launch-count +
admission-witness pins).

## Verdict-integrity defects fixed (all pre-existing, none caused by L2)

1. **Settle masking (all three brackets).** `settle_*_lifecycle(...)?`
   propagated the settle step's own transient failure, discarding the
   command's verdict — a Terminal outcome could exit 1 and be restarted by
   a supervisor over a poisoned data directory. For most terminal paths
   (divergence via worker exit, chain-id mismatch, identity mismatch) the
   settle step was the *sole* exit-code determinant, with no containment
   backstop. Fixed structurally by the verdict-neutral recorder.
2. **`CommandError::Lifecycle(_)` unconditionally Terminal.** A transient
   `SQLITE_BUSY` on a lifecycle write exited 30 and paged, while recovery's
   own classifier treated the identical error as retryable. Now classified
   by variant: fact refusals (`NotAdmissible`/`CanonicalDivergence`/
   `Malformed`) are Terminal; `Storage` classifies by persistence like
   every other storage error.
3. **Signer misconfig exited 1, not 30.** Corrected scope from the
   adversarial pass: a malformed private key hit `CommandError::Io` →
   Unclassified in **all three** keyed commands (shared
   `verify_submitter_key` gate), not just `run`; and `run`'s worker-launch
   provider build separately bypassed `SignerMisconfig` for bad-URL
   failures. Both paths now classify `BootstrapError::SignerMisconfig` →
   Terminal (messages never echo key material; test-pinned).

**D4's open taxonomy question is closed by L2 + this change.** "Should
misconfig poison the lifecycle?" — there is no poison to apply anymore;
terminal now *is* "terminal-exit-without-poison" (exit 30, page, no durable
residue; a fixed config boots cleanly). What remained of D4 was exit-code
accuracy, fixed as defect 3.

## Verification

`cargo fmt` / `clippy --all-targets --all-features -D warnings` clean;
`cargo test --workspace --exclude canonical-test` fully green (550 sequencer
lib tests among 700); TLC on the rewritten `admission.tla` clean with
unchanged state counts.

**Post-landing adversarial review (same day; 3 lenses, 12 findings, each
independently verified): 8 confirmed / 4 refuted — one code regression, one
missing test pin, six doc/comment staleness items. All fixed same day:**

- **Regression: the Ok-path divergence refusal was dropped with
  `settle_clean`.** The old clean settlement's `refuse_on_canonical_divergence`
  meant a run that persisted divergence and then drained cleanly (SIGTERM
  landing inside the detector's 2 s poll window; the lane's biased shutdown
  check skips its frontier backstop during a drain) exited 30; the new
  bracket exited 0 — the one code that breaks the supervisor's
  restart-then-preflight rediscovery chain. Restored as an explicit
  Ok-path fact check (`refuse_divergence_on_clean_exit`, run bracket only —
  flush has no divergence writer), ordered before the black-box recorder so
  the verdict gets its row, and test-pinned. This is a fact check, not
  telemetry; the verdict-neutral principle is untouched. Severity: ops
  signalling only — no soft confirmation is ever issued over divergence,
  and the next `run` attempt still refuses at preflight.
- **Test pin added** for the `CommandError::Lifecycle` by-variant
  classification (busy → 1, persistent/fact refusals/divergence → 30).
- **Staleness fixed:** `docs/recovery/cockroach.md` (two baseline-journal
  mentions), `docs/snapshots/lifecycle.md` (an `Active(Starting)` remnant),
  the ADR's L2 amendment banner (now carries the L3 pointer),
  `recovery/mod.rs` and `commands/setup/mod.rs` doc comments,
  `runtime/shutdown.rs`'s six "journal" mentions, and
  `LifecycleError::Malformed`'s display (now "lifecycle contract violated",
  since `complete_setup`'s precondition refusal shares the variant).
