# Cross-Module Invariants

The register of invariants whose **statement, enforcement, and consumers live in
different files**. Single-file invariants belong in that file's comments; this
file exists because the most dangerous knowledge in this codebase is the
invariant that spans modules with nothing pinning it — the kind a locally-sound
refactor silently breaks.

Each entry: what holds → where it's enforced → who depends on it → what breaks.
Symbol names can drift; verify against the code before relying on an entry.
When you change anything listed under *enforced by*, re-check every line under
*depended on by*.

## The check policy (fail-loud)

**Impossible states fail loud; they are never handled.**

- An invariant violation gets exactly one response: abort the operation loudly
  (assert, trigger `RAISE`, typed error). Cheap cross-module assertions at
  boundaries are *encouraged*. Failing loud is safety-preserving, not
  necessarily self-healing: a transient failure may clear on restart, while a
  persistent invalid row or state transition is terminal and can require
  inspection or cockroach recovery. A silently-tolerated bug that externalizes
  (a signed batch, an ack, a feed event) is state divergence — theft-equivalent
  and unrecoverable at runtime.
- **Never handle gracefully what cannot happen.** No fallback branches, no
  re-deriving a neighbor's answer to double-check it, no `Option`-handling for
  can't-be-`None`. One contract, one source of truth, no second code path.
- **Never absorb silently.** No `INSERT OR IGNORE`, saturating decode, or
  `unwrap_or_default` on data the contracts make impossible; use the loud
  variant of the same operation.
- **Command admission is fact-derived; a contained terminal fault closes
  in-process first.** Admission is governed by three facts, each with one
  owner: the kernel process lock (concurrent owners; the exclusive OS-held
  lock in `sequencer/src/runtime/process_lock.rs` makes
  one-process-per-data-dir kernel-enforced, the controller retains it
  through settlement, and nested work retains clones until it actually
  stops), `setup_complete` (two-sided command ordering: setup/rebuild never
  restart over a completed setup, run/flush never start before one), and
  `canonical_divergence` (the one absorbing refusal — only a
  fresh-directory cockroach rebuild proceeds). There is no lifecycle
  admission state machine and no operator acknowledgement: standard
  recovery is automatic — every run boots through the fact-derived
  reducer — and restart policy after a terminal fault is the exit-code
  contract (30 = do not restart, page), enforced by the supervisor; a
  persistent fault re-detects fail-loud on any boot that reads it. The
  only durable telemetry is the `terminal_faults` black box: append-only
  terminal-cause rows, written best-effort and verdict-neutrally —
  telemetry never changes a command's verdict, and nothing reads the
  black box for decisions. Runtime admission re-runs the
  reducer over one transactionally consistent fact set immediately before
  the non-yielding launch block; the process lock plus the task-free
  prepare phase make that read the decision's linearization. Baseline
  schema+history creation and setup completion are each one
  `synchronous=FULL` transaction. Runtime
  containment remains classification-at-birth: detection CAS-elects one
  reporter, sets the sticky containment bit, arms the independent
  two-second abort watchdog, and requests cooperative shutdown; it writes
  nothing durable. The black box's terminal-cause row is written once, by
  the command bracket at settlement, from the verdict the drain returns
  (best-effort telemetry — the exit code and logs carry the verdict if it
  fails). The watchdog holds only a
  weak process-lifetime witness; it aborts at the deadline exactly when a
  controller, worker, or nested blocking operation still retains the
  process lock. Cleanup polls all workers concurrently so one hung drain
  cannot hide a terminal exit that must arm the bound. Ordinary
  operator/recovery shutdown has no hard deadline. Externalization sites
  (acks, L1 sends, WS frames) check the containment bit before emitting;
  snapshot streams check it only at stream start today. A missed check is
  bounded by the exit-code contract and by the I15 freeze triggers on the
  tables they cover — partial structural backstops, not a barrier.
- **Maintenance is flush-only.** `flush-mempool` is an operator command,
  not a run-reducer alias: it settles the wallet nonce and never acquires
  Sync/Cascade semantics. It requires completed setup and no divergence.
  (The old origin-restoration machinery is gone with the admission state
  machine — there is no verdict state for a flush to erase, and a
  successful wallet flush proves nothing about the rest of the runtime.)
