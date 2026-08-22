# Over-engineering review — findings and work queue (2026-08-18)

Full-branch review of the authority-boundary + durable-history-foundation
branch (merge-base `f59ec25`, 20 commits, ~20k insertions) against the
project's explicit design goals: readable, auditable, easy to reason about;
over-engineering treated as a threat; every mechanism judged against its
weight. Method: seven parallel subsystem reviews (durable lifecycle, recovery
reducer, runtime supervision, inclusion lane, storage/history, scheduler
contract, periphery), each simplify/cut proposal then adversarially
cross-examined against `docs/invariants.md`, the review ledgers, the TLA+
models, and git history; plus an independent premise challenge of the ADR's
G1/G2/G3 and a compositionality pass. 141 mechanisms inventoried: 98 keep,
25 simplify, 6 cut, 12 question — with several simplify/cut proposals then
refuted by the adversarial pass (recorded below so they are not re-proposed).

Calibration rule adopted with this review (maintainer, 2026-08-18): this
sequencer is not algorithmically complex — the complexity budget belongs to
concurrency, mutual exclusion, durability, and hostile-L1 robustness. A large
file is a smell: either an invariant we're not seeing, a reasonable assumption
we're not taking, or plain over-engineering.

## Verdict first

**The branch is not over-engineered — it is unevenly engineered.** The
premise survives attack: the motivating bug class is documented by count, the
ADR's rejections (`RunEpoch`, `EffectGate`, `LiveKernel`, the generic command
reducer) left no residue in the code, and the four mechanisms are separable,
each guarding a distinct thing. The 20k-line headline overstates weight:
~3,100 lines are integration tests relocated in-crate (a consequence of the
crate-private launch boundary, which is that boundary's point), and the
heaviest files are 40–55% tests that overwhelmingly pin real contracts.

Three findings cut against the ADR's "smallest sufficient design" claim:

1. **The branch did not finish its own thesis in-process.** Three of the four
   mechanisms turn the forgot-the-predicate bug class into type-level
   obligations; the fourth — in-process terminal containment — is still an
   ambient `AtomicBool` consulted at ~11 hand-placed sites in two spellings.
   The consolidation is strongest where the original findings were weakest
   (boot/crash authority) and weakest where they were found (in-process
   externalization). Structural fix: S-A below.
2. **~700 lines of mechanical repetition and residue** the ADR never argued
   for, because it was never a design question — it accumulated. Queue: H1–H14.
3. **The heaviest module exceeds its guarantee.** `storage/lifecycle.rs`
   (1,857 lines) implements the right write-ahead guarantee with a heavier
   representation than G2 requires. Decision L1 below.

## Affirmed — do not re-litigate

Mechanisms reviewed and judged load-bearing at close to minimal weight; a
future reviewer should start from these dispositions:

- **ProcessLock** (`runtime/process_lock.rs`) — 215 lines, kernel-enforced
  exclusivity, `Weak` witness is exactly the right primitive. The cheapest
  mechanism per unit of guarantee on the branch.
- **Two-second terminal abort watchdog** — ~50 lines; the only G1 enforcement
  not dependent on per-site discipline; armed before audit work so a blocked
  SQLite write cannot defeat it.
- **prepare/admit/launch + `AdmittedRuntime`/`RuntimeAdmission`** — single-use
  capability, minted at one line, `launch` is `fn` not `async fn` (infallible,
  non-yielding, enforced by the type system).
- **Pure recovery reducer + ephemeral witnesses + `admission.tla`** —
  `reduce_recovery` is a 52-line pure function; `drive_recovery` is the ADR's
  loop written literally in 26 lines. `RecoveryProgress` **is** the
  phase-ordering state machine (Cascade-requires-Flush-in-this-process) and
  must survive H4's trim around it.
- **G3 trigger-enforced divergence freeze** — the best-grounded guarantee on
  the branch; structural at the data layer; honest race bound correctly
  accepted rather than closed with a cross-worker lock.
- **`SafeFrontierState` typed poison** — poison inside the return type; a
  caller cannot obtain a frontier without matching past divergence.
- **Operator-sticky stale `Active` + acknowledge-to-`NeedsRecovery`** — kept,
  with the honest reframe in P8: it is crash-loop-breaking, not verdict
  preservation (a reflexive ack yields byte-identical decisions to automatic
  inspection; the design degrades safely).
- **`RunId` exact-record acknowledgement** — closes a real ABA across DB
  replacement; 16 bytes, one `randomblob`; see P1 for the threat-model note.
- **History triple `(EraId, RecoveryGeneration, K)` storage foundation** —
  minimal for the two client remedies (generation = replay soft suffix; era =
  rebootstrap; K = honest availability); each coordinate recordable honestly
  only at the moment it happens. See P5 for the at-risk flag on era semantics.
- **`executed_inputs` sparse canonical projection** — not speculative: three
  live production consumers today (catch-up validation, lane-start coherence
  refusal, snapshot registration assert). Restart-determinism machinery, not
  API groundwork.
- **Track 4 decode policy** — no `saturating_query_bound` violation found at
  any storage call site; several absorbing patterns moved to loud ones.
- **Shared typed execution boundary + opaque capability pair**
  (`sequencer-core/src/application/mod.rs:313-414`) — the best abstraction on
  the branch: one writer for a consensus coordinate consumed by five paths,
  raw hooks structurally unreachable. The post-commit accessor-coherence
  assertion stays: it is the only guard in the two contexts with no database
  backstop (canonical RISC-V fold, `fold_replay`).
- **Two-regime lane loop, five-block frame clock, `WriteHead` cache posture,
  no-timeout reconciliation turn** — each traces to a documented failure or a
  documented revisit condition; deliberately absent machinery is recorded
  rather than omitted.
- **Test weight overall** — the high test ratios are earned; near-duplication
  is rare and noted per-area (mild: the three maintenance origin-preservation
  tests; scheduler nonce-focused pins).

**Proposals refuted by the adversarial pass** (do not re-propose without new
evidence):

- Unifying the three lifecycle settle functions — previously proposed and
  rejected after auditing fact boundaries; `settle_run_lifecycle`'s
  already-poisoned early return (`runtime/mod.rs:305-308`) is a real guard.
  H2 unifies the *bracket plumbing only*, never the settle policies.
- Merging `Workers::finish`'s two drain modes as sketched — the winning select
  arm consumes its `JoinHandle`; the naive merge turns a benign stop into a
  poisoned data directory.
- Removing the post-commit accessor-coherence assertion — see above.
- "Three-variant frame-drain writer family is bloat" — backwards: the branch
  *demoted* the raw physical writers to `#[cfg(test)]` and added attributed
  successors; the production surface has one way to write a frame.
- `FuturesUnordered` for cleanup polling — `futures-util` is a dev-dependency
  only; not worth promoting a dependency tree to delete one small hand-written
  future.

## Defects (D-items)

| # | Sev | Finding | Fix |
|---|---|---|---|
| D1 | P1 | `worker_exit_is_terminal` ends `_ => false` (`runtime/error.rs:226`): a new worker-error variant silently classifies non-terminal — fail-open on exactly the axis the branch made compiler-visible elsewhere. | Name every remaining arm now; H1 later makes terminality a method on each worker error type. |
| D2 | P1 | Admission bypass: `FeeOracle::start`/`new` still `pub` and `ShutdownSignal` publicly `Default`-constructible — an app crate can launch a DB-writing worker with no lock, lifecycle, or watchdog. Falsifies ADR "raw launch functions are crate-private" (5/6 were tightened). | `pub(crate)` the fee-oracle launch surface; gate `ShutdownSignal` construction (fully resolved by H2). |
| D3 | P2 | `reduce_recovery`'s `Repaired` arm ends in catch-all `status => Retry` (`recovery/mod.rs:268`); `classify_lifecycle` likewise — the ADR's headline compiler-visible property unmet where cheapest to fix. | Name the five remaining variants + `CanonicalDivergence(_) => unreachable!` mirroring `:283`. ~6 lines. |
| D4 | P2 | Classification drift already live: `VerifiedSignerProviderError::Create` (operator misconfig per `l1/provider.rs:116-118`) is Refuse→terminal in `recovery/mod.rs:495-497` but a plain run error in `setup.rs:639` / `flush.rs:92`. | **Fixed (2026-08-18):** one shared `From` classification (`BootstrapError::SignerMisconfig`, terminal) makes `Create` agree across setup/flush/run, following the dominant in-code semantic (`ChainIdMismatch` and `FeeOracleMisconfig` precedents: persistent misconfig → terminal). Recovery keeps its explicit polarity map. **Open taxonomy question, deliberately not smuggled into this fix:** whether misconfig should poison the lifecycle at all, or deserve a terminal-exit-without-poison verdict (the data is untouched; poison buys only don't-auto-restart plus ack toil). Decide once, then reclassify `ChainIdMismatch`/`SignerMisconfig`/`FeeOracleMisconfig` together. |
| D5 | P2 | Containment bit/cause publication window: `storage_invariant_contained` flips (`shutdown.rs:159-162`) before the cause publishes (`:173`); `is_storage_invariant_contained()` true while `containment_cause()` is `None`. | Merge both into one `OnceLock<String>`: `set(..).is_err()` is the CAS; `get().is_some()` is the bit. Removes a concept and the defect. Folded into H2. |
| D6 | P2 | `Storage::open` (`pub`, `open.rs:56`) on a fresh path runs `baseline_migration(None, ..)`: era minted, `history_state` written, **no lifecycle row** — a fourth state the ADR says does not exist ("database absence is Uninitialized", ADR:152). Production-unreachable today only by call ordering. | `pub(crate)` `Storage::open`; make the command-less baseline refuse (or test-only-explicit). |
| D7 | P2 | Snapshot routes 503 on `is_shutdown_requested()` (`egress/api/snapshot.rs:66-72`) where ADR:44-51 says immutable operator reads are not authority-bearing — the watchdog's byte-compare poll is refused during ordinary graceful drain. Also leaves open the pre-watchdog-arm window the containment bit covers. | Narrow to `is_storage_invariant_contained()`; pin with a test; fix the `invariants.md:63-65` wording to match. |
| D8 | — | ~~`admission.tla` narrowed vs code~~ **Retracted after adversarial verification (2026-08-18):** the restrictions in `RecoverTipCompleted`/`CascadeCompleted` are *faithful storage postconditions*, not drift — `cascade_and_reopen`/`recover_aging_tip_for_recovery` provably end with a valid open tip in their own transaction, the repair phases cannot reproduce Closed/TipDanger, and no repair contacts L1. Widening would model unreachable states. The Rust `Repaired ∧ Safe ∧ ¬tip` arm is production-dead parallelism, not a missed proof. | Resolved by documentation: the derivation now lives as a comment above the two actions in `admission.tla`, and the dead Rust arm is annotated. No model change; state counts stand. |
| D9 | P3 | `BootstrapError::FeeOracleTransient` documented "may self-heal" (`error.rs:328-330`) — the definition of `EXIT_RESTART_TRANSIENT` (20) — but classified Unclassified (1) at `error.rs:297-299`, test-pinned. Operator restart policy gets the wrong signal. | Decide and align classification + test. |
| D10 | P3 | `ApplicationProgress::new` panics inside the dump-decode path (`wallet.rs:109-113` via `from_snapshot_parts`): same corrupt-dump input, two failure protocols (`AppError::Internal` vs panic escaping `from_dump`'s `Result`). | Add `ApplicationProgress::try_new` for deserialization paths; keep `new`'s panic for in-code construction. |
| D11 | P3 | Genesis count check is `assert_eq!` across the app-crate boundary (`setup_fill.rs`; test needs `catch_unwind`). Keep the check — it is the only tie between the app's count and the durable base — but a foreign impl deserves a diagnosis, not a stack trace. | Typed `GenesisApplicationNotAtZero { count }` returned from `register_genesis_finalized_snapshot`. |

## S-A — The `Authorized` externalization token (structural)

The one remaining distributed terminal predicate:
`is_storage_invariant_contained()` read at ~11 hand-placed production sites
(`ingress/api.rs:84`; `inclusion_lane/mod.rs:166,244,308,342`;
`l1/submitter/poster.rs:224,230,282`; `egress/api/subscribe.rs:193`;
`workers.rs:404,783`), in two spellings inside the lane alone. Nothing forces
a new externalization site to consult it — the original bug class, one level
up.

Fix: `RuntimeScope::authorize() -> Option<Authorized<'_>>` returning a
zero-sized borrow-scoped token, required by the externalization primitives
(HTTP respond, batch submit, feed emit, snapshot-stream start). Forgetting the
check becomes a compile error. This is **not** the rejected `EffectGate`
(ADR:58, 646-647): no mutex, no actor, no runtime state, no second state
machine — the same predicate moved from convention into the signatures of the
operations it guards. Deletes
`stop_before_externalizing_after_storage_fault` and its inline duplicates.
Depends on H2 (a real `RuntimeScope` to hang `authorize()` on). Own reviewed
change (Wave 3).

## Harvest queue (H-items, PR-shaped)

### Wave 1 — deletions and residue (each independently landable)

- **H5 — Lane containment spray 7→5.** Delete `inclusion_lane/mod.rs:244-248`
  (byte-for-byte duplicate of the helper it follows), `:162` (subsumed), and
  one of `:160`/`:212`. Maintainer precedent exists one function away.
  Superseded entirely by S-A later.
- **H6 — Dead/dying surface.** (a) Collapse the `SafeInputRecord` shim
  (`storage/l1_inputs.rs`) to one honest row model — Track 6 already recorded
  the verdict; the shim fabricates zero timestamps/hashes into columns the
  feed serves as real L1 provenance. (b) Trim `EraId` text codec + serde
  (~75 lines, zero non-test callers; **keep** `Display` — `Debug` calls it —
  and the v4 bit validation). (c) `#[cfg(test)]` or delete the
  production-dead snapshot inserts (`insert_finalized_dump`,
  `insert_pending_dump`) — the branch replaced their production callers and
  left them public. (d) Delete `ExecutionOutcome::is_included` (zero callers,
  public consensus API). (e) `#[cfg(test)]` the cross-type `PartialEq`
  (`ProcessResult` ⟷ `ProcessOutcome`).
- **H7 — Async/spawn residue.** (a) Move `load_catchup_info` into
  `spawn_blocking`, making `L2TxFeed::subscribe_from`'s `async` honest;
  deletes both `catch_unwind`s, `panic_message`, and the
  unreachable-by-construction `StorageInvariantViolation` classifier arms
  (~50 lines). (b) Replace the `tokio::spawn` around the documented-sync
  containment call (`http.rs:195-199`) with a direct call — git-provable
  residue of the deleted async containment API.
- **H8 — One staleness predicate.** `has_elapsed_since` calls
  `protocol::age_exceeds` (argument order is swapped between them — a real
  future-unification hazard).
- **H10 — Required release reporter.** Drop
  `Option<PersistentReleaseFailureReporter>`; both production sites pass
  `Some`; tests take a no-op closure.
- **H11 — `BatchPoster` sheds `&ShutdownSignal`.** Field on
  `EthereumBatchPoster` (constructed after the signal exists); the gate stays
  in the send loop; trait, mock, and drivers shed the parameter.
- **H12 — `chain_id_validation.rs` seeding.** Move in-crate (or crate-internal
  seed helper) and seed via `initialize_for_command` + `complete_setup`
  instead of raw SQL that re-implements the lifecycle encoding.
- **H13 — Small readability items.** `preflight` + `unreachable!` puzzle →
  `expect_err` (`recovery`); move the two design-rationale doc blocks off
  `#[cfg(test)]` wrappers onto the production entry points; migration CHECK
  style: extend the `typeof()` guard to the SQL-arithmetic columns (dropping
  it is unsafe — TEXT/BLOB compare above INTEGER, so bare `>= 0` passes
  `'abc'`).

### Wave 2 — collapses

- **H1 — Worker-exit plumbing (~350 lines).** One generic
  `WorkerStop<E> { StoppedUnexpectedly, Source(E), Join(JoinError) }`;
  terminality becomes a method on each worker's error type (structural fix
  for D1); named `FirstExit` constructors replace the type-keyed `From`
  dispatch (the coherence hazard is live: a 7th worker returning
  `Result<(), InputReaderError>` would silently misroute today). Preserve:
  the select-vs-shutdown mapping distinction (`Ok(())` = clean drain on the
  shutdown path, `StoppedUnexpectedly` on the select path) and
  `DangerDetected` as a bespoke arm.
- **H2 — A real `RuntimeScope`; `ShutdownSignal` shrinks to its name.**
  Split: `ShutdownSignal` = notify + `is_shutdown_requested` (freely
  `Default`-able); `RuntimeScope` = `ProcessLock` + watchdog + containment
  (one `OnceLock<String>`, absorbing D5) + fault recorder, constructible only
  from a `ProcessLock` **at lock acquisition** (four install sites exist
  before any run signal — `setup.rs:126,226` et al. — matching ADR:117-121).
  Workers needing only cooperative stop take the signal; workers that touch
  the data dir or externalize take the scope. Deletes both `Option` fields,
  the four `Option<ProcessLock>` worker fields and setters, the fail-silent
  `retain_process_lock` installation protocol (H14 folded in), and D2's
  `Default` footgun. Lands the ADR's own vocabulary in the code.
- **H3 — Fee-oracle bootstrap behind its module (~120 lines net).** One
  `l1::fee_oracle::bootstrap(identity, rpc, max_age, scope)` with the
  transient/misconfig split inside; deletes `PreparedFeeOracle` and both
  80-line copies; callers map a two-variant error. If setup and run need
  different tolerance policies, pass one enum, not two copies of the
  algorithm.
- **H4 — Recovery type-stack trim (~60 lines, 4 types).**
  `RecoveryDriver::perform` returns `RecoveryProgress` directly; drop
  `RecoveryState`, `PhaseCompletion`, both witness structs,
  `transition_after_phase`. **Keep `RecoveryProgress`** (the ordering state
  machine) and the reducer's progress parameter untouched.
- **H9 — One blocking-storage-task idiom.** A shared spawn+join+classify
  helper for the snapshot endpoint's three spawn sites and
  reader/submitter's `map_storage_task_join` twins. Scope honestly: the three
  postures are deliberately different (worker-typed error → supervisor exit
  vs immediate containment vs feed) — consolidate the shape, not the
  posture, and leave the two `File::open`-NotFound sites out.

## L1 — Lifecycle representation (decided 2026-08-18)

**Decision: re-platform `storage/lifecycle.rs` on a current-state singleton
row + append-only audit side-table** (Wave 4, own change). Both deep reviews
agreed the write-ahead guarantee is right and disagreed only on
representation; the maintainer's calibration rule breaks the tie — the
validator's extra weight guards loud detection of a hand-corrupted historical
suffix that nothing consumes, while boot authority needs only the latest
state.

Preserved exactly: write-ahead ordering (admission updates the singleton and
appends the audit row in one `Immediate` transaction, re-deriving current
state first); `RunId` exact-record acknowledgement; operator-sticky stale
`Active` semantics; `refuse_on_canonical_divergence` at every entry;
append-only triggers + shape CHECKs on the audit table; single-row
enforcement on the singleton. The maintenance-origin invariant ("maintenance
does not erase recovery") becomes structural absence — the flush writes no
recovery columns — dissolving `MaintenanceFlushAdmission`'s origin-carrying
machinery and the three-record-window validator arm.

Deleted: `validate_history_edge` (~105 lines), `query_current`'s full-log
replay, the maintenance predecessor lookback, ~150 lines of ordered-history
tests. Given up, knowingly: loud detection of schema-valid history forgeries
below the latest row (the two forged-history tests move to pin the singleton
+ audit-consistency instead). Target: roughly one-third of the current 1,070
production lines, same guarantee surface, same external API
(`begin_lifecycle_command` / `admit_lifecycle_run_if` / `settle_*` /
`acknowledge_lifecycle` signatures unchanged so callers don't churn).

## P-items — premise, docs, model

**Status (2026-08-18):** P1, P2, P5, P6, P8 landed; P3's in-tree seed landed
(the lane test `reconciliation_digests_an_epoch_sized_outage_backlog_in_one_turn`
drains 5,000 directs over a 7,200-block jump in one turn — ~45 ms debug — with
the full ACK-latency-during-catch-up measurement still owed to the benchmark
harness); P7 was resolved as D8's retraction. **P4 remains an open maintainer
decision** (what the 500 ms contract is *for*), as does the D4 row's
misconfig-poison taxonomy question.

- **P1** Threat model: name the supported operator-mistake class (process
  lock, `RunId`, exact-record ack all defend an actor the table marks
  Trusted) so future reviews judge such mechanisms against a stated boundary.
- **P2** Pin I1's documented predicate omission with a test (I1 remains
  "review + tests only" — the theft-equivalent invariant deserves at least
  its pin). Note I11's three-consumer sync as the same shape.
- **P3** Synthetic digestibility test before any production claim: simulated
  multi-hour L1 outage → epoch-sized jump → measure ACK p99 *during* the
  catch-up reconciliation turn. The ADR's revisit trigger ("production
  measurements") discovers the failure on users.
- **P4** The 500 ms contract currently does no design work (8× above worst
  measured; nothing shaped by it). Either it encodes catch-up-overlap
  headroom (then P3 measures that) or restate the real objective.
- **P5** Flag era semantics at-risk in the Track 3 handoff: Bart's 2026-07-28
  confirmation covers the scalar generation contract only; changed-era
  bootstrap is explicitly unconfirmed. Schema is cheap to carry; wire
  projection must wait for his review.
- **P6** Doc-code sync: ADR vocabulary → code names (`RuntimeScope` after H2,
  `RecoveryDecision`/`RecoveryInspection`/`PhaseCompletion`/`reduce_recovery`);
  ADR §5 records only `K` where the code binds the pair (`+
  base_safe_input_index`); step-6 hot-path cost is three queries per chunk,
  not one; `application-contract.md` documents `AppError::Io` as fatal (all
  callers already treat it so); move the frame-clock policy section out of
  the canonical-acceptance doc's fold section (or cross-mark it); move
  `FRAME_CLOCK_INTERVAL_SAFE_BLOCKS` to `ProtocolTiming` beside its siblings.
- **P7** = D8 (widen the admission model).
- **P8** Runbook: distinguish first stale-`Active`-after-known-cause from
  repeated ones; decide the supervised auto-ack policy at the orchestrator
  layer now, before operators invent scripted acks (reflexive ack ≡
  automatic inspection; the design degrades safely, but decide it on
  purpose).

## Wave plan

| Wave | Content | Status |
|---|---|---|
| 0 | Defects D1–D11 (D5 via the narrow OnceLock merge; D8 retracted with the derivation documented) | **landed 2026-08-18** (`052d4e5`, `c5347d3`, `f0e02b9`, `d9e09af`, `efaa67d`) |
| 1 | H5–H8, H10–H13 (deletions/residue) | **landed 2026-08-18** (`85f349a`, `dd2dda3`) |
| 2 | H1–H4, H9 (collapses; H2 absorbed D2's completion and H14) | **landed 2026-08-18** (`ed41f9b`, `69563ed`, `dd05e28`, `b334829`, `389ad66`) |
| 3 | S-A `Authorized` token | **landed 2026-08-18** (`be128bd`), adversarially re-reviewed post-commit |
| 4 | L1 lifecycle re-platform (own change) | **landed 2026-08-18**; production 1,070 → 905 lines — short of the ⅓ target because the enum/API/error skeleton (~450 lines) is representation-independent, but the conceptual weight is gone: no history replay, no 105-line edge validator, no three-record maintenance window, no origin-carrying token, no dual encoding of the transition table; verdict preservation and once-per-database completion became engine CHECKs and write-boundary refusals. **Post-landing adversarial review (2026-08-19, 3 lenses, per-finding verification): 1 confirmed / 7 refuted — the completion rule had been half-ported (run could begin before setup completion at the storage boundary; unreachable in production behind the runtime's own gate). Fixed same day: `require_beginnable` is two-sided and both directions are test-pinned.** |
| — | P-items land continuously with their waves | **landed 2026-08-18** except P4 (maintainer decision) and P3's harness measurement |

## Addendum — module homing decisions (2026-08-19)

Follow-on layout review of `sequencer/src/runtime/` with the maintainer,
applying the same brackets-vs-mechanisms lens the waves surfaced:

- **`commands/` split (landed).** `runtime/` was renamed on its true seam:
  the command *brackets* now live in `commands/` (`run/` with its worker
  supervisor, `setup/` with its fill phase, `flush`, and exact-run
  acknowledgement — the fourth command that had been invisible inside
  `runtime/mod.rs`), while `runtime/` keeps the shared authority machinery
  (config, error taxonomy + exit projection, process lock, runtime
  scope/shutdown). Mechanisms stay in their domain modules
  (`crate::recovery`); brackets invoke them. Public API unchanged
  (`sequencer::run` re-exported; `sequencer::runtime::RunError` et al.
  untouched).
- **`clock.rs` hoisted to the crate root (landed).** The crate-wide wall
  clock lived under `runtime/` while `storage/`, `recovery/`, and
  `l1/fee_oracle` depended on it — an inverted layering arrow for a
  42-line helper with no runtime-specific content.
- **`http.rs` stays, deliberately.** Its three tenants (shared `ApiError`,
  serve orchestration, snapshot lease-release supervisor) are a known mix,
  but at ~380 lines the split fails the weight test today. Decision: revisit
  when the ingress/egress servers split into separate listeners — that
  change forces the file apart along its natural seams anyway.
- **Deferred, recorded:** the `RunError` rename (`CommandError`?) — the
  shared error type is named after one command, which is most of why the old
  module read run-centric; and the command-bracket combinator (Book item 2)
  remains available if the three brackets ever drift again.

Second pass (same day, with the maintainer): the merge question — "should
`runtime/` fold into `commands/` entirely?" — was settled by consumer
analysis. `config`/`error`/`test_support` are consumed only by the commands
and the harness, so they moved under `commands/` (and the deferred rename
landed with them: `RunError` → `CommandError`, `RunFailureVerdict` →
`CommandFailureVerdict` — the shared taxonomy is no longer named after one
command). `shutdown`/`process_lock` are consumed by the entire data plane
(egress ×7, ingress ×3, l1 ×4, http, recovery), so `runtime/` now holds
exactly the ADR's structured-process-ownership capabilities and nothing
else; a full merge would have made `commands` both the top-level
orchestrator and the bottom-layer capability provider. Two arrows were
untangled to make it true: `L1Config` rehomed to `l1/` (recovery and the
submitter consume it — it was never command config), and `process_lock`
grew its own two-variant error so the substrate no longer imports the
command taxonomy (`commands::error` owns the conversion and its
retry-safe classification).

## L2 — Lifecycle admission gating removed (decided and landed 2026-08-19)

> **Superseded in part by L3 (2026-08-22):** the write-only journal this
> section leaves in place was itself narrowed to a terminal-fault black box
> after a fresh-eyes review — see
> [`2026-08-22-lifecycle-simplification.md`](2026-08-22-lifecycle-simplification.md).

Follow-on from the maintainer's two-recovery analysis, which pulled the L1
thread to its end. The trigger was the consumption audit of the lifecycle
state machine: the operator acknowledgement carried **no decision the
machine consumed** (ack routed to `NeedsRecovery`, which every admission
treated identically to `Ready`; the run reducer never read lifecycle fields
at all), and the states collapsed to a binary may-begin/human-gate whose
payloads were audit vocabulary. The recovery model is: **standard recovery
is automatic** (fact-derived reducer, no intervention, consumers get the
generation bump); **cockroach recovery is manual and clean-slate**
(detected externally, typically by the watchdog; fresh-directory rebuild).
The operator brings no in-band information, so the in-band gates went:

- `clear-terminal-fault` (the fourth command) deleted, with its CLI,
  config, and `RunId::parse_canonical`.
- The `runtime_lifecycle` singleton state machine deleted; what remains is
  `runtime_lifecycle_journal`, a write-only trail (began / live / settled /
  terminal-cause; a `began` row with no settlement is an unclean death's
  tombstone). Nothing reads it for decisions.
- Admission is three facts, each with one owner: the kernel process lock
  (concurrent owners — `Active` was always redundant with it for refusal),
  two-sided `setup_complete` (command ordering), `canonical_divergence`
  (absorbing; cockroach only). `safe_input_floor_in`'s escape hatch
  simplified to the completion fact alone.
- Crash-loop throttling is owned by the R4 exit contract (30 = do not
  restart, page), where it always mechanically lived; the DB gate was a
  second enforcement of the same ops policy.
- G2's enforceable content — no boot silently fast-paths — was always
  carried by the unconditional reducer inspection, which is unchanged.

**Accepted trades, eyes open:** (a) a known-terminal fault refuses at
*re-detection*, not at boot — a restarted instance can serve until the
faulty state is next read (cold-row corruption may take a while to re-trip;
exit-30 paging means an operator is already investigating, and the honesty
backstops — rollbackable soft confirmations, watchdog byte-compare,
divergence freeze — never depended on the boot gate); (b) a supervisor
misconfigured to restart on exit 30 will loop through re-detection windows
(that supervisor would have scripted the acknowledgement anyway); (c) the
G2 "stickiness" language in the ADR is superseded (amendment note added at
mechanism 2). `storage/lifecycle.rs`: 905 → ~537 production lines (journal + fact
checks); the ADR's transition table, the maintenance origin machinery's
successor CASE, the ack protocol, and the ABA token role are all gone.

**Post-landing adversarial review of L2 (2026-08-19; 23 agents, 3 lenses,
per-finding verification): 11 confirmed / 9 refuted — all confirmed findings
were documentation and telemetry honesty, no runtime defects.** The clusters,
all fixed the same day: `admission.tla` still modeled the deleted state
machine and acknowledgement (rewritten — the `lifecycle` variable became the
attempt's journal progression `NotBegun → Began → Live`, `AcknowledgeStale`
and `StickyCrash` deleted, crash/restart modeled as a fresh attempt; TLC
re-verified 860 generated / 266 distinct / depth 13 / 0 violations, README
claims and counts updated); the top-level README's Failure Modes still
instructed operators to acknowledge stale runs (rewritten); AGENTS.md's flush
bullet and command list still stated the origin-restoring admission
(rewritten); the fault-recorder failure logs and the R4 SIGABRT comment still
promised the deleted stale-`Active` boot refusal (rewritten to the honest
telemetry framing); invariants.md I18 and the reducer bullet still named
`Active(Starting)`/`Active(Live)` (rewritten, including the floor-predicate
description matching the simplified `setup_complete` check).
