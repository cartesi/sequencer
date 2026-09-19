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
- **Command admission is fact-derived; terminal runtime faults abort the
  process.** Three facts (the kernel process lock, two-sided
  `setup_complete`, `canonical_divergence`), no admission state machine, no
  operator acknowledgement, and a verdict-neutral black box; the statement,
  the accepted trade, and the termination policy are owned by the
  [authority-boundary ADR](plans/2026-08-authority-boundary-adr.md)
  (mechanisms 1 and 2). What this policy adds: telemetry writes are
  verdict-neutral — a failed black-box record loses only the black-box copy,
  and the exit code and logs still carry the verdict. Runtime aborts skip
  settlement entirely. The [I15](#i15-divergence-marker-present--acceptance-frontier-frozen)
  freeze applies immediately to its named tables; runtime reaction still
  requires a worker to observe the committed marker.
- **Maintenance is flush-only.** `flush-mempool` is an operator command,
  not a startup-recovery alias: it settles the wallet nonce and never acquires
  Sync/Cascade semantics. It requires completed setup and no divergence.
  There is no verdict state for a flush to erase, and a successful wallet
  flush proves nothing about the rest of the runtime.
- **Normal run repair and admission share one selection policy.** Local
  absorbing facts are inspected before fallible provider facts; startup
  selects one repair and checks its result. The flush's safe-block witness
  is boot-local, and Sync must catch the persisted view up through it before
  Cascade. Final admission checks one consistent fact set and yields
  the single-use `RuntimeAdmission` witness consumed by the infallible,
  non-yielding launch; no refusal or retry can construct it, and raw worker
  and HTTP launch surfaces are crate-private. Design:
  [`docs/recovery/README.md`](recovery/README.md). Mutation and output
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

### Writer roles

One writer role per fact. Reads over batch data go through the `valid_*`
views (`valid_batches`, `valid_closed_batches`, `valid_open_batch`), which encapsulate the "exclude invalidated rows"
filter; writers target the base tables. The batch lifecycle columns partition
by writer and are write-once (`0001_schema.sql`).

| Writer | Writes |
|---|---|
| inclusion lane | `batches` (insert + `sealed_at_ms`), `frames`, `user_ops`, `application_inputs`, `dumps`/`snapshots` (batch close) |
| input reader | `safe_inputs`, `l1_safe_head`, `safe_accepted_batches`, `canonical_divergence` (the divergence poison marker) |
| recovery (startup) | `batches.invalidated_at_ms`, Tip reopen, current `application_inputs` suffix deletion |
| history metadata (setup/recovery) | `history_state` — complete era/application-count/L1-block baseline, generation bump in a non-empty standard-recovery cascade |
| batch submitter and mempool flusher | `wallet_nonce_watermark` — deliberately shared under one protocol: each raises it before its first broadcast (write-before-broadcast, I14) |
| egress (HTTP) | `dumps.lease_count` (leases); `run`'s startup hygiene resets it to zero as the crash backstop |
| setup | `deployment_identity` (pinned once), `batch_tree_anchor` (the root nonce, frozen once setup completes), the initial `dumps` + `snapshots` rows (genesis or rebuild registration, atomic with the complete history baseline), the `setup_complete` fact (written once), `batch_policy.log_gas_price` + `log_gas_price_updated_at_ms` (first write; Fixed and Uniswap) |
| snapshot GC (the lane after reconciliation, `run`'s startup hygiene) | unreferenced `dumps` row deletion (`gc_unreferenced_dumps`) |
| command brackets (run, setup, flush) | `terminal_faults` (append-only, best-effort at settlement) |
| admin | `batch_policy` alpha knobs (`log_alpha`, `log_one_plus_alpha`) |
| fee oracle | `batch_policy.log_gas_price` + `log_gas_price_updated_at_ms` (Uniswap mode only; stamps on every successful refresh) |

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
  checkpoint selection, soft-confirmation honesty.
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
  an empty-prefix instance of the same rule. That leading direct prefix is
  recoverable from `application_inputs` plus `frames.safe_block` alone.
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
  scheduler-semantics frame-clock section). The lane's frontier time gate
  bounds SQLite observation load and is not part of the clock semantics.
- **Depended on by:** `check_danger`'s arm ordering (see I4); the scheduler's
  within-batch monotonicity check; "if the frontier batch is fresh, all are".
- **Breaks:** I4's guarantee evaporates; danger detection mis-orders.

### I4. Closed-frontier danger takes precedence over Tip danger

- **Holds:** `check_danger` checks `ClosedBatchInDanger` before `TipInDanger`.
  With monotonic frame clocks, the closed frontier is at least as old as the Tip.
- **Enforced by:** arm order in `storage/recovery.rs` and I3.
- **Depended on by:** dispatch: closed-frontier danger selects flush before
  a Tip-only repair can be selected. The Tip itself has no L1 footprint.

### I5. Recovery removes exactly the invalidated application suffix

- **Holds:** invalidating a batch deletes its `application_inputs` through the
  schema trigger. The cascade, generation increment, and replacement Tip commit
  together. Original source records and immutable snapshots remain; snapshot
  selection excludes invalidated batches and GC retires their unleased artifacts.
- **Enforced by:** `cascade_and_reopen`, application-input constraints, valid views.
- **Breaks:** loading invalidated state or leaving a hole in current history.

### I6. The frame clock accounts for a complete L1 interval

- **Holds:** the surviving latest frame clock, floored by baseline block `C`,
  identifies the completely accounted L1 prefix. Reconciliation executes all
  external directs in the newly safe interval before committing its next frame
  and application rows. Envelopes-only intervals advance the clock with no rows.
- **Enforced by:** complete-block ingestion; the lane's indivisible reconciliation
  turn; storage range and execution-receipt checks. Batch closure preserves the
  current frame clock.
- **Breaks:** skipping or double-applying directs after restart/recovery.

### I7. Every committed batch close has an immutable snapshot

- **Holds:** file creation precedes the transaction sealing the batch, opening
  the next Tip, and registering its snapshot by local `batch_index` and count.
- **Enforced by:** `close_batch_with_snapshot` and
  `close_frame_and_batch_with_snapshot`. Selection requires the exact expected
  batch's snapshot; a missing artifact fails loud.
- **Breaks:** losing the accepted recovery/watchdog checkpoint.

### I8. A rollback-safe snapshot and valid Tip exist before the lane starts

- **Holds:** setup publishes the complete durable baseline before completion;
  recovery retains the newest accepted snapshot, or that baseline until first
  acceptance. A rebuilt baseline is a restore point, not a comparison at `C`.
- **Enforced by:** `complete_baseline_setup`, recovery admission, atomic Tip
  reopen, and `PreparedRuntime::prepare` artifact checks.
- **Depended on by:** unconditional snapshot restore and catch-up.
- **Breaks:** startup fails loud instead of inventing state.

### I9. Acceptance identity: "accepted nonce N" means "our valid batch N"

- **Holds:** by nonce **and content** — the **content-identity check**: every
  landing strictly after baseline block `C` that the off-chain
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
- **Depended on by:** the gold frontier, cascade pivot selection, checkpoint selection,
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
  prove global agreement. Conversely, a structurally malformed foreign landing
  may conservatively record divergence even when the on-chain algorithm would
  reject it — an accepted false positive under self-trust. Detection is
  automatic once the landing is safe and successfully ingested; repair is
  manual cockroach recovery, never standard
  recovery. Detection latency is inherent to the optimistic model: the check
  fires when the divergent landing reaches safe depth and is ingested, so
  soft confirmations issued inside that window are built on already-diverged
  state — bounded, and those confirmations are rollbackable by design.

### I10. Replay uses an inclusive application-history boundary

- **Holds:** `ExecutedInputCount = X` means input `X` executes next. Current
  rows cover `[K, H)`; snapshots at `X` replay with `offset >= X`. `H` waits.
- **Enforced by:** integer primary key, contiguous insertion, coherent versioned
  pages, and pre-execution catch-up count checks.
- **Depended on by:** restart and replica resume without skipping or duplication.

### I11. Batch envelopes remain outside application history

- **Holds:** `safe_inputs` retains every InputBox observation. The storage/lane
  boundary selects external directs by the setup-pinned submitter address;
  only those inputs and included user ops enter `application_inputs`.
- **Enforced by:** classified direct reads and complete receipt validation at
  append. Startup/recovery derive the initial direct rows before catch-up,
  which must execute them successfully before admission. Replay and WS need
  no envelope filter because every row executes.
- **Depended on by:** application replay and replicated state correctness.

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
- **Depended on by:** [flush completeness](recovery/README.md#closed-batches-flush-sync-cascade)
  and cascade soundness (I9).
- **Breaks:** zombie txs evade the flush — a dropped-locally but
  network-surviving batch tx re-lands at a slot the recovery batch reuses,
  and the scheduler executes invalidated content.

### I15. Divergence marker present ⇒ acceptance frontier frozen

- **Holds:** a fully-accepted landing that
  fails the content-identity check writes the `canonical_divergence`
  singleton **in the same transaction** as the sync that detected it, and
  `populate_safe_accepted_batches` returns early whenever the marker exists —
  so no acceptance row or gold-frontier advance can ever
  happen past a detected divergence.
- **Enforced by:** the `trg_*_frozen_on_divergence` trigger family
  (`0001_schema.sql`) — specifically batch-tree writes and snapshot collection RAISE in the engine while the marker exists. This is
  the immediate persisted freeze for those named tables, not a general
  user-op hot-path barrier. The accepted frontier itself has no trigger: its
  single writer refuses past the marker — the guard at the top of
  `populate_safe_accepted_batches`. The typed error surface also includes
  `check_danger`'s first arm (`CanonicalDivergence`, ranked ahead of every
  other arm). Startup inspection refuses before any provider query, and
  each repair transaction reasserts its durable preconditions and the
  absence of divergence before writing. A clean worker drain waits for
  in-flight reader appends: `run` then re-reads the marker on
  its Ok path (`refuse_divergence_on_clean_exit`) and exits terminal rather
  than 0, the one code that would break the supervisor's restart-then-refuse
  rediscovery. The admission and preemptive TLA+ models verify the
  controller ordering (`LocalTerminalDominance` in `admission.tla`) and
  slot/batch safety respectively.
- **Runtime reaction:** the danger detector owns prompt process-wide reaction,
  reading `check_danger` on its poll interval (`DANGER_DETECTOR_POLL_INTERVAL`). Independently, the inclusion lane's existing time-gated
  SQLite read returns `SafeFrontierState::CanonicalDivergence` instead of an
  `Open` frontier when the marker is already present. The lane then exits
  with a terminal error, causing the supervisor to abort, before direct execution or the five-block rotation decision. This is opportunistic
  refusal at an existing read, not another detector or a timing guarantee.
  One bounded dequeue chunk (`max_user_ops_per_chunk`) is the fast-turn limit, so rejected traffic cannot
  starve the read once its time gate is due. There is deliberately no
  per-chunk marker query or extra poll.
- **Race bound:** a lane turn that already read `Open` may finish if the reader
  commits divergence concurrently. Preventing that would require a lock or
  transaction spanning application execution. Existing freeze triggers stop
  conflicting batch-tree writes; the detector and next typed read
  stop the process. A chunk committed before either runtime observation may
  acknowledge and later roll back.
- **Watchdog boundary:** the freeze blocks accepted-checkpoint publication before the
  offending landing becomes a comparable sequencer checkpoint. Because the
  watchdog skips replay when the finalized inclusion block is unchanged, it
  does not subsume this wire-identity detector. Conversely, the check does
  not subsume the watchdog's broader independent application-state
  comparison.
- **Depended on by:** standard recovery never running on a diverged frontier
  (a flush+cascade there would compound the divergence); egress never
  publishing a diverged landing; the remedy being cockroach recovery only.
- **Breaks:** silent permanent scheduler/sequencer divergence — the
  theft-equivalent failure.
- **Baseline-aware frontier:** accepted-batch scanning starts strictly after
  the immutable baseline L1 block `C`, seeded with anchor nonce `N'`. The whole
  prefix is opaque, including previously rejected future-nonce batches; it must
  never be reinterpreted using a later expected nonce. Subsequent scans resume
  after the last acceptance. Rebuild defers the projection until the complete
  baseline and anchor are published.

### I16. The batch tree has exactly one valid parentless root, carrying the deployment's anchor nonce

- **Holds:** every batch's nonce is
  `parent.nonce + 1`, except the single parentless root, which carries the
  `batch_tree_anchor` nonce — `0` for a genesis deployment, `N'` for a
  cockroach-recovered one (`setup --recovery` writes the anchor before the
  `setup_complete` marker). The first Tip is that root (there is no separate sentinel batch): plain
  setup leaves its creation to startup recovery; rebuild creates it at `C` in
  the baseline transaction. A fully-torn cascade re-roots parentless at the
  same anchor via `open_fresh_tip_in_tx`'s `parent = None` path, after
  invalidating the old root — so only one *valid* parentless root ever exists,
  invalidated ones coexisting.
- **Enforced by:** the parent foreign key (enabled on every writer) rejects
  dangling parents; `trg_enforce_nonce_contiguity` checks nonce succession.
  Its parentless arm is an
  *exact* match `nonce == (SELECT nonce FROM batch_tree_anchor)` (tighter
  than a bare "must be 0"), plus an at-most-one-valid-parentless-root guard
  scoped to `invalidated_at_ms IS NULL`; `compute_next_nonce(None)` reads the
  same anchor; `trg_batch_tree_anchor_write_once` freezes the anchor once
  `setup_complete` exists.
- **Depended on by:** the submitter resuming at the right nonce — `run` submits
  `valid_closed_batches` with `nonce >= frontier_nonce`, where `frontier_nonce`
  defaults to the anchor (`= N'`) while `safe_accepted_batches` is still empty
  after recovery, so the submitter starts at `N'` rather than 0; the recovery
  fill roots the rebuilt tree at `N'` without replaying history. (`N'` is fold-derived from trusted
  checkpoint nonce `N`; wrong-low and wrong-high `N` are outside the supported
  checkpoint model — see
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
  cache writes. Direct-input uniqueness is enforced in the current application sequence;
  invalidation removes the old row before recovery can reuse its source.
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

### I18. History identity is published with the complete baseline

- **Holds:** file-first setup publishes `(EraId, generation=0, K, C, N')`, the
  baseline snapshot, any recovery root, and setup completion in one FULL
  transaction. The history row is absent before this boundary. `K` and `C`
  remain immutable even after baseline artifact GC or recovery-root invalidation.
- **Standard recovery:** one generation increment iff a valid batch is
  invalidated, in the cascade transaction. Clean restart changes neither token.
- **Enforced by:** `complete_baseline_setup`, immutable history triggers,
  exact-`+1` generation trigger, and `cascade_and_reopen`.
- **Depended on by:** mandatory snapshot-derived WS claims. Identity is validated
  before the requested count, including for empty history.
- **Breaks:** a client silently resumes a replaced suffix or inaccessible prefix.
- **Operational boundary:** rebuilding uses a fresh/wiped data directory.
  Checkpoint state, inclusion block, and next nonce are trusted operator inputs;
  neither clone detection nor distributed fencing is implied.

### Do-not-simplify (deliberate shapes that look like cleanup targets)

The refactorer-facing mirror of the register above — each of these *looks*
like a simplification and would break a registered invariant:

- **Don't move filesystem work into `storage/snapshot_dumps.rs`** — the
  module boundary *is* the GC crash-ordering guarantee (I13).
- **Don't reorder `check_danger`'s arms** or merge its two `find_*` helpers
  into one that consults the Tip first — the closed-frontier-first order is
  the dispatch table's meaning (I4).
- **Don't derive application order from L1 positions** — optimistic user ops
  precede their envelope and have no general one-to-one L1 mapping.
- **Don't retain a numeric resume offset without its history identity** —
  recovery deliberately reuses suffix counts (I18).
- **Don't discard a valid snapshot beyond the accepted frontier** — that exact
  batch can become the next required recovery checkpoint (I7).
- **Don't add internal retry loops to the flusher/submitter for provider
  errors** — the orchestrator respawn is the retry mechanism; internal
  retries mask exactly the failures the danger machinery routes on.
- **Don't unify the two staleness references** (inclusion-relative vs
  current-relative) — deliberately different formulas for different
  questions.

### I19. Application progress follows the shared execution contract

- **Holds:** `ApplicationProgress` is the pair
  `(ExecutedInputCount, last_executed_safe_block)`. Count zero implies clock
  zero. A successful canonical application input returns its pre-execution
  count as the offset and commits exactly `(count + 1, max(clock,
  input_clock))`; rejection changes neither field. `AppError` is fatal and
  defines no canonical successor.
- **Enforced by:** the application owns and persists the progress pair and
  returns it by value. All execution consumers use the shared boundary, which
  preflights count overflow and asserts the exact expected successor after a
  successful hook. Validation purity and native-engine mutation remain
  self-trusted; progress ownership does not require a Rust-side mirror.
- **Depended on by:** the canonical scheduler, inclusion lane, catch-up,
  recovery fold, cockroach base `K`, durable execution attribution, and the
  versioned replica protocol.
- **Breaks:** an input can be applied without advancing history, an offset can
  advance twice, or recovery can derive the wrong checkpoint clock — silent
  application-history divergence.
- **Scope:** application-specific mutation and determinism remain self-trusted.
  A failing hook is not rolled back; every production caller terminates that
  path and discards the instance.

### I20. Application history is committed with its execution receipts

- **Holds:** `application_inputs` contains every current included user op and
  external direct exactly once, keyed by mandatory pre-execution count. Its
  source reference, owning batch/frame, and payload tables reconstruct replay.
  There are no entries for batch envelopes or the opaque baseline prefix.
- **Creation atomicity:** user-op source rows and application rows commit in the
  same FULL chunk transaction that authorizes acknowledgements. Direct inputs
  commit with the complete frame rotation, after checking all execution receipts.
  Startup/recovery create leading direct rows before restoring the engine;
  successful catch-up is required before admitting that sequence.
- **Recovery:** source records remain, while invalidation deletes the current
  suffix. Replacement rows reuse counts under the incremented generation.
- **Snapshot/replay agreement:** a batch-close snapshot records storage-derived
  `H`; the restored engine must report the same count. Each replay row must
  match the engine's next count and executes with its persisted frame fee/clock
  (or the direct input's source block). Missing rows or count mismatches fail
  loud; they are never repaired or backfilled.
- **Enforced by:** shared execution receipts, storage append APIs, PK/FK/XOR/
  uniqueness and contiguous-offset triggers, coherent canonical pages, and
  catch-up checks.
- **Performance boundary:** head discovery uses the integer primary-key maximum;
  replay seeks directly by offset and joins bounded source rows. Neither scans
  invalidated history. Chunk insertion adds no durability transaction or actor.