- **Normal run repair and admission have one reducer boundary.** Local
  absorbing facts are inspected before fallible provider facts. The pure run
  decision performs at most one recovery phase, and every completed phase
  returns to inspection. A successful flush contributes only an ephemeral
  safe-block witness for the current boot attempt; Sync must catch the
  persisted view up through that witness before Cascade, and a crash may safely
  establish a fresh witness by flushing again. After the reducer decides to
  admit, runtime preparation remains fallible and task-free; final
  admission then re-runs the same reducer over one consistent fact set and
  yields the single-use `RuntimeAdmission` witness consumed by the
  infallible, non-yielding launch. Raw worker and HTTP launch surfaces are
  crate-private; production app crates enter through `run`/`run_main`. No
  refusal or retry can construct the capability. Mutation and output
  authorization remains role-local at the durable boundaries documented
  below; public low-level storage helpers are not an authority API.
- An assertion must check a **real invariant** — true in every legitimate
  execution, including crash-recovery, replays, and clock steps — never an
  environmental assumption. (Cautionary tale: `sealed_at_ms >= created_at_ms`
  was once CHECK-enforced; wall-clock regression is legitimate, and the
  constraint wedged recovery before it was dropped.)

Decision test for any proposed check: (a) real invariant? (b) near-zero cost?
(c) fails loud with no alternative code path? Three yeses → write it. Any no →
don't.

## Register

### I1. Scheduler-acceptance semantics agree across all implementations

- **Authoritative prose:** [`docs/protocol/scheduler-semantics.md`](protocol/scheduler-semantics.md).
- **Holds:** the canonical fold (`Scheduler<A>`,
  `sequencer-core/src/scheduler/mod.rs`), the off-chain predicate
  (`ProtocolTiming::scheduler_accepts`, `sequencer-core/src/protocol.rs`), and
  the inclusion lane's live prediction produce the same
  accept/reject/ordering decisions for every input. (Known, documented
  exception: the predicate omits the two structural rejections — self-trust,
  since the simulator only runs over the sequencer's own well-formed batches;
  the omission is documented in `scheduler-semantics.md` and test-pinned by
  the I1 duality test in `sequencer-core/src/scheduler/mod.rs`, which asserts
  the canonical fold and the predicate diverge exactly and only there.)
- **Enforced by:** review + the duality test. No structural mechanism.
- **Depended on by:** everything — the gold frontier, recovery's cascade pivot,
  promotion, soft-confirmation honesty.
- **Breaks:** silent permanent scheduler/sequencer divergence.
- The expected-nonce fold is homed next to `scheduler_accepts` as
  `advance_expected_batch_nonce`; `decide_submit_start` consumes it, while
  `populate_safe_accepted_batches` keeps a deliberate inline copy (its advance
  interleaves with storage-only side effects that can't move below the protocol
  layer — see the call-site comment).

### I2. Drain attribution: accumulated directs land in the clock-advanced frame

- **Holds:** when the observed safe head is at least five blocks beyond the
  open frame clock, every newly-safe undrained direct is sequenced into the
  **new** frame, which is stamped with the observed safe head
  (`close_frame_in`, `storage/ingress.rs`). Directs may have accumulated across
  several below-threshold observations. Frame K's wire content is therefore
  "directs ≤ S_K, then ops validated on top"; a clock tick with no directs is
  an empty-prefix instance of the same rule.
- **Enforced by:** `close_frame_in` ordering; lane convention.
- **Depended on by:** the duality (scheduler's drain-before-ops equals the
  flattened replay order); catch-up; the feed.
- **Breaks:** ops validated against a state the scheduler won't reproduce —
  divergence.

### I3. Frame `safe_block`s are non-decreasing along the spine

- **Holds:** during an admitted live run, logical frame time advances directly
  to the latest observed safe head `H` only when `H - S >= 5`, where `S` is the
  open frame's persisted `safe_block`. An observation jump creates one frame at
  `H` and resets the anchor; no intermediary frames are synthesized. Batch
  closure may create a structural successor frame at the unchanged `S`, so
  equality is valid. Bootstrap and recovery are anchoring transitions, not
  live clock ticks: they may open a fresh Tip at a proven checkpoint/current
  safe head without applying the five-block delta.
- **Enforced by:** lane flow + `append_safe_inputs`' monotonicity asserts
  (`storage/l1_inputs.rs`) +
  `ProtocolTiming::FRAME_CLOCK_INTERVAL_SAFE_BLOCKS` (homed with its timing
  siblings in `sequencer-core/src/protocol.rs`; prose owner is the
  scheduler-semantics frame-clock section).
