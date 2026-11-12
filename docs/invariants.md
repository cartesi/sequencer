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
- **Terminal faults, the terminal facts, and the maintenance flush**
  follow the [authority-boundary ADR](plans/2026-08-authority-boundary-adr.md)
  (mechanisms 1–3) and [batch landing and outages](recovery/README.md). Raw worker
  and HTTP launch surfaces are crate-private, and public low-level storage
  helpers are not an authority API: mutation and output authorization stay
  role-local at the durable boundaries this register lists.
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

One writer role per fact. The batch lifecycle columns partition by writer and
are write-once (`trg_batch_lifecycle`, `0001_schema.sql`). The sealed batches
are the `closed_batches` view and the one unsealed batch is `open_batch`;
writers target the base tables.

| Writer | Writes |
|---|---|
| inclusion lane | `batches` (insert + `sealed_at_ms`), `frames`, `user_ops`, `application_inputs`, `snapshots` (batch close) |
| input reader | `safe_inputs`, `l1_safe_head`, `batches.accepted_input`, `canonical_divergence` (the divergence poison marker) |
| `run`'s start | an open batch when none is open, which is how the era's first batch is created |
| history metadata (setup) | `era` — one immutable row (base count, stop block, rebuilt history end, first queued input, anchor nonce), written once by setup's publish |
| batch submitter and mempool flusher | `wallet_nonce_watermark` — deliberately shared under one protocol: each raises it before its first broadcast (write-before-broadcast, I14) |
| setup | `deployment_identity` (pinned once, the direct-payload cap with it), `rebuilt_history` (its only writer, and only before it publishes the era), the `era` row (whose existence is setup's completion), the baseline `snapshots` row (atomic with the era; no `batches` or `frames`), `batch_policy.log_gas_price` + `log_gas_price_updated_at_ms` (first write; Fixed and Uniswap) |
| snapshot GC (the lane after reconciliation, `run`'s startup hygiene) | obsolete, unleased `snapshots` row deletion (`collect_snapshots`), including a superseded baseline artifact |
| command brackets (run, setup, flush) | `terminal_faults` (append-only, best-effort at settlement) |
| fee oracle | `batch_policy.log_gas_price` + `log_gas_price_updated_at_ms` (Uniswap mode only; stamps on every successful refresh) |

### I1. Scheduler-acceptance semantics agree across all implementations

- **Authoritative prose:** [`docs/protocol/scheduler-semantics.md`](protocol/scheduler-semantics.md).
- **Holds:** the canonical scheduler (`Scheduler`,
  `sequencer-core/src/scheduler/mod.rs`, which `CanonicalRunner` in
  `scheduler/driver.rs` drives with the executor), the off-chain predicate
  (`scheduler_accepts`, `sequencer-core/src/protocol.rs`), and
  the inclusion lane's live prediction produce the same
  accept/reject/ordering decisions for every input. (Known, documented
  exception: the predicate omits gate d's three structural rejections and
  gate f — self-trust, since the simulator only runs over the sequencer's own
  well-formed batches, opened in order at safe heads it has seen; the reason
  is recorded at `scheduler_accepts`, and it is test-pinned by the I1 duality
  test in `sequencer-core/src/scheduler/mod.rs`, which asserts the scheduler
  and the predicate diverge exactly and only there.)
- **Enforced by:** review + the duality test. No structural mechanism.
- **Depended on by:** everything — the gold frontier, the void history's
  choice of the oldest unaccepted batch (I4), checkpoint selection,
  soft-confirmation honesty.
- **Breaks:** silent permanent scheduler/sequencer divergence.

### I2. Drain attribution: accumulated directs land in the clock-advanced frame

- **Holds:** when the observed safe head is at least five blocks beyond the
  open frame clock, every newly-safe undrained direct is sequenced into the
  **new** frame, which is stamped with the observed safe head
  (`close_frame_in`, `storage/ingress.rs`). Directs may have accumulated across
  several below-threshold observations. Frame K's wire content is therefore
  "directs ≤ S_K, then the frame's ops"; a clock tick with no directs is
  an empty-prefix instance of the same rule. That leading direct prefix is
  recoverable from `application_inputs` plus `frames.safe_block` alone.
- **Enforced by:** `close_frame_in` ordering; lane convention.
- **Depended on by:** the duality (scheduler's drain-before-ops equals the
  flattened replay order); catch-up; the feed.
- **Breaks:** ops executed against a state the scheduler won't reproduce —
  divergence.

### I3. Frame `safe_block`s are non-decreasing along the spine

- **Holds:** frame `safe_block`s never decrease along the batches; equal
  adjacent clocks are valid. The [frame-clock policy](protocol/scheduler-semantics.md#sequencer-frame-clock-policy)
  says when the lane advances the clock, including the anchoring exception
  for bootstrap.
- **Enforced by:** lane flow + `append_ingested_safe_inputs_with_timestamp`'s monotonicity asserts
  (`storage/l1_inputs.rs`) +
  `FRAME_CLOCK_INTERVAL_SAFE_BLOCKS`
  (`sequencer-core/src/protocol.rs`). The lane's frontier time gate
  bounds SQLite observation load and is not part of the clock semantics.
- **Depended on by:** the void history (I4), which judges the oldest
  unaccepted batch's first frame: with monotonic clocks no later batch is
  older; the scheduler's within-batch monotonicity check.
- **Breaks:** the void history judges the wrong batch and fires late.

### I4. A void history ends the era

- **Holds:** with every input through the persisted safe head `H` judged, the
  oldest batch L1 has not accepted, closed or open, opened at first-frame safe
  block `s`, is void once `H + 1 - s >= max_wait_blocks`; no later batch can
  then be accepted. It stays true, so it is derived again on every read and
  never stored, and while it holds `run` refuses and a running process stops
  ([the void history](recovery/README.md#the-void-history)).
- **Enforced by:** `Storage::terminal_fact`, read at `run`'s start before any
  L1 call and again inside the transaction that opens a batch, and by the
  worker that watches the terminal facts while the process runs
  ([what the sequencer does](recovery/README.md#what-the-sequencer-does)).
- **Depended on by:** soft-confirmation honesty: the process stops within one
  poll of that worker, two seconds, of the safe head showing the history void,
  and the ops confirmed in that interval are lost with the rest; cockroach
  recovery as the remedy.
- **Breaks:** the sequencer keeps confirming user ops that can never land.

### I5. Batches and application history only append

- **Holds:** a batch is created once, sealed once with its hash, accepted
  once, and never deleted or replaced; a frame, a user op and an application
  input are written once and never deleted. Inside an era the history
  therefore only grows, what the feed served stays served, and a sealed batch
  always re-encodes to the bytes its hash was taken from.
- **Enforced by:** `trg_batch_lifecycle` (a batch row changes only by sealing
  once or by being accepted once); the triggers that refuse every delete on
  `batches`, `frames`, `user_ops` and `application_inputs`, and every update
  on `frames` and `user_ops`; and `PRAGMA recursive_triggers = ON` on every
  connection, without which an `INSERT OR REPLACE` deletes a conflicting row
  past those triggers.
- **Depended on by:** a client resuming at any count it saved in this era;
  the void history (I4), which judges a batch nothing replaces; the content
  identity check (I9) and the nonce rule (I16), which need one batch per
  nonce.
- **Breaks:** a client's saved state stops matching the feed it resumes from,
  or a nonce names two batches.

### I6. The frame clock accounts for a complete L1 interval

- **Holds:** the latest frame clock, floored by the era's stop block
  `C`, identifies the completely accounted L1 prefix. Reconciliation executes all
  external directs within the payload cap in the newly safe interval before
  committing its next frame and application rows. Envelopes-only intervals advance the clock with no rows.
  With no frame in the era, the next undrained input is the era's first
  queued input: the era's baseline accounts for every input through `C`
  except the directs still queued there, so the root frame drains from it
  (`next_undrained_safe_input_index_in`, `storage/ingress.rs`).
- **Enforced by:** complete-block ingestion: every committed `safe_inputs` /
  `l1_safe_head` pair holds every input through its head, witnessed by index
  contiguity plus the InputBox count at that head (`l1/reader.rs`), above a
  scan floor setup pins only once the deployment is safe; the lane's
  indivisible reconciliation turn; storage range and execution-receipt checks.
  Batch closure preserves the current frame clock.
- **Breaks:** skipping or double-applying directs after restart.

### I7. Every committed batch close has an immutable snapshot

- **Holds:** file creation precedes the transaction sealing the batch, opening
  the next Tip, and registering its snapshot by batch nonce and count.
- **Enforced by:** `close_batch_with_snapshot` and
  `close_frame_and_batch_with_snapshot`. Selection requires the exact expected
  batch's snapshot; a missing artifact fails loud.
- **Breaks:** losing the accepted comparison checkpoint.

### I8. An accepted snapshot and an open batch exist before the lane starts

- **Holds:** setup publishes the durable era baseline, at the rebuilt history
  end `E`, in the transaction that inserts the era; snapshot collection
  retains the newest accepted snapshot, or that baseline until first
  acceptance, for the comparison endpoints and the lane's restart. The baseline is the application's
  state at the end of stop block `C`, which the canonical machine reaches too,
  so until a batch of the era is accepted it is the comparison checkpoint
  (inclusion block `C`, count `E`, the anchor nonce), genesis base or not.
  Setup opens no Tip; `run`'s start opens the era's first batch before the
  lane starts.
- **Enforced by:** `publish_era`, `finalized_dump_in`'s assertion that the
  baseline sits at `E`, `run`'s guarded open-a-batch transaction, and
  `PreparedRuntime::prepare` artifact checks.
- **Depended on by:** unconditional snapshot restore and catch-up.
- **Breaks:** startup fails loud instead of inventing state.

### I9. Acceptance identity: "accepted nonce N" means "our batch N"

- **Holds:** by nonce **and content** — the **content-identity check**: every
  landing strictly after baseline block `C` that the off-chain
  `scheduler_accepts` simulation accepts is compared against the local closed
  batch at that nonce — `keccak256(landed bytes)` vs the hash stamped at
  seal by the same encode path the submitter broadcasts. The exhaustive local
  outcomes are `Match`, `Foreign` (no local closed batch), and `Mismatch`
  (different bytes); the last two record divergence. A `Match` is recorded on
  the batch itself, as `batches.accepted_input`: the input reader's one
  write-once update, which the schema admits only on a sealed batch
  (`trg_batch_lifecycle`) whose nonce follows the greatest accepted batch's
  (the anchor before any), at a later input
  (`trg_batch_acceptance_extends_the_prefix`). A batch is inserted unaccepted
  (`trg_batch_insert`). The accepted batches are a prefix of the list.
- **Why content, not identity, suffices:** batches deliberately carry no
  identifier because content-equal copies are *effect-equal* — an accepted
  batch's application effects depend on its inclusion block only through the
  overdue force-drain, and for any fresh copy that force-executed prefix is
  a subset of the first frame's drain, in the same queue order. Which
  physical L1 transaction landed carries no semantic weight.
- **Enforced by:** prevention — no batch nonce is reused inside an era;
  across eras setup's flush is best effort, and its escape check refuses what
  it sees ([the flush and the stop block](recovery/cockroach.md#the-flush-and-the-stop-block));
  detection — the content-identity check in
  `record_accepted_batches`, which on violation persists the
  `canonical_divergence` marker and freezes the frontier (I15).
- **Depended on by:** the gold frontier, the void history's oldest unaccepted batch, checkpoint selection,
  local-state ↔ canonical-state agreement.
- **Breaks:** would be silent divergence (a foreign landing at one of our
  nonces; a power-loss re-seal at the same batch nonce
  with different content); instead it is a detected `CanonicalDivergence`
  refusal whose remedy is cockroach recovery.
- **Completeness boundary:** the check completely enforces the accepted-batch
  identity predicate above; it is intentionally not a general canonical/application
  divergence oracle. The entire L1 prefix through stop block `C` is opaque to
  it, including previously rejected future-nonce batches: setup's rebuild
  judged the inputs past its base with the scheduler, and the check trusts the
  base, that rebuild and the anchor nonce instead of reinterpreting them.
  It shares `scheduler_accepts` (including its documented self-trust omissions),
  and does not independently detect bugs in
  direct-input/user-op execution. Absence of the marker therefore does not
  prove global agreement. Conversely, a structurally malformed foreign landing
  may conservatively record divergence even when the on-chain algorithm would
  reject it — an accepted false positive under self-trust. Detection is
  automatic once the landing is safe and successfully ingested; repair is
  manual cockroach recovery. Detection latency is inherent to the optimistic model: the check
  fires when the divergent landing reaches safe depth and is ingested, so
  soft confirmations issued inside that window are built on already-diverged
  state — bounded, and those confirmations are revocable by design.

### I10. Replay uses an inclusive application-history boundary

- **Holds:** `L2TxCount = X` means entry `X` executes next. The
  rebuilt rows cover `[K, E)` and the live rows `[E, H)`; a snapshot
  is never below `E`, so native restart replays only live rows with
  `offset >= X`. `H` waits.
- **Enforced by:** integer primary key, contiguous insertion from `E`
  (`trg_application_inputs_contiguous`), contiguous rebuilt counts
  (`trg_rebuilt_history_insert`, `trg_era_fits`), coherent
  era-checked pages spanning the seam, and pre-execution catch-up count checks.
- **Depended on by:** restart and replica resume without skipping or duplication.

### I11. Batch envelopes remain outside application history

- **Holds:** `safe_inputs` retains every InputBox observation. The storage/lane
  boundary selects external directs by the setup-pinned submitter address and
  payload cap ([I20](#i20-application-history-is-committed-with-its-execution-receipts));
  only those inputs and included user ops enter `application_inputs`.
- **Enforced by:** append-only `safe_inputs` triggers; classified direct reads
  and complete receipt validation at append. The era's root frame takes the
  directs still queued at `C`; lane catch-up executes them
  before processing queued user operations. Setup's rebuild selects the same
  directs (`rebuilt_history` refuses a submitter input as a direct), and the
  era's baseline holds their effects through `C`, with no live rows for that
  prefix. Replay and WS need no
  envelope filter because every served row executes.
- **Depended on by:** application replay and replicated state correctness.

### I12. Safe head advances only on real observation

- **Holds:** the reader early-returns when the fetched head doesn't advance;
  the append asserts the safe head never decreases and commits it with every
  input through it.
- **Enforced by:** reader floor check + asserts (`storage/l1_inputs.rs`).
- **Depended on by:** the void history (I4): `H` only rises, so a void history
  stays void; the landing lag and the safe head's age that `/healthz` reports.
- **Breaks:** a void history read from a head L1 never reached, or a lag that
  hides an outage.

### I13. No `snapshots` row points at a missing directory

- **Holds:** file create (fsync'd) before row insert; row delete before file
  delete; orphan *files* are acceptable and swept at startup.
- **Enforced by:** ordering split between `storage/snapshots.rs`
  (SQLite-only) and the FS halves outside it (the lane's
  `inclusion_lane/snapshot.rs`; the startup sweep in
  `commands/run/startup_hygiene.rs`) — the module boundary *is* the ordering
  guarantee. GC never collects a snapshot a download has leased
  ([lease protocol](snapshots/lifecycle.md#retention-leases-and-crash-safety)).
  Startup and egress classify a missing or structurally corrupt DB-referenced
  artifact as terminal; generic filesystem availability errors remain
  operational.
- **Depended on by:** `from_dump` at catch-up; the serving endpoints.
- **Breaks:** terminal startup refusal (or a terminal egress fault if detected
  while serving), requiring inspection or cockroach recovery rather than an
  automatic restart loop.

### I14. Watermark ≥ wallet nonce of every tx ever broadcast

- **Holds:** the **write-before-broadcast rule** — the watermark commits
  durably (`synchronous=FULL`) before any broadcast at a new nonce,
  uniformly for batch txs and flush no-ops. A crash between commit and send
  only over-covers (the flush later no-ops a never-used slot — harmless).
- **Enforced by:** the `Covered` witness (`l1/watermark.rs`): only `cover`
  mints it, after `WalletNonceWatermarkSink::raise_to` succeeds, and the
  poster's `send_batch_at_wallet_nonce` and the flusher's `submit_noops`
  require it and assert each wallet nonce they send is at or below it.
  `EthereumBatchPoster::submit_batches` covers its whole consecutive range;
  `MempoolFlusher::flush_and_wait` covers every no-op range it sends, and
  refuses to complete until `safe >= watermark + 1`. Pinned by a
  raise-before-send test on each side.
- **Depended on by:** `flush-mempool`'s completeness. Setup's flush runs on a
  new database, which has no watermark, so it does not rely on this
  ([the flush and the stop block](recovery/cockroach.md#the-flush-and-the-stop-block)).
- **Breaks:** zombie txs evade the flush — a batch tx dropped locally but
  surviving in the network lands after the flush reports the wallet settled.

### I15. Divergence marker present ⇒ acceptance frontier frozen

- **Holds:** a fully accepted landing that fails the content-identity check
  writes the `canonical_divergence` singleton **in the detecting sync's
  transaction**, and `record_accepted_batches` returns early while the
  marker exists, so no acceptance or gold-frontier advance follows a
  detected divergence.
- **Enforced by:** the `trg_*_frozen_on_divergence` triggers
  (`0001_schema.sql`) make batch-tree writes, acceptance
  (`batches.accepted_input`) included, and snapshot collection RAISE while the
  marker exists: an immediate, persisted freeze of those tables, not a user-op
  hot-path barrier. Created after every other BEFORE trigger on `batches`, the
  freeze answers first. `Storage::terminal_fact` reads the marker before the
  void history. `run`'s start refuses before any provider query, and its
  open-a-batch transaction reasserts the marker's absence before writing. A
  clean worker drain waits for in-flight reader appends; `run` then
  re-reads the terminal facts (`refuse_terminal_fact`) and exits terminal,
  not 0, the one code that would break the supervisor's restart-then-refuse
  rediscovery.
- **Runtime reaction:** the worker that watches the terminal facts owns the
  prompt process-wide reaction, reading `Storage::terminal_fact` every two
  seconds.
  Independently, the lane's existing time-gated SQLite read returns
  `SafeFrontierState::CanonicalDivergence` instead of `Open`, and the lane
  exits terminal, so the supervisor aborts, before direct execution or the
  five-block rotation decision. This is opportunistic refusal at an existing
  read, not another detector or a timing guarantee, and there is deliberately
  no per-chunk marker query or extra poll. One `max_user_ops_per_chunk` chunk
  is the fast-turn limit, so rejected traffic cannot starve the read once its
  time gate is due.
- **Race bound:** a lane turn that already read `Open` may finish while the
  reader commits divergence; preventing that would need a lock or transaction
  spanning application execution. The freeze triggers stop conflicting
  batch-tree writes, and the watching worker or the next typed read stops the
  process, so a chunk committed before either may acknowledge and later roll
  back.
- **Watchdog boundary:** accepted-checkpoint selection checks the marker in
  the same read as selection, under the lease registry's lock for a download,
  and refuses while it is present. A matching batch before a divergent acceptance in the same L1
  block is not that block's final state, and the watchdog skips replay when
  the finalized inclusion block is unchanged, so it does not subsume this
  wire-identity detector. Nor does the check subsume the watchdog's broader,
  independent application-state comparison.
- **Depended on by:** egress never publishing a diverged landing; cockroach
  recovery as the only remedy.
- **Breaks:** silent permanent scheduler/sequencer divergence, the
  theft-equivalent failure.
- **Baseline-aware frontier:** accepted-batch scanning starts strictly after
  the era's stop block `C`, seeded with anchor nonce `N'`; the prefix through
  `C` is opaque
  ([I9](#i9-acceptance-identity-accepted-nonce-n-means-our-batch-n)).
  Later scans resume after the last acceptance. Storage maintains the
  accepted frontier exactly when the era row exists, reading the batch
  submitter from the pinned deployment identity: before the era (every setup
  sync, genesis included) there is no stop block or anchor, and the old
  instance's batches would read as `Foreign` and falsely freeze the frontier.
  The first append after `publish_era`, `run`'s first sync, judges every
  submitter input past `C`, whichever sync ingested it.

### I16. Batches carry contiguous, unique nonces from the era's anchor

- **Holds:** a batch's key is its nonce, and the batches carry the nonces
  `N'`, `N' + 1`, … where `N'` is the era's `anchor_batch_nonce` — the
  scheduler's next batch nonce at the end of the era's stop block, which
  setup's rebuild ends with and publishes in the era row. A new batch carries
  the last nonce + 1, or the anchor when there is no batch: the era's first
  batch (setup leaves its creation to `run`'s start; there is no sentinel
  batch). Each nonce names one batch, so no nonce is reused inside an era. No
  batch exists before the era does.
- **Enforced by:** `trg_batch_insert`, a BEFORE INSERT check that
  `nonce IS COALESCE(last nonce + 1, era anchor)` (with neither, it
  compares against NULL and aborts); `compute_next_nonce` computes the same
  rule and fails when the era is absent; `batches.nonce` as the primary key;
  `trg_batch_lifecycle`, which keeps a nonce fixed
  once inserted; and the era row's immutability (`trg_era_immutable`, `trg_era_not_deletable`,
  `trg_era_single_insert`), so a live deployment is never re-anchored.
- **Depended on by:** the submitter resuming at the right nonce — `run` submits
  `closed_batches` with `nonce >= frontier_nonce`, where `frontier_nonce`
  defaults to the anchor while no batch is accepted, so the
  submitter starts at the anchor; nonce lookups such as the content-identity
  check (I9), which expect one batch per nonce.
- **Breaks:** batches mis-anchored at the wrong nonce ⇒ `run`'s first
  batch carries a nonce the scheduler rejects ⇒ the sequencer is wedged (never
  submits), or — worse, if defenses were absent — a recovered deployment
  silently diverging from canonical L1 state; two batches at one nonce ⇒
  a nonce lookup picks an arbitrary one.

### I17. `WriteHead` is a coherent cache of the durable open Tip/frame

- **Holds:** SQLite owns the durable open batch/frame facts. The single
  inclusion lane loads one `WriteHead` from those facts at startup and threads
  it through every open-state mutation. Storage validates fallible counter
  advances before commit where needed, commits the durable rows, and mutates
  the caller's cache only after transaction success; an error or restart
  discards it and reloads from SQLite.
- **Enforced by:** the lane being the only open-state writer;
  `load_current_write_head` being the single constructor for persisted state;
  the head being opaque outside storage (private fields, read accessors,
  storage-private mutators); the
  `Storage::append_executed_user_ops_chunk`/attributed `close_*` update
  ordering; storage reloading the Tip and comparing it with the head (batch,
  nonce, frame, counts, fee, safe block) at every frame rotation and batch
  close; and the Tip, frame-position, FK, and PK constraints that fail loud on
  dangerous stale cache writes. Direct-input uniqueness is enforced in the
  current application sequence.
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

### I18. History identity is published with the era

- **Holds:** file-first setup publishes the era row
  `(EraId, published at, K, E, C, first queued input, anchor N')` and the
  baseline snapshot at `E` in one `IMMEDIATE` transaction (`publish_era`),
  with no batch in the database; the row's existence is setup's completion.
  The era row is absent before this boundary. Its fields remain immutable even
  after baseline artifact GC. The era id is the history's whole identity:
  inside an era the history only appends (I5), so a claim is an era id and a
  count, every count from `K` to the head resumes, and a restart changes
  nothing.
- **Rebuilt history:** `rebuilt_history` gains one row per executed
  transaction, written once with its count, each count the last one plus one
  and each direct after every earlier direct in input order, only while the
  era row is absent; no row is ever updated or deleted, and a skipped user op
  has none. The era's `K` and `E` are exactly the span of the rows, no rebuilt
  direct lies at or past its first queued input, which is at most the input
  count through `C`, and the live rows continue at `E`. A client of an older era
  adopts this one only at a count in `[K, E]`
  ([adopting a new era](protocol/application-history.md#adopting-a-new-era)).
- **Enforced by:** `publish_era`, the era triggers (`trg_era_fits`,
  `trg_era_single_insert`, `trg_era_immutable`, `trg_era_not_deletable`), the
  rebuilt-history triggers (`trg_rebuilt_history_insert`,
  `trg_rebuilt_history_immutable`, `trg_rebuilt_history_not_deletable`).
- **Depended on by:** mandatory snapshot-derived WS claims, the adopt rule,
  and the feed's pages across the rebuilt/live seam. Identity is validated
  before the requested count, including for empty history.
- **Breaks:** a client silently resumes another era's history or an
  inaccessible prefix.
- **Operational boundary:** setup refuses a data directory that holds anything
  but its lock file (exit 30) before writing anything, and runs every L1 check
  that can fail retryably first, so a retry finds the directory empty; a setup
  that fails later leaves a directory to discard. `publish_era` refuses a
  completed database or one holding a batch. The base it starts from is
  trusted; neither clone detection nor distributed fencing is implied.

### I19. Application progress follows the shared execution contract

- **Holds:** the application's progress is its L2 tx count (`L2TxCount`) alone.
  An executed transaction returns its pre-execution count as its index
  (`Executed::Done { index, .. }`) and leaves the count at exactly
  `count + 1`; an executed user op also leaves its sender's nonce at exactly
  `nonce + 1`. A skipped op (wrong nonce) changes nothing. `AppError` is fatal
  and defines no canonical successor.
- **Enforced by:** the application owns and persists the count and nonces.
  Every execution consumer calls `sequencer_core::application::execute`, which
  compares the op's nonce, preflights count overflow, and asserts both
  successors after a successful hook. Admission purity and native-engine
  mutation remain self-trusted; the count does not need a Rust-side mirror.
- **Depended on by:** the canonical runner, inclusion lane, catch-up, durable
  execution attribution, and the era-checked replica protocol.
- **Breaks:** a transaction can be applied without advancing history, an
  offset can advance twice, or an op can run at a nonce the canonical machine
  skips — silent application-history divergence.
- **Scope:** application-specific mutation and determinism remain self-trusted.
  A failing hook is not rolled back; every production caller terminates that
  path and discards the instance.

### I20. Application history is committed with its execution receipts

- **Holds:** `application_inputs` contains every current included user op and
  every external direct within the application's payload cap
  (`scheduler_limits().max_direct_payload_bytes`) exactly once, keyed by
  mandatory pre-execution count. A direct over the cap never executes and has
  no row; one SQL predicate (`DIRECT_EXECUTES_SQL`, `storage/queries.rs`)
  selects the directs to execute in every query, and it reads both its
  operands, the submitter and the cap, from the pinned `deployment_identity`
  row. Setup pins the cap, and the staleness window with it; `run` refuses a
  binary whose cap or window differs. A
  direct's source reference, owning batch/frame, and payload tables
  reconstruct replay.
  There are no entries for batch envelopes, and none below the rebuilt
  history end `E`: the era's rows before it are `rebuilt_history`'s
  ([I18](#i18-history-identity-is-published-with-the-era)).
- **Creation atomicity:** user-op source rows and application rows commit in the
  same FULL chunk transaction that authorizes acknowledgements. Direct inputs
  commit with the complete frame rotation, after checking all execution receipts.
  Startup creates leading direct rows before restoring the engine;
  successful catch-up is required before admitting that sequence.
- **Append-only:** no application input is ever deleted (I5).
- **Snapshot/replay agreement:** a batch-close snapshot records storage-derived
  `H`; the restored engine must report the same count. Each replay row must
  match the engine's next count and executes with its persisted frame fee and
  safe block (or the direct input's index, block and timestamp). Missing rows
  or count mismatches fail loud; they are never repaired or backfilled.
- **Enforced by:** shared execution receipts, storage append APIs, PK/FK/XOR/
  uniqueness and contiguous-offset triggers, coherent canonical pages, and
  catch-up checks.
- **Performance boundary:** head discovery uses the integer primary-key maximum;
  replay seeks directly by offset and joins bounded source rows. Chunk
  insertion adds no durability transaction or actor.

## Do-not-simplify (deliberate shapes that look like cleanup targets)

The refactorer-facing mirror of the register above — each of these *looks*
like a simplification and would break a registered invariant:

- **Don't move filesystem work into `storage/snapshots.rs`** — the
  module boundary *is* the GC crash-ordering guarantee (I13).
- **Don't derive application order from L1 positions** — optimistic user ops
  precede their envelope and have no general one-to-one L1 mapping.
- **Don't retain a numeric resume offset without its history identity** —
  a new era reuses counts (I18).
- **Don't discard a snapshot beyond the accepted frontier** — that exact
  batch can become the next comparison checkpoint (I7).
- **Don't add internal retry loops to the flusher/submitter for provider
  errors** — the orchestrator respawn is the retry mechanism; internal
  retries mask the failures the exit codes and the landing lag report.
