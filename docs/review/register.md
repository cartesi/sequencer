# The Review Register

The distilled outcome of every dated review ledger in this directory: what
is still **open**, what was **settled** (with its reasoning's current home),
and what was **refuted** and must not be re-proposed without new evidence.
Check the open section before touching related code; check the refuted
section before proposing a simplification or a new mechanism. The dated
ledgers beside this file are stubs preserving each review's scope and
verdict; process detail beyond that lives in git history.

Statuses of findings 1–18 were verified against the tree on 2026-08-25.
Findings 19–31 and the 2026-09-03 refuted block come from the
[branch stock-take](2026-09-03-branch-stocktake.md), which records every
proposal that review raised, including the ones no jury examined.

## Findings

Code findings, oldest first (file references are starting points, not exact
lines). Numbers are stable identifiers: a closed finding keeps its number and
is reduced to a one-line closure note, so citations in the dated ledgers stay
valid.

1. **Submitter confirmation-timeout defeats pacing** — a watch timeout maps
   to a successful `Submitted` tick, so the next tick immediately re-sends
   the same payloads at the same nonces (usually "replacement underpriced")
   before any sleep. `l1/submitter/poster.rs` + `worker.rs`; add a distinct
   outcome that sleeps.
2. **A transient `SQLITE_BUSY` costs the submitter a full respawn** — 50 ms
   reader `busy_timeout` plus every non-poster error ending the run. It now
   classifies restartable rather than terminal, but the respawn+recovery
   cost stands. Retry BUSY or use the writer-grade timeout for these reads.
3. **An undecodable own-sender payload stalls submission for the safe-lag**
   — the poster hard-fails decode where both scheduler mirrors
   skip-and-continue, so an operator's manual tx from the submitter EOA
   wedges ticks until the block passes the safe head. Skip undecodable
   own-sender payloads.
4. **Closed** (2026-09-03): the flusher's healthy retry pass logs at
   `warn!`, not `error!`.
5. **Closed** (2026-09-03): the application-error 500 body is the fixed
   "application internal error"; the reason stays on the lane error and the
   log.
6. **WS session hygiene** — a mid-session transient read error tears down
   with no close frame; a beyond-head `from_offset` idles forever (currently
   e2e-pinned as intended — decide the contract, then re-pin).
7. **WS invalidation/rollback contract** — `/ws/subscribe` still pages by
   physical rowid with no `HistoryVersion` claim, so a cursor-resumed
   subscriber silently keeps invalidated rows across recovery. Interim
   consumer rule: treat any socket drop as a potential discontinuity.
   Closure is exclusively owned by the
   [Track 3 handoff](../plans/2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff).
8. **Fee-determinism contract under-specified** — the LSB-first
   floor-after-each-multiply order is implemented but not stated as contract
   (`sequencer-core/src/fee.rs`). Load-bearing for the C++ scheduler port;
   interacts with the deferred fee-LUT track. (The `fixed_mul` comment that
   claimed a nonexistent `debug_assert` now states what the truncation
   relies on: the `MAX_EXPONENT` bound upstream — closed 2026-09-03.)
9. **`trg_enforce_nonce_contiguity` NULL hole** — a dangling parent makes
    the comparison NULL and the trigger silent; mitigated by `foreign_keys=ON`
    on every writer connection, but the trigger itself is not NULL-safe.
10. **`seal_and_open_next_batch` takes an unchecked `next_safe_block`**
    (assert equality with the head or drop the parameter). (The bare
    `close_frame_and_batch` is `#[cfg(test)]` as of 2026-09-03.)
11. **Write-only columns** `safe_accepted_batches.{first_frame_safe_block,
    inclusion_block}` have no production reader — drop or mark audit-only.
12. **`direct_q` is unbounded in the shared scheduler** — an adversarial
    deposit flood is bounded in time (force-drain) but not bytes; a
    per-input cap or byte budget closes a (very expensive) guest-OOM vector.
13. **`MAX_BATCH_METADATA_BYTES` (71) understates real SSZ per-op overhead**
    (~83+ with offsets) — byte budgeting undercounts ~15% for max-payload
    ops (`sequencer-core/src/user_op.rs`).
14. **Wallet snapshot decode accepts unsorted entries** while encode sorts —
    enforce strictly-ascending addresses (subsumes the duplicate check) or
    drop the canonical-decode pretense (`app-core/src/wallet_snapshot.rs`).
15. **Reader and submitter re-open `Storage` per tick** — a held connection
    per worker drops per-tick overhead. Low priority.
16. **`should_retry_with_partition` substring-matches the Debug format** —
    consciously accepted and regression-pinned against alloy's format;
    revisit with structured JSON-RPC codes.
17. **Closed** (2026-09-03): `latest_batch_index`,
    `ordered_l2_txs_for_batch`, and `promote_finalized` are `#[cfg(test)]`
    (gated rather than deleted — the first two have callers across three test
    modules). `safe_input_end_exclusive` has a live reader-path caller and
    stays.
18. **`frames` lacks the immutability triggers `batches` got** — `fee` and
    `safe_block` are documented immutable but convention-protected only.
19. **A missing or unreadable key file exits 1 and restart-loops** —
    `resolve_key_source` (`commands/config.rs`) returns a bare `io::Error`
    that lands in `CommandError::Io`, while bad key *content* in the same file
    exits 30. Kind-filter into a distinct `KeySourceUnreadable { path, kind }`
    (NotFound/PermissionDenied/InvalidData/IsADirectory/NotADirectory
    terminal; EIO and friends operational); never echo contents.
    Jury-confirmed.
20. **`StaleDecision` cannot name the tip-present cause** —
    `ensure_open_tip_for_recovery` raises `{ expected: Safe, actual: Safe }`
    when the Tip already exists, and `recovery_tests.rs` pins that line. Add
    `TipAlreadyOpen` plus a paired retry reason so `classify_mutation` carries
    it to the operator. Jury-confirmed; production-unreachable.
21. **Closed** (2026-09-03): `Storage::ensure_open_tip` is
    `#[cfg(test)] pub(crate)`; the two intra-doc links and the snapshot
    lifecycle doc now name the reducer's guarded `EnsureOpenTip` phase.
22. **Detector and reader take `RuntimeScope` where `ShutdownSignal`
    suffices** — each uses the scope only for `wait_for_shutdown` and already
    holds a construction-required `ProcessLock`. Narrow, and restate the three
    doc comments (`commands/run/workers.rs`, `runtime/shutdown.rs`) that claim
    lock retention through scope clones. Jury-confirmed 2–1.
23. **`drive_recovery`'s one cycle has no enforced postcondition** —
    `Repaired` + `Safe` + no Tip → `EnsureOpenTip` → `Repaired`, with no
    watchdog on the boot path; `admission.tla` hardcodes `hasOpenTip' = TRUE`.
    Return a typed `refuse` (exit 30 — not a `debug_assert`, not
    `StaleDecision`) after `open_fresh_tip_in_tx` in the guarded phase, or make
    the reducer arm a terminal `Refuse`. Jury-confirmed 2–1.
24. **A contained run writes two `terminal_faults` rows** — the in-scope
    recorder's raw cause, then the bracket's prefixed cause;
    `latest_terminal_fault` returns the second. Document the shape (recorder,
    ADR, runbook); do not dedupe by variant — the recorder swallows its own
    failures, so the bracket write is a genuine retry. Jury-confirmed.
25. **Closed** (2026-09-03): the token's doc, the ADR, and the settled
    entry state its true scope (three compile-forced primitives; the rest
    hand-placed); "non-clone" became "boot-local"; the witness comment names
    every holder; the "journal", "acknowledge", and `DangerDetectorExit`
    remnants are gone.
26. **`finalized_state` handles an impossible `None`** — `LeasedDump` is
    shared between the finalized and latest lease queries, so the `NOT NULL`
    `inclusion_block` arrives as an `Option` and a `None` escalates to
    containment (`egress/api/snapshot.rs`). Give the finalized lease its own
    non-optional type and delete the branch. Verified first-hand; no jury.
27. **`RecoveryFailure::Provider(String)` carries two verdicts** — the
    startup flush maps `ChainIdRpc` → retry and `Create` → refuse into one
    variant, and nothing pins either arm; `classify_input_reader` has no doc
    comment and its `Bootstrap`/`Join` refusals are unpinned; a pre-v3
    InputBox exits 1 under `setup` and 30 under `run`. Surfaced by the
    refuters; split the variant and pin both polarities.
28. **Setup admission is implemented twice with different semantics** —
    `preflight_lifecycle_command` (used by `run`/`flush`) and
    `admit_setup_lifecycle` (`commands/setup/mod.rs`) each check the same two
    facts; an already-complete plain setup is a no-op in one and
    `NotAdmissible` in the other, and `load_setup_identity` re-checks
    completion with a third error type. Unverified.
29. **`http.rs` hosts a runtime component** — the ~175-line supervised
    lease-release queue with two containment sites belongs in the egress
    snapshot module that owns leases; its queue is unbounded and capped only by
    concurrent snapshot requests. Unverified; weight, not safety.
30. **Harness block-time notions are unreconciled** — four constants with an
    unasserted relationship; `advance_live_frame_until_covers` can drive L1
    ~20× ahead of the process clock; `aging_open_tip_runtime_danger_zone_exit_test`
    stages an impossible chain and greps the rendered log (the CI red). One
    `ChainClock` authority plus a post-mining drift check; assert exit 10
    instead of the log. The log grep is verified first-hand; the rest is
    unverified.
31. **`log_gas_price_updated_at_ms` is production-write-only** — written on
    every refresh, read only by tests, yet the threat model cites it as the
    telemetry that justifies having no expiry gate. Surface it (health field
    or the transient-refresh warn) or drop it and fix the sentence.
    Unverified.

Open maintainer decisions:

- **What the 500 ms acknowledgement contract is *for*** — it is 8× above the
  worst measured value and shapes nothing today; either it encodes
  catch-up-overlap headroom (then measure that) or restate the objective.
- **Catch-up ACK-latency measurement** owed to the benchmark harness: ACK
  p99 *during* an epoch-sized catch-up reconciliation turn (the in-crate
  seed test digests 5,000 directs over a 7,200-block jump in one turn).
- **Track 6 with Bart**: the hardlink-suitability dispute and the
  changed-era bootstrap contract (see the tracks doc).

## Owed tests

- **Arm-ordering discriminating test**: both `ClosedBatchInDanger` and
  `TipInDanger` genuinely in danger; assert Closed wins (today pinned only
  incidentally by an equally-aged fixture).
- **Fail-loud halves**: no test references `CatchUpError::NoSnapshot` or
  `InclusionLaneError::NoOpenTip`.
- **`EstimatedBatchInDanger` e2e** (recipe: mine ~800 blocks, faketime
  +30 min without mining, respawn → refusal with zero invalidations).
- **Process-level divergence scenario** via `respawn_until_stable` (storage
  and reducer coverage exists; the end-to-end freeze/refuse loop does not).
- **Per-variant exit-code e2e assertions** (failure-path e2es assert only
  `!success()`) and a process-level SIGTERM→0 assertion. Assert integer
  literals: the `EXIT_*` values appear only at their declarations, so
  renumbering `EXIT_TERMINAL` passes the suite today. Cheapest 30-class case:
  `run` on a never-set-up data directory.
- **Polarity pins for `classify_input_reader`'s `Bootstrap`/`Join` refusals**,
  and a unit test of the production phase→progress mapping in
  `ProductionRecoveryDriver::perform` (the scripted-driver traces exercise the
  test double's copy of it).
- **Full-tear cascade on a recovered (anchor = `N'`) tree** re-rooting at
  `N'` (anchor unit mechanics are covered; this end-to-end shape is not).
- **Uniswap-mode fee oracle end-to-end**: every fixture and e2e pins fixed
  mode, so no Uniswap-mode sequencer boots in tests. Setup validation,
  RPC-free runtime source construction, transient quote retention, and
  terminal misconfiguration are source-boundary-pinned in-crate as of
  2026-08-25; a real E2E still needs a mock pool — decide whether that extra
  harness is worth its weight.
- **True same-block direct-input ordering end-to-end**: the renamed
  `multi_deposit_reconciliation_test` covers multiple accumulated directs, but
  default Anvil automining puts its portal deposits in distinct blocks. A real
  same-block test needs queued portal sends, one explicit mine, equal receipt
  block assertions, and WS order/block attribution.
- **Verify-then-write-or-strike** (status uncertain on 2026-08-22): the
  encoded-wire-frame stamp at an advanced safe head; the wallet
  insufficient-balance silent no-op and replay-determinism pins; the
  young-never-submitted-batch cascade-policy pin; the `recover_aging_tip`
  torn/no-Tip entry; the cascade-with-backward-clock pin.
- **The batch-close failure half of I7**: pre-insert a `dumps` row with a
  colliding prefix so the seal transaction fails on UNIQUE, and assert the
  batch stays the open Tip. Companion state variant: delete the directory
  under a DB-referenced snapshot row and assert the loud terminal shape
  (the WAL-rewind *cause* stays unsimulable).
- **Harness levers to build with their tests**: pending-tx capture +
  re-inject (`txpool_content`/raw-tx before `drop_all_pending_txs`, then
  `eth_sendRawTransaction`) → unlocks the zombie e2e, the headline
  adversarial scenario; snapshot lifecycle observability (DB readers for
  the snapshot tables + dump-dir inspection) → the take/promote/GC/lease
  e2e, plus finally *asserting* warm-resume-from-dump (every restart test
  exercises it, none asserts it — a silent fall-back to genesis replay
  would pass everything, just slower); a bare second Anvil
  (`--chain-id <other>`) + mid-run endpoint override → the wrong-chain
  e2e; kill-at-log-marker → the flush-completion/cascade-commit crash
  window; SQLITE_BUSY injection → the submitter/WS BUSY items.
  **Recorded do-not-build**: a split-view/response-rewriting L7 proxy
  (unit-level provider mocks instead; e2e validates only the passing
  path) and fsync/power-loss WAL-rewind injection (out of scope —
  state-construction variants cover the detectable halves).

## Settled decisions

Each entry: the decision, its reason, and where the reasoning now lives.

- **Write-before-broadcast watermark** (2026-06): the flush's completion
  anchor is durable, not the local pool's memory → I14.
- **Content-identity check, gated on full acceptance** (2026-06): accepted
  landings compare by content hash; content-equal copies are effect-equal,
  so no batch identifier is needed; detection freezes the frontier
  atomically with the detecting sync → I9, I15.
- **`synchronous=FULL`** (2026-06): externalization rides on commits, so
  every commit fsyncs; noise-level cost on NVMe → `storage/open.rs` doc.
- **Cockroach recovery's flush is best-effort by construction** (2026-06):
  the wiped DB destroys the watermark, so the flush resolves only what the
  provider remembers, and plain `setup`'s detection gate shares the same
  false negative. Accepted because the content-identity check turns the
  residual zombie from silent divergence into a detected freeze (repair:
  wipe and re-run). Recorded option if ever needed: an operator-supplied
  flush floor from the old DB's watermark — fail-safe under corruption
  (too high wastes no-ops; too low degrades to exactly best-effort) →
  `cockroach.md` step 2.
- **Exit-code contract** (2026-06, panics-terminal amendment 2026-07): the
  orchestrator must not parse logs; 10/20/30/40 by restart productivity →
  `commands/error.rs` + the operator runbook.
- **Fail-loud check policy replaces "no defense-in-depth"** (2026-06): the
  line is loud-vs-silent, not self-doubt; assertions must check real
  invariants (the wall-clock CHECK cautionary tale) → the invariants check
  policy.
- **Scoped pending clear** (2026-06): delete only pending rows at/above the
  cascade pivot, in the cascade's transaction → I5.
- **Batch-tree anchor, not a sealed sentinel** (2026-06-25): the parentless
  root carries the anchor nonce, exact-matched by the contiguity trigger →
  I16.
- **`N` is trusted; no recovery-time verifier** (2026-06-26): a
  sequencer-produced finalized dump cannot carry a wrong `N` by
  construction; only wrong-low is caught at `run` → `cockroach.md` data
  dictionary.
- **Anchor-aware frontier; recovery defers population** (2026-06-26):
  below-anchor landings are trusted collapsed history → I15.
- **Recovery drain caps at `C`** (2026-06): `(C, H1]` deposits stay
  undrained so `run` leads them exactly once → `cockroach.md` steps 3/6.
- **Don't resurrect TEST_PLAN.md** (2026-06): the scenario matrix rotted
  once; owed tests live here as a dated, finite list.
- **The authority boundary** (2026-08, re-evaluated 2026-08-02): four
  mechanisms — `RuntimeScope`, fact-derived admission, the pure recovery
  reducer, SQLite-centered runtime with the two-regime lane → the
  [ADR](../plans/2026-08-authority-boundary-adr.md).
- **Storage decode policy** (2026-07): fail-loud for contract-impossible
  values; the named `saturating_query_bound` only where clamping preserves
  the predicate → `storage/convert.rs` + the check policy.
- **The calibration rule** (2026-08-18): the complexity budget belongs to
  concurrency, durability, and hostile-L1 robustness → AGENTS.md design
  principles.
- **The `Authorized` externalization token** (2026-08-18): the containment
  consult is a compile-time obligation of the three effect functions that
  take the token (ack, L1 send, WS emit); the snapshot-stream start, the
  `POST /tx` success body, and the lane's mutation commits are hand-placed
  consults bounded by the exit contract →
  `runtime/shutdown.rs`, ADR mechanism 1.
- **Module homing** (2026-08-19): command brackets in `commands/` (with
  config + the `CommandError` taxonomy), the capability substrate alone in
  `runtime/`, `L1Config` in `l1/`; a full merge was refused because the
  substrate is consumed crate-wide. `http.rs` stays whole until the
  ingress/egress listener split forces it apart.
- **Lifecycle: facts govern; the black box records** (L1 2026-08-18 →
  L2 2026-08-19 → L3 2026-08-22): admission is three facts; no state
  machine, no acknowledgement (it carried no machine-consumed decision);
  telemetry writes are verdict-neutral; terminal faults refuse at
  re-detection with the residual recorded in the threat model → the ADR,
  the invariants check policy, and the two dated ledgers.
- **Misconfig-poison taxonomy** (opened 2026-08-18, closed by L3): there is
  no poison; misconfig is terminal by exit code only, and a fixed config
  boots cleanly.
- **Submitter-key redaction** (2026-08-24): the key enters the process as
  `SubmitterKey` at the clap edge — `Debug` redacts, no `Display` exists,
  and the raw hex is reachable only through `expose_secret`, so every
  consumer of the secret is greppable. The key's public identity is the
  pinned `batch_submitter_address` beside it. Closes the former
  Debug-derive open finding → `l1/mod.rs`. Deferred separately: the startup
  log prints the full RPC URL, which the help-leak test treats as
  token-bearing.

## Refuted — do not re-propose without new evidence

From the 2026-06 reviews:

- **`scheduler_accepts` omitting the two structural rejections is a bug** —
  deliberate self-trust; the simulator runs only over our own well-formed
  batches; the worst case is covered by the content-identity check
  (documented in `scheduler-semantics.md`, duality-test-pinned).
- **Sealed `N'-1` sentinel batch** for recovery rooting — a valid closed
  sentinel is a legal cascade pivot; nothing stops a runtime cascade from
  invalidating it, after which recovery ABORTs. Its safety rested on an
  unenforced assumption.
- **Recovery-time `N` cross-check** — circular: every cheap recomputation
  seeds from the `N` it would check; the only independent check is a
  from-genesis L1 replay, deliberately not built.
- **`FoldInputSource` abstraction** — a wrapper over a single call site;
  revisit only if a second fold-input source appears.
- Wire-fee-exponent panics, stalled-WS DoS, operator `WalletConfig` dead on
  warm start, snapshot bytes history-dependent — each verified as
  deliberate/out-of-scope (see the threat model's scoping).

From the ADR re-evaluation (2026-08-01/02):

- **`RunEpoch`**, **`EffectGate`**, **`LiveKernel`** — see the ADR's
  rejected-alternatives section for each argument.
- **A generic command controller** over setup/rebuild/run/maintenance —
  unrelated facts; a larger state machine closing no hole.
- **A per-chunk divergence query / reader mailbox** on the hot path — the
  check is not a divergence oracle; the cost buys no complete boundary.
From the 2026-08-18 adversarial pass:

- **Merging `Workers::finish`'s two drain modes by re-awaiting the primary**
  — the winning select arm consumed the handle's completion (the select
  borrows `&mut self.server` etc., so the handle is still in the cleanup
  set); re-polling it panics ("JoinHandle polled after completion"),
  unwinding through `ShutdownOnDrop` into containment — a benign stop
  becomes a poisoned data directory. Scope-narrowed 2026-08-23: the 2026-08
  source read "as sketched" / "the naive merge"; distillation dropped the
  qualifier. A merge that removes the primary from the cleanup set before
  draining — keeping the `expect` that it was present — is settled, not
  refuted (landed as `finish`'s one-loop/two-phase shape).
- **Removing the post-commit accessor-coherence assertion** — it is the
  only guard in the two contexts with no database backstop (canonical
  RISC-V fold, `fold_replay`).
- **"The three-variant frame-drain writer family is bloat"** — backwards:
  the raw physical writers are `#[cfg(test)]`-demoted; production has one
  way to write a frame.
- **`FuturesUnordered` for cleanup polling** — not worth promoting a
  dev-only dependency tree to delete one small hand-written future.

From the 2026-08-23 run-glue simplification pass:

- **`Workers` as a homogeneous component list** (a `Vec<(WorkerId,
  ComponentShutdown)>` built at launch, one select arm racing the list) —
  it makes the "no `.await` between `Poll::Ready` and `swap_remove`"
  property `select!`-load-bearing and untested (violation loses a worker
  exit *and* panics on re-poll); converts the asserted primary-in-set
  precondition into an unchecked cross-function assumption whose failure is
  the benign-stop-to-exit-30 outcome; deletes the composed detector
  select-mapping tests; and makes a zero-component race representable. The
  real hole it targeted (select arms were the one per-worker site not
  compile-forced) is closed by the exhaustive `let Self { .. }` destructure
  in `select_first_exit` instead.
- **`UniswapConfig::pinned(&identity)`** — the exhaustive
  `FeeOracleIdentity` match in the run bracket IS the launch decision for
  the optional oracle worker; moving it behind an `Option<UniswapConfig>`
  constructor makes a future identity variant compile while silently
  launching no worker, and the chain-id-pairing guarantee it claimed is
  already structural now that the identity travels whole inside `L1Config`.

From the L3 review (2026-08-22):

- **A durable boot gate on terminal verdicts** (a gate on a *verdict*, i.e.
  a non-fact) — it needs an acknowledgement to exit, and the acknowledgement
  carries no information the fact-derived reducer doesn't re-derive. A
  verdict-neutral startup *read* of the black box is not covered by this
  entry.
- **A boot-time full-integrity sweep** — expensive machinery that still
  cannot catch semantic violations outside its read set; the residual
  window is recorded and bounded instead.
- **A boot assert on `finalized_snapshot.inclusion_block`** — the column is
  `NOT NULL` at the engine; the claimed gap does not exist. (The runtime
  `Option` branch on the same column is finding 26.)

From the 2026-08-25 fee-oracle lifecycle pass (143a290; a single-pass
decision, not an adversarial review):

- **Fee-price age as a runtime lifecycle gate** — setup owns the required
  live quote; run starts from the persisted price and refreshes it best-effort.
  Shared-endpoint staleness is already detected from safe-head progress, while
  a pool-only outage is an explicitly accepted economic residual. Reintroduce
  an expiry gate only with an independently derived economic bound and action,
  not by borrowing the L1 liveness threshold. The telemetry this entry leans
  on is production-write-only (finding 31).

From the 2026-09-03 branch stock-take (three refuters per proposal; full
reasoning in the [ledger](2026-09-03-branch-stocktake.md)):

- **`Workers` on a `JoinSet` fed by the shutdown waiters** — `from_select`
  and `from_shutdown` deliberately read a worker's clean `Ok(())` two ways
  (live: stopped unexpectedly; drain: graceful); one set collapses them and a
  silently dead lane becomes exit 0. Evidence: `commands/error.rs:606-627`,
  `commands/run/workers.rs:339-390`. The `swap_remove` hazard is real; a
  cleanup-only set built inside `finish`, with the live race untouched, was
  not examined.
- **Collapsing `PreparedRuntime` into an `async fn boot`** —
  `fn launch(self, RuntimeAdmission) -> Workers` is non-async and non-`Result`,
  so `?` and `.await` after admission are compile errors today; `boot` makes
  them legal and the witness degenerates. Evidence: `workers.rs:288`,
  `recovery/mod.rs:477-483`; 02a2b34 stopped here deliberately.
- **Making the `Authorized` token "uniform" by demoting the `/tx` 200 gate
  to a bool** — the 200 body is the acknowledgement leaving the process;
  `LeasedDumpBody` does not exist and `finalized_inclusion_block` has no
  streaming primitive. Evidence: `ingress/api.rs:84-92`,
  `egress/api/snapshot.rs:101-120,214`. The doc tightening survives (finding
  25).
- **Deleting the `#[from]` impls so a refusal reason cannot be typed into a
  retry** — the enums are public with public variants; the longer spelling
  still compiles and is the dominant idiom at all 15 sites. Evidence:
  `recovery/mod.rs:45-49,109-115`.
- **One `is_terminal()` on `VerifiedSignerProviderError` read by both
  tables** — the `From` impl performs no classification; the verdict is taken
  later over `BootstrapError`, whose variants have four other producers.
  Evidence: `commands/error.rs:671-683,216-260`; `l1/reader.rs:96-104`
  already documents the phase pair.
- **Flattening `RecoveryError` into one enum with `is_retryable()`** —
  `RecoveryFailure::Provider(String)` carries two verdicts, so a total
  function over the value does not exist. Evidence: `recovery/mod.rs:429-438`.
  Splitting the variant is a separate, sound fix (finding 27).
- **Moving the phase→progress mapping into `drive_recovery`** — `(Flush,
  Done)` has no target without the observed block; this is
  `PhaseCompletion`/`transition_after_phase`, deleted by ed41f9b. Deleting
  `RecoveryDriver::admitted` survives.
- **Merging `FeeOracleMisconfig`/`FeeOracleFatal` and
  `ChainIdRpc`/`DetectionNonceRead`** — bare-string producers at
  `commands/setup/mod.rs:144,171` would misattribute an operator mistake in
  the black box; the second merge demotes a compile-forced classification to a
  string discriminant (refuted twice on 2026-08-23).
- **A `TerminalityOf` trait for `WorkerStop`** — admits the same wrong
  classification as `|_| false`, installs a crate-wide answer for `io::Error`
  that `dump_info.rs:49-58` contradicts, trips `private_bounds` under
  `-D warnings`, and splits an inherent convention across eight types.
- **Wiring `check-admission` into CI as one line, and pruning three
  "duplicate" settle actions** — no Nix or `.envrc` is tracked and `just` is
  absent from the `rust` job; TLC checks the spec against itself; the actions
  are state-neutral but `InspectRetry` encodes a recorded commitment. A
  properly pinned standalone `formal` job survives as a proposal.
- **A sequencer-owned `AppWithProgress<A>` wrapper replacing the progress
  capabilities** — the pair lives inside the canonical SSZ bytes the watchdog
  byte-compares and the canonical machine advances it inside its own
  transition; cockroach recovery reads the clock from a dump into a wiped
  database. Evidence: `examples/app-core/src/wallet_snapshot.rs:41-42`,
  `commands/setup/mod.rs:433-451`. Record the composition in the application
  contract (jury-confirmed as a do-not-adopt).

Also standing, from the same reviews: the **do-not-simplify list** now
lives beside the invariants it protects
([`docs/invariants.md`](../invariants.md), "Do-not-simplify"), and these
deliberate declines keep their reasons — egress single-poller fan-out (no
need at current subscriber counts), `LeaseGuard` shared release channel,
`finalized_state` ETag on `l2_tx_index` (no reachable collision),
stale-skip on-chain report (scheduler protocol change; queue behind the
scheduler library), `Storage::read` commit-vs-rollback (no behavioral
difference).

## Historical codename map

Older commit messages and the pre-distillation ledgers (in git history) use
these codes; their concepts now live here:

| Code | Concept | Current home |
|---|---|---|
| R1a | write-before-broadcast watermark | I14 |
| R1b | cockroach recovery's best-effort flush | `cockroach.md` step 2 + settled above |
| R2 | content-identity check | I9, I15 |
| R3 | `synchronous=FULL` decision | `storage/open.rs` |
| R4 | exit-code contract | `commands/error.rs`, runbook |
| R5 | fail-loud check policy | invariants check policy |
| F1–F10 | 2026-06 correctness findings | settled above; F7 = the open "WS invalidation/rollback contract" finding |
| I1–I20 | invariants (stable, still in use) | `docs/invariants.md` |
| D1–D11, H1–H14, S-A, P1–P8 | 2026-08-18 defects / harvest / structural fix / premise items | settled above + ADR |
| WP1–WP11 | 2026-06 work packages (all landed) | settled above |
| L1, L2, L3 | the lifecycle decisions (not Layer 1/2) | ADR mechanism 2 + the two August ledgers |
| S1–S7, A1–A12, B1–B5 | 2026-06 simplification queue / owed tests | open remnants above |