- **Depended on by:** `check_danger`'s arm ordering (see I4); the scheduler's
  within-batch monotonicity check; "if the frontier batch is fresh, all are".
- **Breaks:** I4's guarantee evaporates; danger detection mis-orders.

### I4. Tip-only cascade ⇒ every closed batch is gold

- **Holds:** `check_danger` checks `ClosedBatchInDanger` **before**
  `TipInDanger`; with I3, the closed frontier is always at least as old as the
  Tip, so the Tip arm can only fire when no non-gold closed batch exists.
- **Enforced by:** the arm order in `Storage::check_danger`
  (`storage/recovery.rs`) + I3.
- **Depended on by:** the dispatch table's meaning (a `RecoverTip` boot may
  skip the flush *because* nothing closed is doomed). **Not load-bearing for
  the pending clear**: the clear is scoped to `nonce >= pivot.nonce` in
  `cascade_and_reopen`, so a valid in-flight closed batch's pending survives
  any cascade by construction, regardless of arm order.
- **Breaks:** a Tip-only cascade while a closed batch is doomed would leave
  the doomed batch un-cascaded until the next detector cycle (liveness lag,
  not the old crash-loop).

### I5. Pending-clear is scoped to the cascade and runs in its transaction

- **Holds:** recovery deletes only pending rows with `nonce >=
  pivot.nonce`, atomically with the cascade and the full-backlog tip reopen
  (`cascade_and_reopen`, `storage/recovery.rs`).
- **Enforced by:** the single `write` tx + the scoped
  `clear_pending_dumps_from_nonce_in`.
- **Depended on by:** catch-up never loading a cascaded batch's state
  (cleared rows), and promotion never hitting a deleted row for a batch that
  stayed valid (surviving rows) — the `lifecycle.md` §6/§8 wedge is
  unrepresentable.
- **Breaks:** widening the delete re-arms the promote-wedge crash-loop;
  narrowing it lets catch-up resume from a cascaded batch's state.

### I6. A committed promotion implies an advanced drain

- **Holds:** promotion is folded into the drain's transaction
  (`close_frame_only_promoting_with_executions`), together with canonical
  direct-input attribution.
- **Enforced by:** the single `write` tx in `storage/ingress.rs`; the
  standalone `Storage::promote_finalized` is test-only by policy.
- **Depended on by:** crash-safety of the safe-frontier walk
  (`lifecycle.md` §5–§6).
- **Breaks:** restart re-processes the range and re-promotes a deleted pending
  row — crash-loop.

### I7. A committed batch close has a promotable pending row

- **Holds:** seal + next-Tip open + `pending_snapshots` insert commit together
  (`close_frame_and_batch_with_pending_dump`).
- **Enforced by:** single transaction; `create_dump` happens before, on disk.
- **Depended on by:** promotion (`promote_finalized_in` hard-fails on a missing
  row).
- **Breaks:** promotion wedge at the sealed batch's landing.

### I8. Always-load: a finalized snapshot and a valid Tip exist before the lane starts

- **Holds:** cold start registers the genesis dump as finalized and opens the
  genesis Tip; recovery reopens the Tip atomically across cascades.
- **Enforced by:** `setup` atomically registers the genesis finalized snapshot
  before its completion fact; the run reducer refuses a missing finalized
  fact, opens a missing Tip only through its guarded `EnsureOpenTip` phase,
  and recovery's cascade reopens in-transaction. `PreparedRuntime::prepare`
  reasserts the snapshot artifact before admission.
- **Depended on by:** catch-up's unconditional load path
  (`CatchUpError::NoSnapshot` is fail-loud, not a branch); the lane's
  `NoOpenTip` fail-loud load.
- **Breaks:** startup crash (loud — by design).

### I9. Acceptance identity: "accepted nonce N" means "our valid batch N"

- **Holds:** by nonce **and content** — the **content-identity check**: every
  landing at/above the batch-tree anchor that the off-chain
  `scheduler_accepts` simulation accepts is compared against the local valid
  closed batch at that nonce — `keccak256(landed bytes)` vs the hash stamped at
  seal by the same encode path the submitter broadcasts. The exhaustive local
  outcomes are `Match`, `Foreign` (no local valid closed batch), and `Mismatch`
  (different bytes); the last two record divergence.
