# Branch stock-take (2026-09-03)

Stock-take of the authority-boundary branch (PR #28, seven commits on
`f59ec25`) before it leaves draft, asked as: what is this branch for, is it
over-engineered, what next. Method: first-hand reads of the runtime, command,
recovery, lifecycle, and history code; then a read-only fleet of seven
subsystem lenses and five premise challengers (threat model, minimal design,
refuted-list audit, CI root cause, roadmap); then three adversarial refuters
per proposal for the eighteen highest-ranked proposals. Every proposal the
fleet raised is recorded here, including the ones that were not put to a
jury. Per-item dispositions that are actionable now live in
[`register.md`](register.md) (findings 19–31 and the 2026-09-03 refuted
block); this ledger is the full record and will be distilled when it closes.

**How to read the status tags.**

- **confirmed** — put to three refuters; at most one refuted it. The recorded
  text includes the jury's amendments, which are load-bearing.
- **refuted** — put to three refuters; at least two refuted it on code truth
  or on a registered invariant. The reason is recorded so it is not
  re-proposed without new evidence.
- **unverified** — raised by one reviewer and not adversarially checked.
  Treat as a reviewer's claim: re-verify the cited lines before acting.
- **verified first-hand** — checked directly against the tree during the
  stock-take, independent of the fleet.

## Landed

Wave 1 (2026-09-03), one theme per commit, on top of the CI fix:

- **CI red**: both wallet-sequencer binaries style logs only when stdout is
  a terminal; the aging-tip scenario asserts exit code 10 instead of grepping
  the rendered log (verified: the scenario passes and its log carries no
  escape bytes).
- **Prose matches the types**: the token's true scope in `authorize()`, the
  ADR, and the register; "boot-local" for the flush witness; the lock
  witness's real predicate; three "journal" remnants; the error module's
  dated history and removed command; a module doc that argued instead of
  instructing; the recovery README's nonexistent type name; a test renamed to
  what it asserts. Closes finding 25.
- **Codename sweep finished**: fifteen residual review codes replaced by
  the reason or the invariant id.
- **Test-only storage surface gated**: `ensure_open_tip`,
  `close_frame_and_batch`, `latest_batch_index`, `ordered_l2_txs_for_batch`,
  `promote_finalized` are `#[cfg(test)] pub(crate)`, with their doc links and
  the snapshot lifecycle doc pointed at the production paths. Closes findings
  17 and 21 and the second half of 10.
