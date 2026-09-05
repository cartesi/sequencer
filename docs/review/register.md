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
19. **Closed** (2026-09-04): the key-file read returns
    `BootstrapError::KeyFile { path, source }`, classified by kind
    (`config::key_file_io_is_terminal`: missing, unreadable, not a file, or
    not text → 30; environmental I/O → 1); the message names the path, never
    the contents; both halves pinned. The sibling `create_dir_all` at each
    command's start keeps `CommandError::Io` → 1: it creates rather than
    reads, so a missing path is not an operator mistake there; a read-only or
    wrong-type parent is, and is not yet separated.
20. **Closed** (2026-09-04): `ensure_open_tip_for_recovery` splits its
    disjunction — a non-`Safe` danger raises `StaleDecision`, an already-open
    Tip raises the payload-free `TipAlreadyOpen`, paired with a retry reason
    so the operator sees it; the guard test and the polarity pin assert it.
21. **Closed** (2026-09-03): `Storage::ensure_open_tip` is
    `#[cfg(test)] pub(crate)`; the two intra-doc links and the snapshot
    lifecycle doc now name the reducer's guarded `EnsureOpenTip` phase.
22. **Closed** (2026-09-04): `DangerDetector`, `InputReader`, and the
    fee-oracle worker (narrowed with them: same single use, same
    construction-required lock) take `ShutdownSignal`; `launch` passes
    `shutdown.signal()` for the three and the scope to the lane, server, and
    submitter. Data-directory ownership is each worker's own `ProcessLock`,
    so the watchdog's weak witness is unaffected; the doc comments now say
    workers that externalize or contain take a scope.
23. **Closed** (2026-09-04): the guarded `EnsureOpenTip` phase re-reads
    `has_valid_open_batch` inside its own transaction and returns
    `TipMissingAfterOpen` (classified `refuse`, exit 30) rather than commit
    without a Tip; `drive_recovery`'s doc records the ≤5-phase bound and
    `admission.tla`'s comment names the enforced postcondition.
24. **Closed** (2026-09-04): the in-scope recorder is deleted, so a
    contained run writes exactly one `terminal_faults` row — the command
    bracket's, at settlement. Containment writes nothing durable. The
    accepted loss, stated in the runbook: any death before settlement (an
    abort at the two-second deadline, a controller panic, SIGKILL) leaves
    only the process logs, and the single write has no second attempt. The
    row is telemetry; restart policy is the exit code.
25. **Closed** (2026-09-03): the token's doc, the ADR, and the settled
    entry state its true scope (three compile-forced primitives; the rest
    hand-placed); "non-clone" became "boot-local"; the witness comment names
    every holder; the "journal", "acknowledge", and `DangerDetectorExit`
    remnants are gone.
26. **Closed** (2026-09-04): `acquire_finalized_lease` returns its own
    `FinalizedLease { inclusion_block: u64, dump: LeasedDump }`, so the
    `NOT NULL` column is no longer an `Option` and the impossible-`None`
    containment branch in `finalized_state` is gone. Corrupt-row containment
    is unchanged (the persistent-storage classifier and the decode panic, as
    `corrupt_finalized_snapshot_trips_terminal_storage_fault` pins).
27. **Closed** (2026-09-04): the two-verdict `RecoveryFailure::Provider`
    is split into `ProviderUnreachable` (retry) and `SignerMisconfig`
    (refuse), constructed by a pure `classify_signer_provider` pinned on all
    three arms; `classify_input_reader` documents its phase-dependent
    polarity and pins `Bootstrap`/`Join` refused at startup, non-terminal
    live. The setup-versus-run asymmetry it exposed is finding 32.
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
32. **A deterministic L1 misconfiguration exits 1 under `setup` but 30
    under `run`** — `setup` wraps every non-`Provider` input-reader failure
    — from `InputReader::new` (`setup/mod.rs:107-127`) and from both
    `sync_to_current_safe_head` calls (`setup/mod.rs:260-267,535-542`) — as
    a worker exit, whose `InputReaderError::is_terminal_invariant` treats
    `Bootstrap` as the live-worker case (non-terminal), so it projects to 1.
    `run` meets the bad-RPC-URL half through `classify_input_reader` and
    refuses at 30; the discovery-time half (wrong contract, pre-v3 InputBox)
    has no `run` counterpart, because `run` builds its reader with
    `from_parts` and never re-runs discovery. `setup` is an
    operator-run one-shot, so the harm is a wrong hint, not a restart loop.
    Candidate fixes: classify `setup`'s bootstrap failure through the same
    phase table as `run`, or give `setup` its own terminal variant for L1
    misconfiguration. Surfaced by the 2026-09-04 refuters; not yet decided.

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
- **Per-variant exit-code e2e assertions for classes 20/40/1** (those
  failure-path e2es still assert only `!success()`). Landed 2026-09-04: 10
  in the aging-tip scenario, 0 through `stop_expecting_clean_exit` at the
  healthy stop of `recovery_after_stale_batches`, and 30 in-crate through
  the real command bracket (`harness.rs`, `run` on a never-set-up data
  directory); the five verdicts are pinned to their integers in
  `commands/error.rs`, so renumbering `EXIT_TERMINAL` fails the suite.