- **Why content, not identity, suffices:** batches deliberately carry no
  identifier because content-equal copies are *effect-equal* — an accepted
  batch's application effects depend on its inclusion block only through the
  overdue force-drain, and for any fresh copy that force-executed prefix is
  a subset of the first frame's drain, in the same queue order. Which
  physical L1 transaction landed carries no semantic weight.
- **Enforced by:** prevention — the flush resolving every wallet-nonce slot
  before a cascade reuses a nonce, anchored by the persisted watermark (I14);
  detection — the content-identity check in
  `populate_safe_accepted_batches`, which on violation persists the
  `canonical_divergence` marker and freezes the frontier (I15).
- **Depended on by:** the gold frontier, cascade pivot selection, promotion,
  local-state ↔ canonical-state agreement.
- **Breaks:** would be silent divergence (a zombie replay of our own stale tx
  winning a nonce slot; a power-loss re-seal at the same nonce with different
  content); instead it is a detected `CanonicalDivergence` refusal whose
  remedy is cockroach recovery.
- **Completeness boundary:** the check completely enforces the accepted-batch
  identity predicate above; it is intentionally not a general canonical/application
  divergence oracle. It trusts collapsed history below the anchor and the
  checkpoint application state, shares `scheduler_accepts` (including its
  documented self-trust omissions), and does not independently detect bugs in
  direct-input/user-op execution. A wrong-high cockroach checkpoint nonce is a
  known example that can escape it. Absence of the marker therefore does not
  prove global agreement. Detection is automatic once the landing is safe and
  successfully ingested; repair is manual cockroach recovery, never standard
  recovery. Detection latency is inherent to the optimistic model: the check
  fires when the divergent landing reaches safe depth and is ingested, so
  soft confirmations issued inside that window are built on already-diverged
  state — bounded, and those confirmations are rollbackable by design.

### I10. Replay-offset sentinel: `0` means "from genesis"

- **Holds:** `valid_ordered_l2_tx_head` returns 0 on an empty stream, and
  catch-up pages with `offset > cursor` — sound because `sequenced_l2_txs`
  rowids start at 1 and rows are never deleted (invalidated rows are filtered,
  not removed), so offsets are globally increasing and 0 is never a real
  offset.
- **Enforced by:** SQLite rowid semantics + append-only convention.
- **Depended on by:** catch-up, the feed cursor, snapshot `l2_tx_index`.
- **Breaks:** first transaction skipped or double-applied on replay.
- **Scope:** this is the current physical SQLite replay cursor. It is not the
  canonical `Application::executed_input_count()` feed coordinate. The
  canonical mapping is durable, but the public feed has not changed from rowid
  pagination yet.

### I11. Own-batch safe inputs are sequenced but never executed or fanned out

- **Holds:** batch-submitter-sent safe inputs enter `sequenced_l2_txs` like any
  drained input, but are skipped by sender at catch-up replay
  (`catch_up.rs`), at live execution (`execute_safe_inputs_chunk`), and at WS
  delivery (feed filter).
- **Enforced by:** sender checks at each consumer (three places — keep them in
  sync).
- **Depended on by:** replay correctness (a batch payload must never execute as
  a deposit); feed consumers' state.
- **Breaks:** batch bytes applied as a direct input — divergence.

### I12. Safe head advances only on real observation; `synced_at_ms` is genuine progress time

- **Holds:** the reader early-returns when the fetched head doesn't advance;
  `append_safe_inputs` asserts monotonicity and stamps `synced_at_ms` only on
  commit.
- **Enforced by:** reader floor check + asserts (`storage/l1_inputs.rs`).
- **Depended on by:** the wall-clock danger arms (`L1ViewStale`,
  `EstimatedBatchInDanger`) — their baseline must be true progress time, or
  outages are masked.
- **Breaks:** danger detection silently late during exactly the outages it
  exists for.

### I13. No `dumps` row points at a missing directory