- **Two register nits**: the flusher's healthy retry logs at `warn!`
  (finding 4); `fixed_mul`'s comment states what the truncation relies on
  (finding 8's comment half).
- **`/tx` 500 body is fixed text**: the application's reason stays on the
  lane error and the log (finding 5).
- **Panicking progress constructor deleted**: `ApplicationProgress::new`
  had only test callers; `try_new` is the one constructor, tests use it with
  `expect`.

Wave 2 (2026-09-04), the containment diet, each commit refuted by three
read-only reviewers before landing:

- **Containment writes nothing durable**: the in-scope fault recorder is
  deleted; the command bracket's settlement write is the black box's one
  writer, so a contained run records one row. The accepted loss (any death
  before settlement leaves only the process logs) is stated in the runbook.
  `run` logs the last black-box row once at startup, ahead of the preflight,
  so the table has its first in-product reader. Closes finding 24.
- **Finalized lease is non-optional**: `acquire_finalized_lease` returns
  `FinalizedLease { inclusion_block, dump }`; the impossible-`None`
  containment branch in `finalized_state` is gone. Closes finding 26.
- **The reducer's one cycle is cut at the storage boundary**: the
  `EnsureOpenTip` phase splits its guard (`TipAlreadyOpen`, retry) and
  refuses inside its own transaction rather than commit without a Tip
  (`TipMissingAfterOpen`, exit 30); `drive_recovery`'s doc records the
  ≤5-phase bound. Closes findings 20 and 23.

Not yet landed from the recommended split: the PR title/body, which come
last.

## Verdict

Proportionate overall, with three named pockets of residue. The maintainer's
fear was quantitatively wrong about scale and right about residue.

| What | Lines |
|---|---|
| Branch diff | +20,459 / −5,252 across 127 files |
| Authority machinery changed (process lock, scope, lifecycle facts, supervisor) | ~1,700, under 900 production |
| Share of the branch | ~8% |
| Test functions | 418 → 588 |

The lifecycle machinery that felt over-built was built and removed inside the
branch (decisions L1 → L2 → L3, ~490 production lines); what survived is three
admission facts and one append-only table. The heavy parts of the sequencer
are recovery and storage, which predate the branch and defend in-scope L1
outage and zombie-transaction threats.

The three pockets, in descending confidence:

1. **The black box's write path.** `terminal_faults` has zero production
   readers; the in-scope recorder opens a second SQLite writer from inside
   containment, is the reason the "arm the watchdog before recording" ordering
   hazard exists, and a contained run that drains normally writes two rows.
2. **The exit-code test encoding.** Sixty-seven hand-built projection asserts
   pin a pure function four times over, while no test asserts that a real
   failing process exits with the promised code; renumbering the terminal code
   passes the whole suite.
3. **Documentation fan-out.** Fact-derived admission is described in fourteen
   places and the divergence freeze in eleven files; the branch's own L3
   rename missed three "journal" sites.

Beyond mechanisms, four lenses independently found one defect class: prose
that claims more than the types enforce (see finding 25 in the register).

**What passed the weight test and should not be cut:** the process lock; the
containment bit and the `Authorized` token at the ack, L1 send, and WS emit;
the pure reducer over one consistent inspection; the `RuntimeAdmission`
witness; the two-second abort watchdog (`/livez` returns 200 unconditionally,
so on a wedged post-containment drain nothing else pages); the clean-exit
divergence re-check.

## Verified first-hand

- CI red cause: `tracing-subscriber` 0.3.23 enables ANSI whenever `NO_COLOR`
  is unset, with no TTY check (`fmt_layer.rs:743`); the harness inherits the
  parent environment and pins only `RUST_LOG`; the e2e assertion at
  `tests/e2e/src/test_cases.rs:3151-3157` greps for `status=TipInDanger(` and
  the log carries `ESC[3mstatus ESC[0m ESC[2m= ESC[0m TipInDanger(0)`. It is
  the only log-grep assertion in the suite. Both wallet-sequencer mains lack a
  `with_ansi` call.
- PR metadata is stale: title "Fix storage decode policy", head branch
  `feature/review-ledger-and-tracks`, body describing the decode-policy scope
  and citing a retired codename; no risk/compatibility paragraph although the
  baseline migration is rewritten in place and the `Application` hooks are
  renamed. No reviewer comments. No `TODO`/`FIXME`/`unimplemented!` in the diff.
- `/livez` returns 200 unconditionally (`egress/api/health.rs:41-43`).
- `finalized_state` handles an impossible `None` on the `NOT NULL`
  `inclusion_block` by escalating to containment (`egress/api/snapshot.rs:139-146`).
- Stale vocabulary: "journal" at `storage/history.rs:201`,
  `commands/run/workers.rs:400` and `:680`; an "acknowledge" command at
  `commands/error.rs:11`; `docs/recovery/README.md` names
  `DangerDetectorExit::DangerDetected` (the type is `WorkerExit::DangerDetected`).
- `RecoveryProgress` derives `Copy`; `docs/recovery/README.md:315`,
  `admission.tla:23`, and `recovery/mod.rs:893` call the witness "non-clone".
- `Authorized` is a real signature obligation at three functions
  (`submit_batches`, `acknowledge_included`, `send_authorized`); it is minted
  and discarded at `snapshot.rs:104/129/182`, `ingress/api.rs:87`, and
  `inclusion_lane/mod.rs:211`; the batch-close and reconciliation commits at
  `inclusion_lane/mod.rs:165/309` use the raw predicate.
- `terminal_faults` has zero production readers; a contained run appends two
  rows (recorder raw cause, then the bracket's prefixed cause);
  `latest_terminal_fault` returns the second.
- `ShutdownSignal` on main was 43 lines; `runtime/shutdown.rs` is now 450.
  `http.rs` grew a lease-release supervisor with two containment call sites.

## Confirmed (jury)

- **cfg-test-gate-ensure-open-tip** (3–0). `Storage::ensure_open_tip`
  (`storage/ingress.rs:104`) is `pub` with zero production callers — a new
  instance of open finding 17 created by this branch, sitting beside its
  guarded replacement. Gate it `#[cfg(test)] pub(crate)` (not private: eight
  of nine callers live outside `storage::ingress`), de-link the two intra-doc
  references at `ingress.rs:118` and `:486`, correct `:486`'s claim that the
  runtime calls this form, and fix `docs/snapshots/lifecycle.md:26-31`, which
  still credits it with the production genesis Tip.
- **stale-decision-carries-the-failed-condition** (3–0).
  `ensure_open_tip_for_recovery` (`storage/recovery.rs:254-259`) raises
  `StaleDecision { expected: Safe, actual: facts.danger }` for a disjunction,
  so the `has_open_tip` case renders "expected Safe, found Safe", and
  `recovery_tests.rs:114-132` pins that as intended. Add a payload-free
  `RecoveryMutationError::TipAlreadyOpen`, split the two checks, add a paired
  `RecoveryRetryReason::TipAlreadyOpen` so `classify_mutation` (which today
  discards `expected`) carries it to the operator, classify Retry, update the
  test and the polarity pin. Roughly +14/−6 across three files. The arm is
  production-unreachable under the process lock; this is diagnostics.
- **detector-takes-shutdown-signal-not-runtime-scope** (2–1).
  `DangerDetector` and `InputReader` use the scope for exactly one thing,
  `wait_for_shutdown`, and each already carries a construction-required
  `ProcessLock`. Narrow `start`/`start_preflighted`/`run_forever` to
  `ShutdownSignal` and pass `scope.signal()` at the two launch sites. The
  change is incomplete without restating three doc comments that assert the
  property it relocates: `workers.rs:100-105` and `:1120-1124` ("every spawned
  worker retains a RuntimeScope clone, which also retains the process lock")
  and `shutdown.rs:96-99` ("workers that touch the data directory take a
  scope", already false for the submitter). Restate as: workers that
  externalize or contain take a scope; data-directory ownership is a separate
  construction-required `ProcessLock`. The dissent would narrow only the
  detector.
- **app-with-progress-wrapper** (2–1, a do-not-adopt). Moving
  `ApplicationProgress` into a sequencer-owned wrapper (deleting both
  capabilities, the seal, three trait methods, all three asserts, ~130 lines)
  is not viable: the pair is inside the canonical SSZ bytes
  (`examples/app-core/src/wallet_snapshot.rs:41-42`) that `create_dump` writes,
  `/finalized_state` streams, and the watchdog byte-compares, and the canonical
  machine advances it inside its own state transition; cockroach recovery reads
  the clock out of a dump into a wiped database (`commands/setup/mod.rs:433-451`,
  `Checkpoint::load`). The rationale for the clock exists at
  `docs/snapshots/format.md:113-118`; the missing piece is the composition.
  Add one sentence to `docs/protocol/application-contract.md` §4 and to
  `ApplicationProgress`'s doc comment, cross-referencing `format.md`, and
  extend `format.md`'s "must live in the canonical state bytes" sentence to
  cover `executed_input_count`.
- **bound-or-prove-drive-recovery-termination** (2–1). `drive_recovery`
  (`recovery/mod.rs:288-313`) is an unbounded loop with one cycle:
  `Repaired` + `Safe` + `!has_open_tip` → `EnsureOpenTip` → `Repaired`. No
  watchdog exists on the boot path (the scope is constructed in `prepare`,
  after recovery). Main could not spin. Take the postcondition, not the loop
  bound: after `open_fresh_tip_in_tx` in `ensure_open_tip_for_recovery`,
  re-read `has_valid_open_batch` and return a new typed variant classified
  `RecoveryError::refuse` (exit 30) — not a `debug_assert` (compiles out in
  release; the file's existing postcondition at `ingress.rs:538-541` is one),
  and not `StaleDecision` (maps to retry and relocates the non-termination into
  the supervisor). Record the ≤5-phase bound in `drive_recovery`'s doc.
  Alternative accepted by two jurors: make the reducer's `Repaired`+`Safe`+no-tip
  arm a terminal `Refuse`, removing the cycle from the pure function. The
  dissent notes the antecedent is unreachable by SQLite semantics; the
  counter-argument that carries is fidelity — `admission.tla:271-286`
  hardcodes `hasOpenTip' = TRUE`, so TLC proves termination of a model whose
  postcondition the code does not enforce.
- **typed-key-source-io-error** (2–1). `resolve_key_source`
  (`commands/config.rs:243-254`) returns a bare `std::io::Error` that lands in
  `CommandError::Io` → exit 1 ("restart with backoff") for a missing or
  unreadable key file, while bad key content in the same file exits 30 via
  `SignerMisconfig`. Kind-filter rather than blanket-map, mirroring
  `referenced_artifact_io_is_terminal` (`dump_info.rs:49-58`): NotFound,
  PermissionDenied, InvalidData, IsADirectory, NotADirectory terminal;
  everything else operational (a not-yet-mounted secret must not consume the
  do-not-restart code). Prefer a distinct `BootstrapError::KeySourceUnreadable
  { path, kind }` over reusing `SignerMisconfig`; never echo file contents.
  Roughly +30–40 lines with the predicate and tests. Consider the same
  treatment for `create_dir_all` or record why not.
- **table-drive-exit-code-tests** (2–1). Fold the five per-class tests and
  the duplicating verdict test (`error.rs:782-828`) into one `const CASES`
  table keeping every distinct error shape and every reason string as the
  assert message; drop the verdict column (derivable from the bijection);
  assert `is_terminal()` per row, which extends a five-shape pin to ~54. Do not
  invent rationales for rows that carry none. Realistic saving is 110–150
  lines, not 250. The point is the spend: no test asserts a real failing
  process's exit code (the four failure-path e2es assert only `!success()`),
  and the `EXIT_*` values appear as literals only at their declarations, so
  renumbering `EXIT_TERMINAL` to 31 passes the suite. Add SIGTERM → 0 (the
  harness already waits and discards the status) and one 30-class failure → 30
  (`run` on a never-set-up data directory needs no new lever), asserting integer
  literals. Correction from the jury: composition is pinned in-crate at
  `workers.rs:1209/1228`, `run/mod.rs:237`, `startup_hygiene.rs:168/191`,
  `commands/mod.rs:330`, `process_lock.rs:159`; the gap is the process-level
  projection at `harness.rs:117-119`.
- **dedupe-terminal-fault-rows** (2–1 for documenting; 0–3 against skipping
  by variant). Document the two-row shape at `record_terminal_fault`, in the
  ADR's black-box paragraph, and in the runbook's postmortem line, including
  the asymmetry: a clean contained drain yields two rows; a controller panic or
  watchdog abort yields one. Do not skip the bracket write when the error is
  `StorageInvariantViolation`: the recorder swallows both `open_writer` and
  `record_terminal_fault` failures into a `warn!`, so the variant does not
  prove a row landed, and the post-drain bracket write is the attempt more
  likely to succeed after `SQLITE_FULL` or contention (5 s `busy_timeout`
  against a 2 s abort deadline). If one row per fault is wanted later,
  condition the skip on evidence (an `AtomicBool` the recorder sets on
  success), not on the variant.

## Refuted (jury) — do not re-propose without new evidence

- **supervise-workers-with-joinset** (3–0). `select_first_exit` and `finish`
  deliberately read the same worker return two ways: `WorkerStop::from_select`
  maps `Ok(Ok(()))` to `StoppedUnexpectedly` (the runtime is live), while
  `from_shutdown` maps it to `Ok(())`. Feeding both phases from one `JoinSet`
  of `wait_for_*_shutdown` futures collapses them into the shutdown reading, so
  a worker that dies silently while live yields a value `FirstExit` cannot
  represent; the available completions are "run with a dead lane" or "drain
  and exit 0", the one code `run/mod.rs:82-87` names as breaking the
  supervisor's rediscovery chain. `into_supervision(self)` would also drop
  `ShutdownOnDrop`, requesting shutdown milliseconds after launch; the
  conditional fee-oracle push relocates rather than deletes; and
  `FirstExit::detector` plus its mapping tests disappear (one of the four
  reasons the register already refuted this shape on 2026-08-23). **Survives:**
  the `swap_remove` hazard at `workers.rs:653-655` is real and unwritten in
  types; a cleanup-only `JoinSet` built inside `finish`, with the live race
  untouched, was not what the jury examined.
- **collapse-preparedruntime-into-boot** (3–0). The load-bearing claim ("zero
  tasks during fallible work is reviewer-visible, not type-enforced") is false:
  `fn launch(self, _admission: RuntimeAdmission) -> Workers` (`workers.rs:288`)
  is non-async and non-`Result`, so a `?` or `.await` between admission and the
  six spawns is a compile error today. `async fn boot(..) -> Result<..>` makes
  both silently legal, reopening a guarantee registered in the check policy,
  ADR mechanism 1, and AGENTS.md, and the linearization argument at
  `recovery/mod.rs:477-483`. Under `boot` nothing consumes `RuntimeAdmission`
  (`let _a = admit_runtime()?` satisfies `#[must_use]`). Commit 02a2b34 ran
  this pass and deliberately stopped here. **Survives:** the test
  `preparation_outliving_clean_facts_cannot_launch` (`workers.rs:1161`) never
  calls `launch`; rename it to what it asserts.
- **make-authorized-token-uniform** (3–0). `LeasedDumpBody` does not exist
  (the primitive is `stream_body(file, guard)` at `snapshot.rs:214`);
  `finalized_inclusion_block` (`snapshot.rs:101-120`) has no streaming
  primitive to receive a token; and `ingress/api.rs:87` is not a pre-check —
  its comment calls it the publication gate, it runs after the lane's ack
  resolves, and it immediately precedes the success body that is the soft
  confirmation leaving the process, the ack family the ADR names as a token
  site. Only the `inclusion_lane/mod.rs:211` half survives (a fast-turn entry
  gate; the real ack boundary re-consults at `:248`). **Survives:** the doc
  tightening — the token proves "consulted at some point in this borrow", not
  "at this effect boundary" (the poster mints at `worker.rs:243` and then does
  a chain-id RPC, fee estimation, and a nonce fetch before its own re-checks);
  and the coverage claim in the ADR/register should say three compile-forced
  primitives plus hand-placed consults at the HTTP 200 gate and the two lane
  mutation commits, or the 200 body should take the token (~8 lines).
- **recovery-polarity-unconstructible** (3–0). Diagnosis exact:
  `RecoveryError::retry(RecoveryRefusalReason::CanonicalDivergence{..})`
  compiles today and would project the absorbing refusal to exit 20 (bounded to
  one restart by the next boot's preflight). But `recovery` is `pub mod` and
  both enums have public variants, so deleting the `#[from]` impls removes the
  shortest spelling, not the route: `RecoveryError::retry(RecoveryFailure::PolicyRefusal(r))`
  still compiles, and that longer spelling is the dominant idiom at all 15 call
  sites. Cost ~16 renames for a property not achieved. The invoked precedent
  (1fcb9aa) deleted the violating value from the type; this does not.
- **single-table-per-error-type** (3–0). `From<VerifiedSignerProviderError>
  for BootstrapError` (`error.rs:671-683`) performs no terminal/transient
  decision — it selects among three variants with distinct fields, and the
  verdict is taken later over the `BootstrapError` taxonomy, whose variants
  have four other producers. "Have both sites read one `is_terminal`" is not
  implementable; the result is a third table. Part (b)'s premise is false:
  `reader.rs:96-104` states the phase-dependence for `Bootstrap` and `Join`.
  Renaming to `is_terminal_in_worker` is wrong for `FlushError` (no
  `WorkerExit` arm), and "phase in the type" is blocked because the phase
  belongs to the caller (`create_provider(..).map_err(InputReaderError::Bootstrap)`
  appears identically in `sync_to_current_safe_head` and `run_loop`).
  **Survives:** `classify_input_reader` (`recovery/mod.rs:526`) carries no doc
  comment and its `Bootstrap`/`Join` refusals are pinned by no test; and a
  pre-v3 InputBox exits 1 under `setup` but 30 under `run` — a separate
  finding.
- **flatten-recovery-error-to-one-enum-with-is-retryable** (3–0). A flat
  `is_retryable(&self)` must be total over the value, and one variant carries
  two verdicts: `ProductionRecoveryDriver::flush` (`recovery/mod.rs:429-438`)
  maps `VerifiedSignerProviderError::ChainIdRpc` → retry and `::Create` →
  refuse into the same `RecoveryFailure::Provider(String)`. Either resolution
  regresses (a bad RPC URL restart-loops forever, or a transient chain-id
  timeout pages), nothing pins either arm, and dropping the `Box` risks the
  deliberately managed `CommandError` footprint under `result_large_err`.
  **Survives:** split `Provider(String)` into two verdict-determined variants
  as an independent fix; the wrapper's doc claims a context-sensitivity the
  `classify_*` functions do not use.
- **drive-recovery-owns-phase-to-progress-mapping** (2–1). The replacement
  is not total: `(RecoveryPhase::Flush, PhaseOutcome::Done)` has no target
  because `Flushed { observed_safe_block }` needs a block number the loop does
  not hold; the pre-existing implementation of exactly this mapping
  (`PhaseCompletion` + `transition_after_phase`) was deleted by `ed41f9b`, whose
  message pre-answers the argument. **Survives:** delete
  `RecoveryDriver::admitted` (`recovery/mod.rs:283`, a production trait method
  whose only implementor pushes a string into a test trace; `drive_recovery`
  returns `Ok(())` only from the Admit arm); and the five trace tests exercise
  the double's copy of the mapping while production's copy is pinned by no unit
  test.
- **merge-stringly-bootstrap-variants** (2–1). `FeeOracleMisconfig` has two
  further producers (`setup/mod.rs:144` and `:171`) passing bare strings whose
  "fee oracle misconfiguration" words exist only in the variant's Display, and
  the black box stores `error.to_string()`, so merging attributes an operator's
  Uniswap mistake to a trusted-code fault in the one postmortem artifact. Part
  (b) demotes a compile-forced classification to an unchecked `&'static str`
  discriminant, the shape the register refuted twice on 2026-08-23. Real delta
  ~−15, not −25.
- **terminality-trait-for-workerstop** (3–0). Inverts its goal:
  `WorkerExit::is_terminal` already matches all seven variants by name; the
  trait admits `fn is_terminal_invariant(&self) -> bool { false }` exactly as
  plausibly as `|_| false`. `impl TerminalityOf for std::io::Error { false }`
  installs a crate-wide answer for a type the codebase has decided has no
  context-free answer (`dump_info.rs:49-58` classifies several kinds terminal);
  a `pub(crate)` trait in a public type's bound trips `private_bounds` under
  `-D warnings`; and `is_terminal_invariant` is inherent on eight types, only
  five of them `WorkerExit` payloads. Delta inverts to +4..+15.
- **prune-duplicate-tla-actions-and-run-check-admission-in-ci** (3–0).
  "Nix already provides tlc" is false for CI: no Nix expression or `.envrc` is
  tracked, `ci.yml` provisions tools by hand, and `just` is absent from the
  `rust` job; TLC also checks the spec against itself and says nothing about
  spec-vs-Rust drift. The three deletions are state-space-neutral (`Crash`
  subsumes every settle action), but the rationale is false: `decision`
  records Retry/Refuse, `DecideRetry`/`DecideRefuse` have distinct guards, and
  `InspectRetry` encodes its own comment ("a known local divergence cannot be
  masked by the retry edge"). **Survives:** put the 860-state model under CI as
  a properly pinned standalone `formal` job (JDK + `tla2tools.jar` pinned by
  version and sha256 in `toolchain-pins.env`).

## Unverified (raised by one reviewer, not put to a jury)

Re-verify the cited lines before acting on any item below.

### Runtime authority and containment

- **drop-in-containment-fault-recorder** (threat challenge, Δ−110). Delete
  the `FaultRecorder` alias, field, `set_fault_recorder`, and the recorder
  invocation from `runtime/shutdown.rs`; delete `install_terminal_fault_recorder`
  from `workers.rs`. Containment becomes three non-blocking steps (set the
  cause, arm the watchdog, request shutdown), removing the "either may block"
  ordering hazard and the second SQLite writer inside containment. The bracket
  write at `run/mod.rs:90` keeps recording every contained fault that settles.
  Loss: a fault whose drain hangs past 2 s and exits via SIGABRT leaves no row
  — already the documented status for unclean deaths
  (`docs/watchdog/operator-deployment.md:392-395`). Note the confirmed
  dedupe verdict above: the bracket write is a genuine retry, which argues for
  keeping one writer rather than two, and for the bracket one.
- **delete-terminal-faults-black-box** (threat challenge, Δ−330) and
  **cut_black_box** (minimal design, Δ−185). Delete the table, its two
  triggers, `TerminalFault`, `record_terminal_fault`, `latest_terminal_fault`,
  `LifecycleCommand::parse`, `record_terminal_fault_best_effort` and its four
  call sites; replace the runbook's `SELECT * FROM terminal_faults` paragraph
  with the log-and-exit-code instruction. New evidence against decision L3:
  zero production readers; no CLI or API surface; the write path spans four
  files; cockroach recovery wipes the database in exactly the incident class
  where the postmortem matters. Counter-argument neither author could dismiss:
  a Kubernetes Deployment restarts regardless of exit code, so a terminal
  fault restart-loops and the first cause could rotate out of the logs while
  the black box retains it. Judgment call, not a defect.
- **close-or-correct-the-token-coverage-claim** (threat challenge, Δ+8).
  Either make `ingress/api.rs:85-92`'s success response take `Authorized`
  (mirroring `acknowledge_included`), or amend ADR mechanism 1 and the
  register's settled entry to the true scope. Do not extend the token to the
  lane's mutation commits (they sit inside `&mut self.storage` borrows). The
  jury's refutation of the "uniform" proposal above endorses this framing.
- **sketch_boot_shutdown** (minimal design, Δ−260) — a reference sketch that
  keeps the lock, scope, token, `ShutdownOnDrop`, reducer, hygiene, lifecycle
  facts, and the typed `Workers`/`FirstExit`, and deletes `ShutdownSignal`
  (fold into the scope), `RuntimeAdmission`, `PreparedRuntime`/`WorkersConfig`,
  and the `WorkerId`/`ComponentShutdown`/`next_component_shutdown`/six-waiter
  drain in favour of `Option<JoinHandle>` fields taken at the winning select
  arm and one `tokio::join!` over a generic `drain`. Partly overtaken: the
  jury refuted the `PreparedRuntime` collapse and the `ShutdownSignal` half is
  contradicted by the confirmed narrowing (the slim half gains two consumers).
  The `Option`-take drain is the one part not examined by a jury; it avoids
  all four grounds of the 2026-08-23 refutation (named fields, named arms, no
  `swap_remove`, no empty-list race).
- **cut_admit_runtime** (minimal design, Δ−60). Delete `RuntimeAdmission`,
  `admit_runtime`, `AdmissionChanged`, and the `launch(_admission)` parameter;
  keep the fallible-then-infallible ordering. Argument: between the reducer's
  `Admit` and launch, the lock excludes every other process and zero tasks
  exist, so only wall-clock drift can change the facts, and those arms are the
  Retry class re-derived by the detector within one 2 s poll. Adjacent
  refutation applies in part: the jury defended the witness as the thing that
  makes "launch only from a fresh admit" a compile fact and the basis of the
  linearization at `recovery/mod.rs:477-483`. Low priority.
- **taxonomy_min** (minimal design, Δ−145). Regroup `BootstrapError`'s
  seventeen variants into verdict-uniform groups (`Misconfig(..)` uniformly 30,
  `Transient(..)` uniformly 20, `Recovery`, `SetupNotComplete`, `SetupRefuse`,
  `OpenStorage`), move `IdentityError::FirstBootRequiresL1` into the transient
  group so `IdentityError` becomes uniformly terminal, delete
  `CommandFailureVerdict` (five variants in bijection with five constants, two
  consumers), delete `WorkerId` if the drain no longer needs identity. The
  taxonomy has exactly one consumer outside the crate (`harness.rs:117`).
  Partly overtaken: the jury upheld keeping `is_terminal()` as the thing the
  black-box write gates on.
- **leased_dump_inclusion_block_non_optional** (refuted-list audit, Δ−4;
  premise verified first-hand). Stop sharing `LeasedDump` between the
  finalized and latest lease queries (own return type or `LeasedDump<M>`), and
  delete the `let Some(inclusion_block) = leased.inclusion_block else {
  contain(..) }` branch at `snapshot.rs:139-146`. The column is `NOT NULL`
  (`0001_schema.sql:855-856`); the L3 review refuted a boot assert on it and
  left a heavier runtime branch that maps a type artifact to exit 30, contrary
  to the check policy's "no `Option`-handling for can't-be-`None`".
- **startup_log_last_terminal_fault** (refuted-list audit, Δ+8). After the
  preflight in `run`, read `latest_terminal_fault` and emit one `warn!` when
  present. Explicitly not a gate: no acknowledgement, no branch on the value.
  The recorded refutation argues against a gate on a verdict; a read that
  changes no decision is untouched by it. This would give the black box its
  first in-product reader; if declined, say in the register that the black
  box is an out-of-process artifact by design.
- **startup_hygiene_single_finalized_read** (refuted-list audit, Δ−6).
  `require_finalized_snapshot` and `restamp_finalized_promotion` each query
  `finalized_dump()`; fetch once in `run_snapshot_hygiene` and pass the row.
  Ordering of the five steps is unchanged.
- **reconsider-release-supervisor-weight** (lane lens, ~175 lines). The
  supervised lease-release queue in `http.rs:157-235,325` (unbounded MPSC of
  boxed closures, a `JoinSet` supervisor with a two-armed `select!`, a drain
  awaited inside `axum::serve`, containment on both joins, `ReleaseScheduler`
  changed to `Arc<dyn Fn>` plus a second reporter) defends a real but narrow
  hole: a `StatementChangedRows` on release means the leased row vanished,
  which nothing else re-detects. Two lighter shapes: (a) keep the
  classification, drop the supervisor, accept that a release racing the very
  end of shutdown may miss classification (~−120 lines); (b) keep the drain
  but move it to the egress snapshot module that owns leases, so `http.rs`
  stops hosting a runtime component. The queue is unbounded and bounded only
  by concurrent snapshot requests, which have no cap.
- **drop-token-ceremony-at-bool-sites** (lane lens, Δ−10). Overlaps the
  refuted uniform-token proposal: the `/tx` publication-gate half is refuted
  (it is the ack); the snapshot half survives only as "push the token into
  `stream_body`" for the two streaming routes. Separately worth a maintainer
  decision: the `/tx` gate returns 503 for an operation that is durably
  committed and may still reach L1; the API contract should state the
  client-visible semantics of "503 after commit".

### Error taxonomy

- **fee_price_stamp_surface_or_drop** (refuted-list audit, Δ+10).
  `log_gas_price_updated_at_ms` is written on every refresh
  (`storage/fee_oracle.rs:29-40`) and read only by tests, yet the threat model
  cites it as the honest telemetry that justifies having no expiry gate.
  Either surface the age in `GET /healthz` as an informational field, or
  include `retained_price_age_ms` in the existing transient-refresh warn, or
  drop the column and fix the threat-model sentence. No threshold, refusal, or
  lifecycle effect is proposed.
- **register_provenance_and_wording** (refuted-list audit, Δ+6). Applied in
  the register on 2026-09-03: the fee-price-age refutation was added by
  143a290 (2026-08-25) but filed under "From the ADR re-evaluation
  (2026-08-01/02)"; the boot-gate refutation's "(in any form)" is broader than
  the argument it rests on; the drain-merge entry says "Scope-narrowed
  2026-08-23" while the landing commit is dated 2026-08-24.

### History foundation and the application boundary

- **drop-panicking-progress-constructor** (Δ−12). `ApplicationProgress::new`
  panics on an incoherent pair and has only test callers; its own doc says to
  use `try_new` on the only path that constructs one from data. Delete it;
  tests become `try_new(..).expect(..)`. Register finding 17's category.
- **defer-era-newtypes-to-track3** (Δ−110). The schema slice (`history_state`
  columns, five triggers, the generation bump inside `cascade_and_reopen`) is
  cheap to carry and expensive to retrofit, and should stay. The Rust surface
  (`EraId` with Display/Debug/TryFrom, a three-variant parse error,
  `RecoveryGeneration`, `HistoryVersion`) has zero production readers; the WS
  feed destructures the coordinate away with `..` (`l2_tx_feed/mod.rs:299-315`),
  and Track 3 says the era leg is explicitly unconfirmed by the consumer.
  Alternative: represent the era as `[u8; 16]` at the storage boundary and let
  Track 3 introduce the newtypes beside the wire codec. Low cost either way.
- **drop-uuid-version-variant-checks** (Δ−30). `mint_era_id`
  (`open.rs:242-261`) stamps v4/RFC-4122 bits into a random blob, then the Rust
  constructor and a SQL `CHECK` verify that self-imposed constant at three
  points, for a token whose only semantics is equality. Keep the 16-byte
  newtype, length check, and hyphenated Display; drop the version/variant
  stamping and checks (+6 bits of entropy). The one argument for keeping it is
  a future strict-UUID consumer, a Track 3 wire concern; if kept, reword the doc
  from "must carry" to "presentational contract for the future wire form".
- **single-enforcement-for-mapping-contiguity** and
  **drop-duplicate-offset-assert** (Δ−15). `attach_executed_inputs_in`
  (`history.rs:116-133`) recomputes the exact predicate
  `trg_executed_inputs_contiguous` (`0001_schema.sql:562-575`) enforces, on the
  accepted user-op path with the latency contract — one `query_history_state`
  read plus one `MAX(executed_input_offset)` probe per chunk. Its own comment
  says the schema independently enforces the rule. Keep one enforcement point,
  preferably the trigger (cannot be bypassed by any writer, aborts the
  transaction rather than unwinding a panic through the lane). For directs the
  offset is checked a third time by the derive-and-compare below.
- **drop-terminal-fault-typed-reader** (Δ−55). `latest_terminal_fault`,
  `TerminalFault`, `LifecycleCommand::parse`, and the two `Malformed` variants
  that only report a malformed black-box row exist to serve three test
  assertions; an empty cause is already impossible at the engine
  (`0001_schema.sql:628-630`). Contradicted in part by
  `startup_log_last_terminal_fault` above, which would give the reader a
  production caller; decide the black box's reader story once.
- **single-admission-implementation-for-setup** (Δ−25).
  `preflight_lifecycle_command` has two callers (`run`, `flush`); setup and
  rebuild go through `admit_setup_lifecycle` (`setup/mod.rs:361-388`), which
  re-implements the same two facts with different semantics (an
  already-complete plain setup is a no-op success there, `NotAdmissible` in
  the lifecycle module). Make `preflight_lifecycle_command` return a three-way
  admission for setup/rebuild and have setup call it. Also `run` calls the
  preflight (which refuses without `setup_complete`) and then
  `load_setup_identity` re-checks completion with a different error type; drop
  the second check.

### Lane and storage

- **narrow-direct-attribution-cross-check** (Δ−8). `persist_frame_direct_sequence`
  (`mutations.rs:168-181`) re-derives every direct's sender over the whole
  drained range and asserts vector equality with the lane's receipts inside the
  reconciliation commit; the lane read the same rows moments earlier. Cheaper
  shapes: carry the skipped-submitter count and assert
  `executions.len() + skipped == range.len()` plus first/last offset, or keep
  the derive behind `cfg(debug_assertions)`. The honest answer depends on the
  catch-up ACK measurement the register already owes (5,000 directs over a
  7,200-block jump in one turn).

### Tests and e2e

- **gate-remaining-test-only-storage-api** (Δ−40). `latest_batch_index`
  (`l1_submission.rs:97`), `ordered_l2_txs_for_batch` (`:129`), and
  `promote_finalized` (`snapshot_dumps.rs:182`) are `pub` with only test
  callers; `promote_finalized` can promote without the lane's inclusion-block
  and lease invariants. Delete the first two (fold into their tests), gate the
  third. This is what the in-crate test move was supposed to unlock (register
  finding 17).
- **drop-tryfrom-accepts-tests** (Δ−35). Five `*_accepts_*` tests in
  `storage/convert.rs` assert that `std::convert::TryFrom` is correct; keep
  every `should_panic` twin (they pin the settled decode policy) and
  `prepare_time_sql_failures_classify_persistent_in_both_spellings`. Also
  `era_id_displays_canonical_lowercase_hyphenated_form` pins a Display string no
  consumer parses. Record in the register that the 21 new
  `#[should_panic(expected = ..)]` attributes are accepted panic-message
  coupling.
- **unify-harness-chain-clock** (Δ+60). Four notions of block time exist:
  `SECONDS_PER_BLOCK = 12` duplicated at `rollups.rs:264` and
  `sequencer.rs:876`, `LIVE_L1_BLOCK_INTERVAL_SECONDS = 1`, `BOOT_L1_MINE_INTERVAL
  = 1 s`, and the sequencer's configured `seconds_per_block = 12`; e2e
  correctness depends on their unwritten relationship staying under the 12 s
  clock-usability threshold. `advance_live_frame_until_covers`
  (`test_cases.rs:480-514`) can drive L1 roughly 20× ahead of the process
  clock per iteration. Give the harness one `ChainClock` owned by the devnet
  stack, constructed from the value passed to the sequencer, with
  `advance(Duration)` and `mine_live(n)` deriving from it, and one post-mining
  check `l1_head_timestamp − faketime_now < seconds_per_block` so drift fails
  loudly in the harness. Two of the three re-staged scenarios are principled
  and strictly stronger (`sequencer_outage_danger_zone_tip_cascade` now asserts
  an invalidation; `wall_clock_backward_jump_retries_then_recovers` is the only
  per-variant exit-code e2e in the suite).
- **replace-timewarp-tip-injection** (Δ−10). `aging_open_tip_runtime_danger_zone_exit_test`
  injects a wedged lane by mining 1,150 blocks with wall time frozen (a chain
  3.8 h in the future), then compensates with `mine_live_l1_blocks(1)` plus an
  absolute faketime offset, and greps the log to prove the future-dated view did
  not route into the clock-fallback arm. Replace the injection with a
  lane-level one (a `--freeze-frame-clock` test dial beside the existing
  batch-open dial), advance wall and L1 together, and delete the compensation
  and the log assertion. Minimum fix: assert exit code 10 instead of the log.
  Also `set_faketime_offset` resets the cumulative counter while leaving an
  absolute offset in the rc file, so a later `advance_wall_and_mine` in the
  same scenario would regress the child clock.
- **replace-watchdog-sleep-assertions** (Δ−20). Two watchdog tests are
  negative assertions implemented as `recv_timeout(250 ms).is_err()`;
  unfalsifiable by slowness. Expose `is_watchdog_armed()` under `#[cfg(test)]`
  or have the injected abort action record whether a deadline was scheduled.
- **merge-detector-arm-mapping-tests** (Δ−25). Three tests cover the
  13-line `FirstExit::detector`; merge into one table test over the four join
  shapes. The composed containment tests the 2026-08-23 refutation protects are
  untouched.
- **document_second_half_clock_assumptions** (Δ+12). Record at
  `test_cases.rs:3168-3176` that the `+1` alignment margin is not the real
  margin (Anvil block timestamps track wall time) and that the single
  `mine_live_l1_blocks(1)` refresh has one block of headroom only because Anvil
  runs with `--slots-in-an-epoch 1`.
- Also open: `RuntimeScope::default()` (`shutdown.rs:258-267`) leaks one temp
  directory per construction via `mem::forget`; worth asking whether a
  `(RuntimeScope, TempDir)` guard should be the only shape.

### Documentation corpus

- **refuted-evidence-grades** (Δ+10). Give each refuted entry an `evidence:`
  line naming the file/line or measurement a reader can re-run; demote entries
  that cannot produce one to "declined, no evidence recorded". Restore the
  deleted cost datum to the per-chunk divergence-query entry (the
  pre-distillation ADR read "every roughly 14-ms user-op chunk"; "14 ms"
  appears in zero markdown files now). Name the select arm in the
  homogeneous-list entry's title, since the `Vec<(WorkerId, ComponentShutdown)>`
  shape now exists in the tree for cleanup.
- **collapse-six-stubs** (Δ−110, −6 files). Six of the eight dated ledgers are
  15–20 line stubs carrying a verdict plus a pointer; collapse them into a
  "Review history" table at the bottom of the register. Keep the two August
  ledgers (they carry the only re-verifiable evidence in the corpus).
- **adr-dedupe-vs-register-and-invariants** (208 → ~70 lines). Each ADR
  mechanism is also described in the invariants check policy, AGENTS.md, the
  recovery README, the threat model, the runbook, and module docs; five of six
  rejected alternatives are also in the register's refuted list, and the two
  point at each other circularly. Cut the ADR to context, the policy statement,
  and four mechanism names with pointers; move the rejected-alternatives
  arguments into the register so there is one home.
- **single-home-divergence-freeze** (Δ−60). `docs/invariants.md:353-372` and
  `docs/recovery/README.md:398-415` are the same four sentences; the ADR's G3
  and `AGENTS.md:264` are third and fourth compressions. I15 owns the runtime
  reaction and race bound; the others link.
- **agents-hotpath-to-pointers** (50 → ~20 lines). `AGENTS.md:255-304`
  restates I2, I3, I9/I15, I17, I18, and the admission policy in fifteen
  paragraph-length bullets, violating its own line-475 rule; the good pattern
  is already used at `AGENTS.md:118-121` and `:324`.
- **module-docs-explain-not-defend** (Δ−15). Strike the four defensive
  clauses (`workers.rs:21-27` "they are the enforcement, not style";
  `error.rs:10-13`'s dated `RunError` history and stale "acknowledge";
  `shutdown.rs:20-23`; `storage/recovery.rs:15-23`), and fix the three "journal"
  usages.
- **finish-the-codename-sweep** (~14 one-line edits). Residue: `history.rs:202`
  ("L2"), `l1_inputs.rs:41` ("H6"), `wallet.rs:101` ("D10") added by this
  branch; `provider.rs:160,234`, `e2e_sequencer.rs:411,429,537`,
  `tests/harness/src/sequencer.rs:33,38,302,305,583`, `test_cases.rs:3039,3924`
  predate it. Track 6's requirement labels R1–R5 collide with the codename
  map's R1–R5.
- **proportionality-measured**. Measured: ~8,026 lines of standalone doc/spec,
  ~6,792 comment lines, ~21,200 lines of production Rust, roughly 0.7 prose
  lines per code line; the branch's own margin is one doc line per six code
  lines. Volume is defensible; the unstated fan-out is not. Either adopt a
  single-home rule with a named canonical copy per mechanism, or write down
  that redundancy is deliberate and name the canonical copy.
- Also open: `docs/plans/` is listed as timeless in AGENTS.md but the tracks
  board is a dated status board; the deleted terminal-containment plan's
  marker-file protocol has no refuted entry anywhere.

### CI

- **binary_disables_ansi_when_not_a_tty** (Δ+4, rank 1). Add
  `.with_ansi(std::io::stdout().is_terminal())` to both wallet-sequencer mains
  (`IsTerminal` is std). Independent of the test: a daemon writing to a pipe,
  file, or journald must not emit SGR escapes.
- **assert_exit_class_not_log_text** (Δ−4, rank 2). Replace the log grep with
  `exit.code() == Some(10)`; `TipInDanger` projects to 10 and the clock
  fallback to 20. Caveat: 10 does not separate `TipInDanger` from
  `ClosedBatchInDanger`; if that matters, keep one check anchored on the
  never-styled substring `TipInDanger(` alone.
- **harness_pins_no_color** (Δ+2, rank 3). Pin `NO_COLOR=1` beside the
  `RUST_LOG` pins at `sequencer.rs:1300` and `:1394` as hermeticity, not as the
  fix.
- **strip_ansi_in_assertion** — rejected by its author; recorded so it is not
  re-proposed.
- **centralize_tracing_init_in_run_main** (Δ−10, optional). The two mains are
  byte-identical apart from the config constructor; `run_main` already owns the
  exit-code contract and could own log rendering. A library installing a
  global subscriber is a deliberate boundary decision, not part of the CI fix.

### Roadmap items (plan, not simplification)

- **pr-body-rewrite** — a ~270-word draft exists in the fleet output; the
  title should name the actual scope and the body must carry the two breaking
  changes (baseline migration rewritten in place; `Application` hooks renamed).
- **pre-draft-ci-fix**, **pre-draft-metadata** — the two blockers; plus
  `docs/plans/2026-07-coordination-tracks.md`'s "ready for its PR against
  main" line and the register's verification date.
- **fold-before-merge** — findings 4 (flusher `error!` → `warn!`), 5 (`/tx`
  500 body echoing `AppError` strings), 17 and the second half of 10 (gate the
  test-only storage surface), and the false `debug_assert` comment at
  `sequencer-core/src/fee.rs:228`.
- **followup-1-submitter** (findings 1–3, ~250 lines), **followup-2-schema**
  (findings 9, 11, 18, while the baseline-rewrite window is open),
  **followup-3-measure** (the 500 ms objective and catch-up ACK p99),
  **followup-4-harness** (the owed levers and the e2es they unlock, split by
  lever), **followup-5/6/7-track3** (typed history foundation and
  `GET /history-version`; finalized replay routes, gated on consumer decisions
  2 and 3; the `/ws/subscribe` cutover absorbing findings 6 and 7),
  **followup-8-track6** (working-image `Application` API; breaks the same trait
  this PR breaks).

## Recommended split

**In this PR before it leaves draft:** the CI fix (binary ANSI plus the exit
code assertion), the PR title, body, and breaking-change notes, the two doc
lines that go false on merge, the prose-versus-types honesty sweep (token
coverage claim, "non-clone", the witness wording, the three "journal" words,
the "acknowledge" mention, the README type name, the two-row black-box shape),
the five mechanical register items, gating `ensure_open_tip`, and deleting the
panicking progress constructor. Each touches files the branch already rewrites
and none changes behaviour a reviewer has not already seen.

**A focused successor PR:** the remaining confirmed items — `TipAlreadyOpen`,
the notification-half narrowing with its doc restatements, the kind-filtered
key-file error, the tip postcondition refuse, the exit-code table with the two
process-level assertions — plus whichever unverified runtime items survive
their own re-verification (the lease-release supervisor's home, the
non-optional inclusion block, the in-scope recorder). These change behaviour
or exit-code classification and deserve their own adversarial pass and tests.

**Later, in order:** the doc single-home passes; submitter pacing; schema
hardening before first deployment; the latency measurement; the harness
levers; Track 3; Track 6.