- **A unit test of the production phase→progress mapping in
  `ProductionRecoveryDriver::perform`** (the scripted-driver traces exercise
  the test double's copy of it). The `classify_input_reader`
  `Bootstrap`/`Join` polarity pins landed 2026-09-04.
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
  concurrency, mutual exclusion, durability, and hostile-L1 robustness →
  AGENTS.md design principles.
- **The `Authorized` externalization token** (2026-08-18): the containment
  consult is a compile-time obligation of the three effect functions that
  take the token (ack, L1 send, WS emit); the remaining consults are
  hand-placed and bounded by the exit contract → `runtime/shutdown.rs` (the
  token) and ADR mechanism 1 (the consult inventory).
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

From the ADR's rejected list (opened by the 2026-08-01/02 re-evaluation);
the 2026-08-18 premise challenge found these alternatives left no residue in
code:

- **`RunEpoch`** (a globally threaded internal fencing epoch) — the OS lock
  plus structured task lifetime plus fresh per-scope channels already make
  an old sender unable to reach a new receiver, and there is no in-process
  hot restart to fence against; persisted rows cannot distinguish a live
  owner from a stale one, a kernel-held lock can. Revisit only if in-process
  restart or multiple admitted runtimes under one lock are introduced.
  Evidence: `runtime/process_lock.rs` (module doc); no epoch type exists in
  `sequencer/src`.
- **`EffectGate` / `LiveKernel`** (a universal effect mutex or actor, with a
  reader mailbox) — would duplicate the role-local linearization points the
  system already needs and force the reader and the latency-critical lane
  through a new in-memory authority protocol, adding a second state machine
  without making the narrow content-identity check a complete divergence
  oracle; SQLite stays the durable coordination plane. The `Authorized`
  token is not this — see
  [ADR mechanism 1](../plans/2026-08-authority-boundary-adr.md#1-runtimescope-structured-process-ownership).
  Evidence:
  `runtime/shutdown.rs` (`Authorized`); [I9](../invariants.md)'s
  completeness boundary.
- **A generic command controller** over setup/rebuild/run/maintenance —
  their facts are unrelated; combining them enlarges the cross-product state
  machine without closing an enforcement hole, and a flush has no admission
  state to restore or erase. Evidence: the per-command controllers are
  separate — `recovery/mod.rs` (`drive_recovery`), `commands/setup/mod.rs`
  (`admit_setup_lifecycle`), `commands/flush.rs` (the flush body); the one
  shared piece is a *fact* gate, `commands/mod.rs`'s
  `preflight_lifecycle_command` (used by `run` and `flush`; `setup` reads
  its own two facts inline), which checks admission facts and reduces
  nothing.
- **A per-chunk divergence query, provider call, or reader mailbox** on the
  hot path (formerly proposed per user-op) — the content-identity check is
  complete only for at/above-anchor accepted-batch content identity
  ([I9](../invariants.md)), so a query paid on every user-op chunk would buy
  no complete safety boundary. Cost is not the argument: the `POST /tx`
  round trip that carries a chunk is about 13 ms at concurrency 1
  (concurrency-1 HTTP ACK p50 13.231 ms against submit-to-matching-WS-event
  p50 25.313 ms in the same harness session — the ADR's
  [performance posture](../plans/2026-08-authority-boundary-adr.md#performance-posture)
  carries the surviving figures; the maintainer's earlier informal "roughly
  14 ms" localhost round-trip observation carried no metric qualifier), so
  the query would be cheap and still incomplete; a provider call on the same
  path would put L1 liveness inside the acknowledgement path. Evidence:
  `ingress/inclusion_lane/mod.rs` (the bounded chunk and the time-gated
  frontier read); I15's runtime reaction.
- **A durable recovery-phase ledger** — the flush and post-flush-sync
  witnesses are boot-local by design; persisting them would re-create a
  state machine whose only effect is skipping an idempotent flush, and would
  let a restarted attempt trust a half-remembered phase. Evidence:
  `recovery/mod.rs` (`RecoveryProgress` is memory-only and `drive_recovery`
  its only writer; the pin
  `reconstructed_controller_cannot_reuse_a_post_flush_sync_witness`);
  `admission.tla` (no durable per-attempt record gates anything).

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
  entry, and is now exercised: `run` logs the latest row once at startup and
  branches on nothing (`warn_on_previous_terminal_fault`, 2026-09-04).
- **A boot-time full-integrity sweep** — expensive machinery that still
  cannot catch semantic violations outside its read set; the residual
  window is recorded and bounded instead.
- **A boot assert on `finalized_snapshot.inclusion_block`** — the column is
  `NOT NULL` at the engine; the claimed gap does not exist. (The runtime
  `Option` branch on the same column was finding 26, closed 2026-09-04.)

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
  `egress/api/snapshot.rs:102-121,209-214`. The doc tightening survives (finding
  25).
- **Deleting the `#[from]` impls so a refusal reason cannot be typed into a
  retry** — the enums are public with public variants; the longer spelling
  still compiles and is the dominant idiom at all 15 sites. Evidence:
  `recovery/mod.rs:45-49,122-128`.
- **One `is_terminal()` on `VerifiedSignerProviderError` read by both
  tables** — the `From` impl performs no classification; the verdict is taken
  later over `BootstrapError`, whose variants have four other producers.
  Evidence: `commands/error.rs:688-700,220-273`; `l1/reader.rs:96-104`
  already documents the phase pair.
- **Flattening `RecoveryError` into one enum with `is_retryable()`** —
  refuted 2026-09-03 because `RecoveryFailure::Provider(String)` carried two
  verdicts, so no total function over the value existed. That premise was
  retired 2026-09-04 when finding 27 split the variant; every
  `RecoveryFailure` now determines its verdict from its value. The entry
  keeps its place on its remaining ground: the `Retry`/`Refuse` wrapper is
  the reducer's verdict at birth, not a predicate a consumer recomputes, and
  dropping its `Box` grows the `CommandError` footprint managed against
  clippy's `result_large_err` (`commands/error.rs:537`). Re-propose only
  against those. Evidence: `recovery/mod.rs:122-128,545-583`.
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