- **Holds:** file create (fsync'd) before row insert; row delete before file
  delete; orphan *files* are acceptable and swept at startup.
- **Enforced by:** ordering split between `storage/snapshot_dumps.rs`
  (SQLite-only) and the FS halves outside it (the lane's
  `inclusion_lane/snapshot.rs`; the startup sweep in
  `commands/run/startup_hygiene.rs`) — the module boundary *is* the ordering
  guarantee. Startup and egress classify a missing or structurally corrupt
  DB-referenced artifact as terminal; generic filesystem availability errors
  remain operational.
- **Depended on by:** `from_dump` at catch-up; the serving endpoints.
- **Breaks:** terminal startup refusal (or a terminal egress fault if detected
  while serving), requiring inspection or cockroach recovery rather than an
  automatic restart loop.

### I14. Watermark ≥ wallet nonce of every tx ever broadcast

- **Holds:** the **write-before-broadcast rule** — the watermark commits
  durably (`synchronous=FULL`) before any broadcast at a new nonce,
  uniformly for batch txs and flush no-ops. A crash between commit and send
  only over-covers (the flush later no-ops a never-used slot — harmless).
- **Enforced by:** write-before-broadcast — `EthereumBatchPoster::submit_batches`
  raises through `WalletNonceWatermarkSink` before its first send;
  `MempoolFlusher::flush_and_wait` likewise before its no-ops, and refuses to
  complete until `safe >= watermark + 1`.
- **Depended on by:** flush completeness, TLA+ Implementation Constraint 1,
  cascade soundness (I9).
- **Breaks:** zombie txs evade the flush — a dropped-locally but
  network-surviving batch tx re-lands at a slot the recovery batch reuses,
  and the scheduler executes invalidated content.

### I15. Divergence marker present ⇒ acceptance frontier frozen

- **Holds:** a fully-accepted landing that
  fails the content-identity check writes the `canonical_divergence`
  singleton **in the same transaction** as the sync that detected it, and
  `populate_safe_accepted_batches` returns early whenever the marker exists —
  so no acceptance row, no promotion, and no gold-frontier advance can ever
  happen past a detected divergence.
- **Enforced by:** the `trg_*_frozen_on_divergence` trigger family
  (`0001_schema.sql`) — specifically batch-tree writes, promotions, and
  pending-snapshot clears RAISE in the engine while the marker exists. This is
  the immediate persisted freeze for those named tables, not a general
  user-op hot-path barrier. The typed error surface also includes the marker
  guard at the top of
  `populate_safe_accepted_batches` and `check_danger`'s first arm
  (`CanonicalDivergence`, ranked ahead of every other arm). The run
  reducer makes that ordering structural at boot: local inspection refuses
  before any provider query, every completed phase re-enters inspection, and
  each mutating phase transaction reasserts both its durable preconditions and
  the absence of divergence before writing. The admission and preemptive TLA+
  models verify the controller ordering and slot/batch safety respectively.
- **Runtime reaction:** `check_danger` owns prompt process-wide reaction on its
  two-second cadence. Independently, the inclusion lane's existing time-gated
  SQLite read returns `SafeFrontierState::CanonicalDivergence` instead of an
  `Open` frontier when the marker is already present. The lane then closes
  intake, rejects queued work, and terminates before direct execution,
  promotion, or the five-block rotation decision. This is opportunistic
  refusal at an existing read, not another detector or a timing guarantee.
  One bounded dequeue chunk is the fast-turn limit, so rejected traffic cannot
  starve the read once its time gate is due. There is deliberately no
  per-chunk marker query or extra poll.
- **Race bound:** a lane turn that already read `Open` may finish if the reader
  commits divergence concurrently. Preventing that would require a lock or
  transaction spanning application execution. Existing freeze triggers stop
  conflicting batch-tree/promotion writes; the detector and next typed read
  stop the process. A chunk committed before either runtime observation may
  acknowledge and later roll back.
- **Watchdog boundary:** the freeze stops finalized promotion before the
  offending landing becomes a comparable sequencer checkpoint. Because the
  watchdog skips replay when the finalized inclusion block is unchanged, it
  does not subsume this wire-identity detector. Conversely, the check does
  not subsume the watchdog's broader independent application-state
  comparison.
- **Depended on by:** standard recovery never running on a diverged frontier
  (a flush+cascade there would compound the divergence); the lane never
  promoting a diverged landing; the remedy being cockroach recovery only.
- **Breaks:** silent permanent scheduler/sequencer divergence — the
  theft-equivalent failure.
- **Anchor-aware frontier:** the content-identity check fires
  only at/above the batch-tree **anchor** ([I16](#i16-the-batch-tree-has-exactly-one-valid-parentless-root-carrying-the-deployments-anchor-nonce)).
  `populate_safe_accepted_batches` seeds its initial expected nonce from the
  anchor (0 for genesis — unchanged; `N'` for a cockroach-recovered deployment),
  so L1 landings *below* `N'` are skipped by nonce-mismatch — they are **trusted
  collapsed history**, folded into the recovered checkpoint `S'`, not foreign.
  This only affects the empty-frontier seed; a running sequencer (non-empty,
  append-only `safe_accepted_batches`) always resumes from `latest_accepted`, so
  its foreign/zombie detection is byte-identical. `setup --recovery` itself
  *defers* frontier population entirely (`InputReader::set_frontier_mode(DeferUntilAnchorSet)`):
  its syncs run against an empty tree, so a frontier built then would falsely
  diverge — `run`'s first sync populates it once the anchor is set.

### I16. The batch tree has exactly one valid parentless root, carrying the deployment's anchor nonce

- **Holds:** every batch's nonce is
  `parent.nonce + 1`, except the single parentless root, which carries the
  `batch_tree_anchor` nonce — `0` for a genesis deployment, `N'` for a
  cockroach-recovered one (`setup --recovery` writes the anchor before the
  `setup_complete` marker). `run`'s first tip *is* that root (there is no
  separate sentinel batch). A fully-torn cascade re-roots parentless at the
  same anchor via `open_fresh_tip_in_tx`'s `parent = None` path, after
  invalidating the old root — so only one *valid* parentless root ever exists,
  invalidated ones coexisting.
- **Enforced by:** `trg_enforce_nonce_contiguity` — its parentless arm is an
  *exact* match `nonce == (SELECT nonce FROM batch_tree_anchor)` (tighter
  than a bare "must be 0"), plus an at-most-one-valid-parentless-root guard
  scoped to `invalidated_at_ms IS NULL`; `compute_next_nonce(None)` reads the
  same anchor; `trg_batch_tree_anchor_write_once` freezes the anchor once
  `setup_complete` exists.
- **Depended on by:** the submitter resuming at the right nonce — `run` submits
  `valid_closed_batches` with `nonce >= frontier_nonce`, where `frontier_nonce`
  defaults to the anchor (`= N'`) while `safe_accepted_batches` is still empty
  after recovery, so the submitter starts at `N'` rather than 0; the recovery
  fill roots the rebuilt tree at `N'` without replaying history. (`N'` is trusted
  checkpoint metadata, not re-verified at setup — see
  [`docs/recovery/cockroach.md`](recovery/cockroach.md#data-dictionary).)
- **Breaks:** a tree mis-anchored at the wrong nonce ⇒ `run`'s first batch
  carries a nonce the scheduler rejects ⇒ the sequencer is wedged (never
  submits), or — worse, if defenses were absent — a recovered tree silently
  diverging from canonical L1 state.

### I17. `WriteHead` is a coherent cache of the durable open Tip/frame

- **Holds:** SQLite owns the durable open batch/frame facts. The single
  inclusion lane loads one `WriteHead` from those facts at startup and threads
  it through every open-state mutation. Storage validates fallible counter
  advances before commit where needed, commits the durable rows, and mutates
  the caller's cache only after transaction success; an error or restart
  discards it and reloads from SQLite.
- **Enforced by:** the lane being the only open-state writer;
  `load_current_write_head` being the single constructor for persisted state;
  the `Storage::append_executed_user_ops_chunk`/attributed `close_*` update
  ordering; and the Tip,
  frame-position, FK, and PK constraints that fail loud on dangerous stale
  cache writes. Direct-input uniqueness still depends on the lane's drain
  cursor discipline because invalidated-history re-drain forbids a global
  `safe_input_index` uniqueness constraint.
- **Depended on by:** the hot path avoiding a redundant SQLite re-read on every
  chunk; batch-size/frame counters; safe-block drain attribution; every storage
  method that trusts the passed head.
- **Breaks:** a stale cache can target the wrong Tip/frame, duplicate or skip a
  position, or make live application order differ from durable replay order.
  This is an internal bug and fails loud, never a runtime condition to repair.
- **Design latitude:** the cache is reconstructible convenience, not an
  inter-component authority. Re-deriving more state from SQLite per turn may
  simplify the lane, but is an independent benchmarked change rather than part
  of the lane-reconciliation cutover.

### I18. History metadata changes atomically with the history fact it describes

- **Holds:** an authority-bearing initial setup/rebuild baseline creates the
  schema, one immutable UUIDv4 `EraId`, and `RecoveryGeneration = 0` in one
  `synchronous=FULL` transaction. Plain
  setup starts with both bases zero; rebuild starts with
  `base_executed_input_count = NULL` and `base_safe_input_index = NULL`
  because neither the folded application nor its recovery-root cursor exists
  yet.
- **Standard recovery:** `cascade_and_reopen` advances the generation exactly
  once in its transaction iff it invalidates at least one valid batch. A
  missing-Tip ensure or any other no-invalidation path leaves it unchanged.
- **Cockroach bind:** fill derives `K` from
  `S'.executed_input_count()` and captures the recovery root's exclusive
  safe-input cursor after sequencing its `<= C` padding. It binds both values
  in the same transaction that registers the initial finalized snapshot.
  `complete_setup` refuses while either base remains NULL or the finalized
  snapshot is absent. The pair is write-once. On retry, a matching root Tip
  plus that atomically bound snapshot/base pair is authoritative; it is not
  re-compared with a later fold.
- **Durable drain floor:** the next-undrained cursor is the maximum of
  `base_safe_input_index` and `MAX(valid safe_input_index) + 1`. Standard
  recovery may invalidate the cockroach root and thereby remove its padding
  from the valid view, but can never make inputs already represented by `S'`
  drainable or executable again. NULL is interpreted as zero only while
  setup has not completed (only a pre-completion rebuild fill can present a
  NULL floor: plain setup binds base 0 in its baseline transaction, and
  completion refuses while the base is NULL).
- **Coordinate separation:** `K` is an application-history boundary. It is
  deliberately independent of snapshot `l2_tx_index` and the current rowid
  feed cursor, which may include sequenced-but-not-executed cursor-padding
  rows. The per-input projection is now durable, but the current public feed
  still uses the physical cursor; its API/WS projection remains deferred.
- **Enforced by:** `baseline_migration` (`storage/open.rs`), the immutable-era,
  write-once-base, and exact-`+1` schema triggers; `cascade_and_reopen`
  (`storage/recovery.rs`); `insert_initial_finalized_dump`
  (`storage/snapshot_dumps.rs`); and `complete_setup`
  (`storage/lifecycle.rs`).
- **Depended on by:** standard-recovery discontinuity detection, honest
  post-cockroach history availability, the canonical offset projection, and
  the future Track 3 history-version/API protocol.
- **Breaks:** a client can mistake a rolled-back soft suffix for unchanged
  history, or a rebuilt deployment can advertise an unavailable/incorrect
  numeric prefix. Either silently diverges a mirror.
- **Operational boundary:** cockroach recovery remains an explicit
  fresh/wiped-directory operator action. Retaining an early incomplete DB
  reuses its still-unexposed era; a fail-loud partial-fill refusal requires a
  wipe/retry and therefore a new unexposed era. No automated replacement,
  clone detection, distributed fencing, or general resume state machine is
  implied.

### Do-not-simplify (deliberate shapes that look like cleanup targets)

The refactorer-facing mirror of the register above — each of these *looks*
like a simplification and would break a registered invariant:

- **Don't move filesystem work into `storage/snapshot_dumps.rs`** — the
  module boundary *is* the GC crash-ordering guarantee (I13).
- **Don't reorder `check_danger`'s arms** or merge its two `find_*` helpers
  into one that consults the Tip first — the closed-frontier-first order is
  the dispatch table's meaning (I4).
- **Don't "deduplicate" promotion out of the drain transaction** — a
  standalone promotion re-opens the promote-wedge crash loop (I6).
- **Don't filter own-batch rows out of `valid_sequenced_l2_txs`** — the
  drain cursor is `MAX(safe_input_index)+1` over those very rows; a view
  filter would rewind it and re-drain. Sender filtering stays at the
  consumers (I11).
- **Don't replace the rowid offset with count-based pagination** —
  invalidated-batch holes and the 0-sentinel depend on current physical
  behavior (I10).
- **Don't move snapshot GC off the promotion path** to an idle loop or a
  dedicated worker — promotion-coupled GC is starvation-proof and
  single-writer by design (`docs/snapshots/lifecycle.md`).
- **Don't add internal retry loops to the flusher/submitter for provider
  errors** — the orchestrator respawn is the retry mechanism; internal
  retries mask exactly the failures the danger machinery routes on.
- **Don't unify the two staleness references** (inclusion-relative vs
  current-relative) — deliberately different formulas for different
  questions.

### I19. Application progress advances only at the shared execution boundary

- **Holds:** `ApplicationProgress` is the pair
  `(ExecutedInputCount, last_executed_safe_block)`. Count zero implies clock
  zero. A successful canonical application input returns its pre-execution
  count as the offset and commits exactly `(count + 1, max(clock,
  input_clock))`; rejection changes neither field. `AppError` is fatal and
  defines no canonical successor.
- **Enforced by:** raw `apply_*` hooks and mutable progress access require
  distinct borrowed opaque capabilities constructible only by the shared
  execution functions. The boundary preflights count overflow, checks progress
  unchanged after validation and after a hook on both `Ok` and `Err`, and
  re-reads the immutable getter after commit to assert accessor coherence.
- **Depended on by:** the canonical scheduler, inclusion lane, catch-up,
  recovery fold, cockroach base `K`, durable execution attribution, and the
  future Track 3 API projection.
- **Breaks:** an input can be applied without advancing history, an offset can
  advance twice, or recovery can derive the wrong checkpoint clock — silent
  application-history divergence.
- **Scope:** application-specific mutation and determinism remain self-trusted.
  A failing hook is not rolled back; every production caller terminates that
  path and discards the instance.

### I20. Canonical execution offsets are an atomic projection of valid history

- **Holds:** `sequenced_l2_txs` remains the append-only physical replay/audit
  log. `executed_inputs` is a separate sparse projection for the current valid
  history: every user op and non-batch-submitter direct input that executes has
  exactly one mapping from its physical row to the pre-execution
  `ExecutedInputCount`; batch envelopes and cockroach-root cursor-padding rows
  have none. Current mappings occupy the contiguous logical interval `[K, H)`.
- **Creation atomicity:** a user-op chunk inserts its `user_ops`, trigger-created
  physical rows, and explicit execution mappings in the same FULL transaction
  that authorizes acknowledgements. A slow reconciliation turn inserts its
  direct physical rows, mappings, frame rotation, and any snapshot promotion
  in one transaction. The lane carries offsets attached to executed values, so
  an included input cannot be persisted without its receipt.
- **Recovery semantics:** suffix invalidation retains physical audit rows but
  deletes their derived mappings in the same transaction that advances
  `RecoveryGeneration` and opens the replacement Tip. This rewinds `H`
  naturally; replacement inputs reuse the suffix offsets under the new
  generation. The global logical UNIQUE constraint and next-offset trigger
  make a duplicate, gap, or out-of-order creation fail loud. Cockroach padding
  stays outside the projection, and the durable safe-input floor prevents it
  from being attributed later.
- **Snapshot/replay agreement:** every pending/finalized snapshot row stores
  both physical `l2_tx_index` and canonical `executed_input_count`. Snapshot
  registration asserts its count equals storage-derived `H`; startup compares
  the loaded application's count with the row; catch-up then checks each
  physical row's expected mapping before executing it. Missing, extra, or
  wrong mappings are terminal invariant failures, never repaired/backfilled.
- **Enforced by:** `ExecutedInputCount` receipts
  (`sequencer-core/src/application/mod.rs`); attributed lane/storage APIs
  (`ingress/inclusion_lane/`, `storage/ingress.rs`,
  `storage/mutations.rs`); `executed_inputs` constraints and invalidation
  trigger (`storage/migrations/0001_schema.sql`); storage-derived `H`
  (`storage/history.rs`); snapshot count checks
  (`storage/snapshot_dumps.rs`); and pre-execution catch-up checks
  (`ingress/inclusion_lane/catch_up.rs`).
- **Depended on by:** restart determinism, standard-recovery rollback/reuse,
  post-cockroach continuation at `K`, snapshot coherence, and the future
  canonical-offset HTTP/WS protocol.
- **Breaks:** the same numeric offset can name the wrong application input, or
  a restart can apply a different prefix than live execution—silent mirror or
  canonical-state divergence.
- **Performance boundary:** deriving `H` is a covering lookup over the logical
  UNIQUE index. Recovery deletes only its doomed projection suffix; it does
  not scan invalid physical history on every hot-path insertion. Direct
  execution receipt accumulation and classification live in the already-slow
  L1 reconciliation regime; the user-op hot path adds one chunk-level mapping
  query and inserts inside its existing durability transaction, not another
  fsync or actor.
