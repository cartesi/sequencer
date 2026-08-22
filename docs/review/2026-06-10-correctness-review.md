# Whole-Project Correctness Review — 2026-06-10

**Status:** findings ledger, fixes pending. Each finding carries a checkbox; tick it
(and link the commit/PR) when resolved. Design resolutions in §2 were settled in the
review discussion and are the agreed direction; everything else is open.

**Provenance note (2026-07-23):** `docs/review/` was accidentally dropped from the
squash that merged the cockroach branch to main (`8ce41cf`) — AGENTS.md/CLAUDE.md
kept pointing at it while it was absent. Restored from `0561d53`, with statuses
updated to reflect what landed since (see F7/WP5).

**Method.** Twelve parallel module reviews (one per subsystem, each reading code +
normative docs), every medium/high concern independently adversarially verified
against the tree (verifiers instructed to refute), plus a line-by-line pass over the
core files (scheduler fold, `scheduler_accepts`, inclusion lane, ingress/recovery
storage, schema, catch-up, snapshot lane, flusher). Refuted concerns are kept in §6
so future reviews don't re-litigate them.

**Headline.** The sequencer/scheduler duality itself is in good shape — the three
implementations of "what does the scheduler accept" (canonical fold,
`ProtocolTiming::scheduler_accepts`, the lane's live prediction) were checked
against each other from multiple angles and are consistent, including drain
attribution, staleness boundaries, flattened replay order, and empty-batch
semantics. The confirmed problems cluster at the boundary between the sequencer
and the infrastructure it stands on: fsync semantics, the local node's mempool
memory, RPC fleet coherence, and the subscriber protocol.

Severity: **high** = potential scheduler/sequencer state divergence, fund loss,
crash-loop, or data corruption; **medium** = real correctness issue with bounded
blast radius; entries in §4/§5 are robustness/hygiene.

---

## 1. Confirmed findings

### F1 (high) — Flush slot coverage has no durable anchor; zombie batches re-enable the counterexample TLA+ excludes

- [x] Fixed in: WP2, 2026-06-11 — R1a watermark landed: `wallet_nonce_watermark` singleton, write-before-broadcast in the poster + flusher (`WalletNonceWatermarkSink`), flush completion anchored on `safe >= watermark + 1` (Anvil test `flush_covers_watermark_slots_the_pool_forgot`). The R2 content-identity backstop landed in WP3 (2026-06-12), completing the loop.

`docs/recovery/README.md` Implementation Constraint 1 requires `walletNonce` — the
highest wallet-nonce slot ever assigned — to be durable and never reset, so the
flush consumes every dead-batch slot. The implementation re-derives it from the
local node's volatile view at every use:

- `MempoolFlusher::flush_and_wait` returns immediately when
  `get_transaction_count(Pending) <= get_transaction_count(Safe)`
  ([flusher.rs:122-131](../../sequencer/src/recovery/flusher.rs)) and submits
  no-ops only for `[Latest, Pending)` per the local node.
- The poster assigns new slots from `get_transaction_count(Latest)`
  ([poster.rs](../../sequencer/src/l1/submitter/poster.rs)).
- The schema persists no wallet-nonce bookkeeping at all.

If a broadcast batch tx at slot *k* is dropped from the local node's pool (node
restart, pool eviction — fail-stop behavior the threat model tolerates) while
surviving in the wider network (the mempool is fully adversarial; zombies are a
named first-class threat), then at recovery: the flush sees `pending == safe`,
submits **zero** no-ops, and returns; `recover_post_flush` cascades the batch as
"Pending no-op'd"; the resumed submitter assigns the recovery batch (same
scheduler nonce N, different content) to the **same slot k**. Zombie and recovery
batch now compete for one slot — exactly the wallet-nonce-mutual-exclusion
counterexample in `docs/recovery/history/`. If the zombie lands fresh by
inclusion (the preemptive margin guarantees a window where this is possible), the
scheduler executes the *invalidated* content, `populate_safe_accepted_batches`
accepts it (nonce matches; content is never compared), and the system diverges
silently and permanently.

**Fix (settled):** persisted wallet-nonce watermark — §2 R1. Detection backstop:
content-identity check — §2 R2.

### F2 (medium) — Post-flush re-sync has no coherence check against the flusher's observation

- [x] Fixed in: WP2, 2026-06-11 — `flush_and_wait` returns the safe block it observed resolution at; `run_flush_and_cascade` refuses to cascade (`RecoveryError::ResyncBehindFlushView`, orchestrator respawn retries) when the re-synced `current_safe_block` lags it

`run_flush_and_cascade` ([recovery/mod.rs:329-372](../../sequencer/src/recovery/mod.rs))
waits for `Pending <= Safe` on the flusher's provider connection, then re-syncs the
safe head through the input reader on a separate connection, then cascades
unconditionally. The documented precondition on
[`Storage::recover_post_flush`](../../sequencer/src/storage/recovery.rs) — the gold
frontier "must reflect the latest safe head" — is never verified. Behind a
load-balanced RPC endpoint (which `docs/recovery/README.md` itself names as the
most common real-world degradation), the reader can be served by a replica lagging
the flusher's view by minutes, miss batches that landed-and-finalized during the
flush window, under-extend the frontier, and cascade a batch the scheduler
actually accepted — the recovery batch then reuses its nonce: same divergence
shape as F1.

**Fix:** `flush_and_wait` returns the safe block number at which it observed
resolution; `run_flush_and_cascade` asserts the re-synced `current_safe_block`
is `>=` that value before cascading (retry/respawn otherwise).

### F3 (high) — External effects are emitted on non-durable WAL commits (`synchronous=NORMAL`)

- [x] Fixed in: WP1 durability flip, 2026-06-11 (single-PR branch) — `synchronous=FULL` via the single `SYNCHRONOUS_PRAGMA` constant; round-trip benchmark deltas were noise-level on NVMe (p50 24.1 ms vs 23.8 ms baseline, p95 35.6 vs 35.8, concurrency 8)

[`open.rs`](../../sequencer/src/storage/open.rs) sets WAL + `synchronous=NORMAL` on
all writer connections: commits survive process crash but **not** power loss / OS
crash before the next checkpoint. Two externalization points outrun durability:

- The lane acks `POST /tx` immediately after the (non-fsynced) chunk commit
  ([inclusion_lane/mod.rs:222-226](../../sequencer/src/ingress/inclusion_lane/mod.rs)).
- The submitter reads sealed batches and broadcasts them to L1.

Worst case: batch B (scheduler nonce N) is sealed and its L1 tx mined; power loss
rewinds the DB so B is the open Tip again; the lane re-seals nonce N with
different content (B-v2); the scheduler executed B-v1 while local replay,
snapshots, and successor batches are built on B-v2 — silent divergence that no
danger arm detects (acceptance and promotion are keyed by nonce only). Lesser
case: acked user ops vanish outside the documented invalidation path.

This is in-model, not theoretical: the repo's own crash bar includes kernel
crashes — `Application::create_dump` explicitly fsyncs against WAL/file
reordering. The DB is the unguarded half of an invariant the dump side already
pays for.

Note: the re-seal divergence case is *detected* (not prevented) by §2 R2.

### F4 (medium) — GC unlinks dump directories after a non-durable row delete

- [x] Fixed in: WP1 durability flip, 2026-06-11 — under `synchronous=FULL` the GC's row-delete commit fsyncs before `delete_dump_dir` unlinks, so a power loss can no longer resurrect a row pointing at a missing directory

`run_gc` ([snapshot.rs:61-73](../../sequencer/src/ingress/inclusion_lane/snapshot.rs))
deletes `dumps` rows in one (non-fsynced) tx, then removes directories on disk.
Power loss before the next checkpoint can rewind the row delete while the unlink
persists: a resurrected `finalized_snapshot`/pending row points at a missing
directory. The startup sweep only handles the inverse (orphan dirs); startup GC
won't remove a still-referenced row. If the resurrected row is the resume
checkpoint, `A::from_dump` fails → lane exits → restart hits the same row →
**crash-loop with no automatic escape**, violating the documented "no dangling
row" invariant under the same kernel-crash model `lifecycle.md` §7 claims to
defend against.

**Fix:** fsync/checkpoint the WAL after the GC commit and before `delete_dump`
(automatic under `synchronous=FULL`), or defer file deletion to the next startup.

### F5 (high) — Safe-range scan trusts `get_logs` completeness against a separately-fetched safe head

- [x] Fixed in: reader InputBox-index contiguity check (structural layer 2),
  external review 2026-06.

`advance_once` ([reader.rs:185-250](../../sequencer/src/l1/reader.rs)) fetches the
safe head (RPC call 1), issues `eth_getLogs` over `[floor+1, safe]` (call 2,
possibly served by a different backend), and persists the call-1 head on any `Ok`.
Geth-lineage nodes silently clamp `toBlock` to their local head and return partial
logs without error. Behind a load-balanced fleet, deposits in the tail of the
range are permanently skipped — the floor never re-scans below the persisted
head. The sequencer then advances frames' `safe_block` past a deposit it never
executed while the scheduler force-drains it: canonical state divergence.

The threat model scopes this away ("Our own node; honest by assumption",
`docs/threat-model/README.md:51`), but nothing in code or configuration enforces
or documents single-consistent-node-only, and
[provider.rs](../../sequencer/src/l1/provider.rs) /
[partition.rs](../../sequencer/src/l1/partition.rs) explicitly anticipate
Infura/Alchemy/QuickNode.

**Fix (two layers):**
1. Operational: after the logs query, confirm the serving node's view covers the
   scanned end block (re-fetch head/safe and require `>= range end`), or pin the
   scan end to the same response's view.
2. Structural (stronger): the reader currently **drops the on-chain input index**
   carried by `EvmAdvanceCall`; persist it and reconcile against the local
   `safe_input_index` assignment (`MAX+1`). Any gap → fail loud. This converts
   silent input loss from any cause into a detected refusal, and is cheap.

**Resolution (2026-06):** both layers landed in `advance_once`, run before the
safe head is persisted:
1. **Right prefix (contiguity).** It reads each `InputAdded.index` (per-app,
   gap-free on-chain counter) and `check_input_index_contiguity` asserts the
   ingested run continues densely from the stored count — equal to the assigned
   local `safe_input_index`, since we ingest every event for this app from
   genesis. Catches a dropped *middle* input.
2. **Complete prefix (count witness).** `check_input_count_complete` queries
   `InputBox::getNumberOfInputs(app)` *pinned at the scanned safe block* and
   requires it equals what we now hold (stored + received). Catches a truncated
   *tail* — which is still a contiguous prefix, so step 1 alone would miss it —
   on the same tick, before persisting. Pinning to the safe block also forces
   the serving node to have that block's state, so it largely subsumes the
   originally-proposed layer-1 same-view bound (a lagging replica errors or
   returns the true count).

Either failure returns a retryable `InputReaderError::InconsistentL1Response`:
the partial set is never persisted and a consistent provider self-heals on the
next tick.
**Residual:** only a *byzantine* node that lies about both the logs and the count
consistently could slip through — outside the fail-stop threat model. (`F2`, the
flush/re-sync view desync, is separately **fixed** — WP2's `ResyncBehindFlushView`
resync-lag check — and unaffected by this change.)

### F6 (medium, fixed) — Cached-identity fallback could skip chain-id validation

- [x] Fixed in: setup/run split (`8ce41cf`) plus reader-side first-contact
  verification.

At review time, `validate_rpc_chain_id` ran only in the
`InputReader::new`-success branch
([runtime/mod.rs:141-176](../../sequencer/src/runtime/mod.rs)). On the
L1-unreachable bootstrap path the process started from cached deployment identity
and never re-checked the RPC's chain id; the reader filtered only by InputBox
address and app-contract topic ([partition.rs:33-37](../../sequencer/src/l1/partition.rs))
— both address-based, and Cartesi contracts are commonly CREATE2-deployed at
identical addresses across chains, so wrong-chain events could pass. A
wrong-chain RPC (typo, LB flip) after a fallback boot could feed wrong-chain
`InputAdded` events into `safe_inputs`.

**Resolution:** `InputReader` verifies the pinned chain id on its first
successful provider contact, before reading or persisting an L1 view; a
transport failure leaves the check armed. Reader-level
`advance_once_refuses_wrong_chain_id` and run-bootstrap integration test
`run_refuses_on_wrong_chain_rpc` pin both boundaries.

### F7 (high) — WS feed has no invalidation/rollback signal; cursor-resumed subscribers silently diverge across recovery

- [ ] Fixed in: _pending_. PR #26's context fields and the durable
  history/canonical-offset storage foundation are landed, but
  `/ws/subscribe` still uses physical rowid and has no required
  `HistoryVersion` claim. The invalidation contract therefore remains open —
  and `batch_nonce` cannot substitute for it: recovery reopens the Tip at the
  **same** batch nonce, so a cursor-resumed stream stays offset- and
  nonce-monotone while silently containing dead rows. The interim consumer
  rule remains: mirrors treat **any** socket drop as a potential discontinuity
  and rebuild their soft suffix. Closure is owned by the
  [Track 3 ordered handoff](../plans/2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff)
  and requires the WS v2 cutover plus B3 tests.

The feed streams Tip rows (soft confirmations) and documents cursor-based
reconnect (`from_offset`). Recovery invalidates already-streamed rows and
re-drains direct inputs at **new** offsets. A subscriber reconnecting with its
pre-recovery cursor keeps every invalidated tx as confirmed, receives re-drained
directs a second time, and cannot detect either: offset gaps are normal (batch-
submitter rows are filtered), and the protocol carries no epoch/generation
marker ([subscribe.rs](../../sequencer/src/egress/api/subscribe.rs),
[broadcast.rs](../../sequencer-core/src/broadcast.rs)). README's claim that the
feed "match[es] the on-chain execution order" holds only for fresh
subscriptions.

**Fix shapes** (decide at implementation): a stream-generation id in the WS
handshake that changes on any cascade; an explicit invalidation event; or at
minimum a documented requirement that reconnect goes through `/latest_snapshot`.

### F8 (medium) — Wall-clock regression wedges batch close and the recovery cascade

- [x] Fixed in: WP6, 2026-06-11 — the two cross-column CHECKs dropped (timestamps keep NOT-NULL/`>= 0` and the write-once triggers); `should_close_batch_by_time`'s silent stall got the explanatory comment

`sealed_at_ms >= created_at_ms` and `invalidated_at_ms >= created_at_ms` are
schema CHECKs ([0001_schema.sql:27-30](../../sequencer/src/storage/migrations/0001_schema.sql))
stamped from the raw wall clock. A backwards clock step (NTP correction, VM
resume) larger than the open Tip's age makes every `seal_batch` fail the CHECK
(lane exits; under load this is a crash-loop until the clock catches up), and —
worse — `cascade_invalidate_from` stamps `invalidated_at_ms = now` on rows
seconds old, so **startup recovery itself** can fail → respawn → same failure.
The safety-net path's liveness is coupled to clock monotonicity.

Note the irony: these cross-column CHECKs are defense-in-depth against self-bugs
(which repo policy explicitly avoids) and they bought a liveness wedge.

**Deep-dive verification (2026-06-11):** no production code reads
`sealed_at_ms` or `invalidated_at_ms` as *values* — every reader is an
`IS NULL`/`IS NOT NULL` predicate (the `valid_*` views and the write-once
triggers). The CHECKs guard an ordering nobody consumes.

**Fix (recommended):** drop the two cross-column CHECKs (keep NOT-NULL/≥0);
the timestamps are observability stamps, the write-once triggers stay. The
clamp alternative (`MAX(now, created_at_ms)`) is acceptable but keeps a
constraint that buys nothing. Note `should_close_batch_by_time`'s
`duration_since().unwrap_or_default()` independently makes the *time-based*
close trigger stall silently during regression (size trigger unaffected) —
fine to leave, worth a comment.

### F9 (medium) — Blanket pending-snapshot clear deletes still-valid gold pendings; safety rests on an undocumented three-way coupling

- [x] Fixed in: S3+WP6, 2026-06-11 — both recovery paths share `cascade_and_reopen`, and the clear is scoped to `nonce >= pivot.nonce` (`clear_pending_dumps_from_nonce_in`); regression test `cascade_preserves_gold_unpromoted_pending_below_pivot`; lifecycle.md §8 + invariants I4/I5 rewritten to match

Both recovery paths run `clear_pending_dumps_in` (unfiltered `DELETE FROM
pending_snapshots`) whenever the cascade invalidated anything
([recovery.rs:298-313](../../sequencer/src/storage/recovery.rs)). The justifying
comments ("states the canonical replay will never reach" / "batches that haven't
been observed landing on L1 yet") are **false** for gold batches accepted during
the startup sync but never lane-promoted — routinely reachable after extended
downtime. The reason this doesn't recreate the §6 promote-wedge crash-loop from
`lifecycle.md` is an unstated coupling: the clear runs only when the cascade ran
⇒ `open_fresh_tip_in_tx` runs **in the same tx** ⇒ it sequences the *entire*
undrained safe-input backlog ⇒ the lane never re-observes those landings, so
`promote_finalized_in` never fires on a cleared nonce. Any future change that
scopes the reopen drain, makes the clear unconditional, or re-walks sequenced
inputs silently re-arms the wedge (gold batches never age into a cascade, so
there is no automatic escape).

Side effect today: the skipped promotion leaves `finalized` lagging until the
next post-recovery batch lands; restarts catch up from an older checkpoint.

**Deep-dive addition (2026-06-11): there is a *fourth* coupling.** The blanket
clear in the `RecoverTip` path would be a genuine crash-loop wedge if any
*valid in-flight* closed batch existed at clear time: its pending row is
deleted while the batch stays valid; when its tx later lands accepted, the
lane's promotion hits the deleted row (`QueryReturnedNoRows`) and — because
everything is young and healthy — **no danger arm ever fires to heal it**.
This is unreachable today only because of `check_danger`'s arm ordering:
`ClosedBatchInDanger` is checked before `TipInDanger`, and frame-safe-block
monotonicity makes the closed frontier always at-least-as-old as the Tip, so
a Tip-only cascade implies all closed batches are gold. The clear's safety
thus rests on the ordering of two `if` statements in `check_danger` plus a
monotonicity argument, documented nowhere near the clear. (Pre-watermark, the
F1 zombie also manifests through this same promote-wedge: a zombie accepted at
the recovery nonce before the recovery Tip closes has no pending row at all.)

**Fix (recommended):** scope the delete to nonces `>=` the cascade pivot's
nonce. Verified consequences: doomed pendings deleted exactly; gold-unpromoted
pendings preserved (catch-up resumes from a *fresher* checkpoint; rows are
cleaned by the next promotion's `DELETE <= max_nonce`); `RecoverTip` deletes
nothing (the Tip never has a pending); the torn-cascade case degenerates to
the blanket clear (pivot nonce 0). Scoping removes the wedge-arming
structurally — any nonce the lane could later observe as accepted either has
its pending row intact or belongs to a post-recovery batch with a fresh row —
instead of leaning on three-to-four cross-file couplings. Fix the two comments
and `lifecycle.md` §8 to match.

### F10 (medium) — `Application::execute_direct_input` defaults to a silent no-op

- [x] Fixed in: trait-pruning wave, 2026-06-11 (single-PR branch) — both methods required; the same wave also deleted the dead `current_user_nonce`/`current_user_balance` and hoisted `validate_and_execute_user_op` to a non-bypassable free function (PLAN.md §7)

Deposits are direct-input-only, making `execute_direct_input` the sole
deposit-crediting path; the trait defaults it to `Ok(Vec::new())`
([application/mod.rs:131-133](../../sequencer-core/src/application/mod.rs)). An
`Application` that forgets to override compiles and runs cleanly while every
deposit is escrowed on L1 with no L2 credit (symmetrically on both sides, so no
divergence — just silent fund inaccessibility). `executed_input_count` has the
same hazard. Make both required; test stubs can implement them explicitly.

---

## 2. Design resolutions (settled in review discussion)

### R1 — Two-path flush: durable watermark for standard recovery; best-effort for cockroach recovery

**R1a — Standard recovery (X): wallet-nonce watermark.**

New persisted singleton: `wallet_nonce_watermark` = the highest wallet nonce ever
*broadcast* by this deployment's submitter key. Rules:

1. **Write-before-broadcast.** Any component about to broadcast a tx at wallet
   nonce `n` first commits `watermark = max(watermark, n)` durably, then sends.
   Uniform rule for *every* broadcast from our key — batch txs and flush no-ops
   alike — so the invariant is simply "watermark ≥ nonce of everything we ever
   sent" with no case analysis. A crash between commit and send only over-covers
   (the flush later no-ops a never-used slot: one wasted no-op, harmless).
2. **Flush condition.** `flush_and_wait` is done iff
   `pending <= safe && safe >= watermark + 1`. The current early-return
   (`pending <= safe` alone, [flusher.rs:125](../../sequencer/src/recovery/flusher.rs))
   is exactly where the F1 hole lives: it consults only the local node's memory.
3. **No-op range.** `[latest, max(pending, watermark + 1))` instead of
   `[latest, pending)`.
4. **Post-flush assert.** `get_transaction_count(Safe) >= watermark + 1` —
   fail-loud confirmation that every slot we ever used is resolved at safe depth.
5. **Submitter unchanged.** `decide_submit_start` and the re-broadcast-at-same-
   slot liveness behavior stay as they are; the watermark is the *flush's* floor,
   not the poster's.

Why this is sound: nonces are sequential, so any zombie of ours sits at a slot
`k ≤ watermark` with `k ≥ latest_count`; the widened no-op range covers it, the
mutual-exclusion race resolves it (either outcome is fine once a competitor
exists), and the wait condition refuses to declare victory until slot
`watermark` is consumed at safe depth. This restores TLA+ Implementation
Constraint 1 with a durable realization of `walletNonce`. Genesis: watermark
absent ⇒ nothing ever broadcast ⇒ flush trivially complete.

**Durability dependency:** the watermark commit must actually be on disk before
the broadcast — under `synchronous=NORMAL` it isn't guaranteed. R1a therefore
depends on R3 (either `synchronous=FULL` or an explicit checkpoint on this
commit). Without it the anchor has a power-loss hole of its own.

**Doc updates owed:** `docs/recovery/README.md` Implementation Constraints
(mechanism for Constraint 1), Step 3 description, and the TLA+ correspondence
notes.

**R1b — Cockroach recovery (Y): best-effort flush, explicitly.**

Y starts from a wiped DB: no watermark survives. Y's flush is therefore
*best-effort by construction* — it resolves every slot the provider remembers,
and cannot resolve a zombie the provider has forgotten. Consequences to state
plainly in PLAN.md:

- Y's **detection** step (plain `setup` refusing when `pending != safe`) has the
  same false-negative: a forgotten zombie keeps `pending == safe` locally.
- Post-Y, a zombie batch carrying exactly the resume nonce `N` can land fresh
  and diverge. The wallet-slot competition with the new instance's first batch
  is a coin flip, not a defense.
- Optional mitigation: `setup --recovery --flush-through-nonce <n>` — an
  operator-supplied watermark. Its only real source is the **previous
  instance's DB** (the persisted watermark singleton): explorer history shows
  the ledger — landed txs — which is exactly the set the best-effort flush
  already resolves via on-chain nonce counts; the gap is txs that neither
  landed nor survive in the provider's pool, visible nowhere public. Key
  property: the watermark is **fail-safe under corruption** — read from an
  untrusted, half-destroyed old DB, too high costs a few wasted no-ops, too
  low degrades to exactly best-effort (backed by R2). So Y may spelunk the old
  DB for this one value without violating its don't-trust-local-state premise.
- The actual backstop that makes best-effort *acceptable* is R2: the residual
  failure becomes detected-and-recoverable instead of silent. The repair loop is
  Y itself: on a detected divergence, wipe and re-run `setup --recovery`; the
  fold replays the zombie's content as canonical and the rebuilt state is
  correct. (Cockroach recovers from the failure of its own flush.)

### R2 — Batch content-identity check (the divergence backstop)

> **Landed in WP3, 2026-06-12** (single-PR branch): hash-at-seal on the
> `batches` row (keccak256 of the submitter's encode path, stamped in the
> seal UPDATE, write-once trigger); acceptance-gated check in
> `populate_safe_accepted_batches`; `canonical_divergence` poison marker
> written **atomically with the detecting sync** (the frontier freezes
> instead of the sync aborting — strictly more atomic than the
> abort-then-mark sketch below, same effect); `DangerStatus::
> CanonicalDivergence` ranked ahead of every arm; terminal
> `Refuse(CanonicalDivergence)` dispatch. Tests: hash-at-seal identity,
> foreign-landing freeze, mismatch freeze + frozen-forever, dispatch
> mapping. Test fixtures now seed accepted landings with the local batch's
> real wire bytes (`local_batch_payload`).

The gold frontier currently equates "scheduler accepted nonce N" with "our valid
batch carrying nonce N" — identity by nonce only, content never compared. Every
silent-divergence path in this review (F1 zombie-wins, F3 power-loss re-seal,
Y's residual) flows through that equation breaking.

**Check:** in `populate_safe_accepted_batches`, gated on **full acceptance** —
exactly where `scheduler_accepts` returns `Some` (sender + decode + fresh-by-
inclusion + expected nonce):

1. Look up our **valid closed** batch with nonce N. If none exists, the landed
   batch is foreign (we never submitted a closed batch at N) → divergence.
2. If it exists, compare the on-chain payload bytes against ours. **Preferred
   variant: hash-at-seal** — persist the payload hash on the `batches` row at
   seal time, computed by the same encode path the submitter uses
   ([l1_submission.rs](../../sequencer/src/storage/l1_submission.rs)); the
   check is then a hash lookup, and it survives wire-format upgrades (an old
   batch compares against bytes produced by the code that sealed it).
   Re-encode-at-check is an acceptable v1.

**The gate matters.** "Expected nonce" alone is not sufficient: a
nonce-matching but *stale* zombie is skipped by the scheduler — a true no-op,
no divergence — and late-landing dead resubmissions are routine, so comparing
their content would false-positive. Rejected/stale/undecodable copies are
scheduler no-ops; their content is irrelevant by construction.

**Why content (not identity) suffices.** Batches were deliberately designed
without an identifier. Content-equal copies are *effect-equal*: an accepted
batch's app effects depend on its inclusion block only through
`force_execute_overdue`, and for any fresh copy the force-executed prefix is a
subset of the first frame's drain (fresh ⇒ `inclusion − MAX_WAIT <
first_frame.safe_block`), in the same queue order. So which physical tx landed
carries no semantic weight — indistinguishability cuts both ways.

**On detection:** persist a poison marker (committed even though the sync
aborts), surface as a new danger arm — `DangerStatus::CanonicalDivergence` →
`Refuse` at startup, detector-exit at runtime. The marker must rank **ahead of
every other danger arm** so the orchestrator's respawn loop cannot route a
diverged node into `Proceed`/`FlushAndCascade` (e.g. running a flush+cascade on
top of a diverged frontier). The remedy is **cockroach recovery, never standard
recovery**: X reconciles the *shape* of the batch tree under the assumption
that accepted nonce N is our batch N; a content mismatch means canonical state
contains executed effects with no reliable local source (Y-residual: not in the
DB at all; F1: in invalidated rows, but un-invalidating would have to unwind
soft-confirmed successors built on the other fork). Rebuild-from-L1 is the only
honest repair, and the Y fold — which executes actual on-chain payloads —
converges by construction.

**Detection latency (contract language):** the check fires when the divergent
landing reaches *safe* and the reader syncs it, so soft confirmations issued in
that ~2-epoch window are built on already-diverged state. Inherent to the
optimistic model; bounded; should be stated where soft-confirmation honesty is
documented.

**Bonus coverage:** the check also neutralizes the worst case of the
structural-reject simulation gap (§6 first entry): a foreign batch at the
expected nonce that the real scheduler structurally rejects would today be
sim-accepted and silently desync the frontier forever; with the check it fails
content comparison and flags. (Under self-trust, a foreign batch from our key
means key compromise — refuse-into-Y is the right severity for that too.)

**Why this is not "defense-in-depth against self-bugs":** the adversary is the
L1 mempool (fully adversarial per the threat model) replaying *our own stale
transactions* at times we don't control. This is trust-boundary validation of
external input, exactly what the threat model prescribes. It also converts
F3's worst case and any future flush-coverage gap from silent divergence into a
detected refusal.

**Cost:** one SSZ encode + compare per accepted batch (rare, off hot path).
**Edge:** a wire-format change between versions could false-positive across an
upgrade — acceptable; it surfaces as a refusal the operator resolves with Y.

### R3 — Durability decision (settled in storage deep-dive: `synchronous=FULL`)

Set `synchronous=FULL`. Evidence from the deep dive:

- **One-line change with total coverage.** Every production writer funnels
  through `open_writer_connection` and the single `SYNCHRONOUS_PRAGMA` constant
  ([open.rs:20](../../sequencer/src/storage/open.rs)) — including the
  `LeaseGuard`'s drop-time reconnect (`Storage::open_writer`). No second
  pragma site to forget.
- **Write-frequency analysis.** The only hot-path commits are the lane's chunk
  commits (single-threaded, so the fsync serializes into the existing ack
  path); everything else is low-frequency — frame rotation per frontier
  advance (~12s at the time of this review; now a five-safe-block clock), batch
  close, reader append per safe-head advance, lease
  acquire/release per snapshot HTTP request, GC after promotion. One fsync per
  chunk commit (≈0.05–1 ms NVMe, ≈1–10 ms cloud block storage) against the
  500 ms ack budget is noise.
- **Verification before merge:** run `tests/benchmarks` ack/round-trip suites
  before and after the flip; only if they regress materially, fall back to
  targeted barriers at the four externalization sites (ack, submitter pickup,
  GC unlink, watermark broadcast). One pragma beats four hand-maintained
  barrier sites unless the numbers force otherwise.

This resolves F3 and F4 wholesale and is a precondition for R1a's
write-before-broadcast guarantee. Update AGENTS.md's "ack tied to chunk
durability" to state the (now true) power-loss-durable meaning.

### R4 — Process exit-code contract (signed off 2026-06-11; implementation = WP10)

Today `main() -> Result<(), RunError>` exits 1 on every failure (101 on panic):
the orchestrator cannot distinguish "restart me" from "restarting is futile"
without parsing logs — exactly what the recovery README says it shouldn't have
to do. The `RunError` taxonomy ([runtime/error.rs](../../sequencer/src/runtime/error.rs))
already carries every needed distinction; the contract is a projection of it,
implemented as one `exit_code()` match in the **binary** (the `run()` library
API is unchanged). Classes, by *restart productivity*:

| Code | Class | Maps from | Orchestrator policy |
|------|-------|-----------|---------------------|
| 0 | clean shutdown | graceful SIGINT or SIGTERM | — |
| 1 / 101 | unclassified failure | worker crashes, provider errors, panics — **superseded for trusted-contract panics by the 2026-07-30 amendment below** | restart with backoff |
| 10 | restart; **expect a recovery boot** | `DangerDetected{ClosedBatchInDanger\|TipInDanger}` | restart; next boot may legitimately take 15+ min (flush + safe-finality wait) — startup probes/deadlines must accommodate it |
| 20 | restart; transient refusal | `DangerDetected{L1ViewStale\|EstimatedBatchInDanger}`, `Refuse(L1ViewStale\|EstimatedBatchInDanger)`, `FirstBootRequiresL1`, `ChainIdRpc` | restart with backoff; these self-heal when the L1 view freshens (each boot re-syncs before re-checking); alert only if persisting |
| 30 | terminal; operator required | `CanonicalDivergence` (R2 marker), `IdentityError::Mismatch`/`OrphanedState`, `ChainIdMismatch`, `InvalidProtocolTiming` | do not restart (systemd `RestartPreventExitStatus=30`); page |

Notes pinned by the design:

- **The exit code is an ops hint, never protocol.** Authority over what the
  next boot does stays with startup's own `check_danger` re-check; the code is
  a prediction for restart/alert/deadline policy only.
- **Class 10's concrete payoff** is the startup-deadline problem: a recovery
  boot runs the flush (~13–15 min mainnet) *inside* the new process; an
  orchestrator that kills slow boots would interrupt recovery. The code makes
  "this restart will be slow, that's expected" machine-readable.
- **Class 20 vs 30 is the load-bearing split**: all *existing* Refuse arms are
  transient (they self-heal when the provider recovers) — conflating them with
  terminal would page operators for conditions that resolve themselves.
  `CanonicalDivergence` (R2) is the first genuinely terminal state, and is what
  makes this contract worth having; "further boots fail" is enforced by its
  persisted marker (and naturally true for identity/config mismatches), not by
  the exit code itself.
- **k8s nuance**: Deployments restart regardless of exit code; there the
  terminal class manifests as fast-refusing boots in CrashLoopBackOff plus
  alert rules on `lastState.terminated.exitCode`. systemd can act on the code
  directly. Document both in the ops guide.
- Reserve plain `1`, `2` (clap usage errors), and `101` (panic); deliberate
  codes start at 10. (`101` now applies only to panics the command harness
  cannot contain — see the amendment below.)

**2026-07-30 R4 amendment:** panics and typed row-shape failures proven to come
from trusted sequencer/application contracts are terminal (exit 30), including
those caught inside the command harness or supervised workers. This deliberately
supersedes the table's blanket "worker crashes, panics → 1/101" rule: an
unproven operational error remains restartable as exit 1, while retrying a
persistent invariant failure would only create a crash loop. A panic before
the command harness starts can still exit 101.

### R5 — Replace the "no defense-in-depth" policy with a fail-loud / fail-silent rule (signed off 2026-06-11; landed in AGENTS.md, threat model, and docs/invariants.md)

The current policy (AGENTS.md:261 "Don't layer defense-in-depth checks against
sequencer self-bugs"; threat-model "Self-trust") draws the line on the wrong
axis. Three pieces of evidence from this review:

1. **The codebase's best features already violate it.** The write-once
   triggers, tip-targeting triggers, nonce-contiguity trigger,
   `ux_single_valid_tip`, the lane's safe-head-regression and
   contiguity asserts, `seal_batch`'s row-count check — all are runtime
   checks against our own bugs, and the schema's own comment says so
   ("ensure the DB never reaches an inconsistent state if the writer
   misbehaves"). The threat-model text is even internally inconsistent:
   "internal invariants are enforced by type system, SQL constraints, and
   tests — not by defensive runtime checks" — SQL constraints *are* runtime
   checks.
2. **The hazards the review actually found are *silent absorbers* the policy
   doesn't address**: `INSERT OR IGNORE` advancing `expected` on a no-op
   insert, saturating conversions laundering corrupt rows,
   `unwrap_or_default` masking clock regression. These convert bugs into
   divergence — the worst outcome this system has.
3. **The one harmful "defense" (F8) is harmful for a different reason**: it
   checks a non-invariant (wall-clock monotonicity) and its failure mode
   wedges the safety path. The problem is *what* it asserts and *how* it
   fails, not that it doubts our code.

Proposed drop-in replacement (AGENTS.md + threat-model "Self-trust"):

> **Impossible states fail loud; they are never handled.**
>
> - An invariant violation gets exactly one response: abort the operation
>   loudly (assert, trigger `RAISE`, typed error). Cheap cross-module
>   assertions of contracts at boundaries are *encouraged*: in this system a
>   loud crash is recoverable by design (orchestrator respawn + startup
>   recovery), while a silently-tolerated bug that externalizes (signed
>   batch, ack, feed event) is divergence — theft-equivalent and
>   unrecoverable at runtime.
> - **Never handle gracefully what cannot happen.** No fallback branches, no
>   re-deriving a neighbor's answer to double-check it, no `Option`-handling
>   for can't-be-`None`. One contract, one source of truth, no second code
>   path. (This is the bloat the old policy rightly targeted — keep that.)
> - **Never absorb silently.** No `INSERT OR IGNORE`, saturating decode, or
>   `unwrap_or_default` on data that is impossible under the contract;
>   use the loud variant of the same operation.
> - An assertion must check a **real invariant** — true in every legitimate
>   execution, including crash-recovery, replays, and clock steps — never an
>   environmental assumption. (Cautionary tale: `sealed_at_ms >=
>   created_at_ms` is not an invariant under NTP corrections, and its CHECK
>   wedged recovery — F8.)

Decision test for any proposed check: (a) does it assert a real invariant,
(b) is it near-zero cost, (c) does it fail loud with no alternative code
path? All three yes → write it. Any no → don't.

The rule cleanly decides every case this review left ambiguous: F8's CHECKs
(drop — non-invariant), the contiguity trigger's NULL hole (fix — loud
variant of an existing check), `INSERT OR IGNORE` (replace with plain
`INSERT`), the direct-input double-sequencing trigger from §5 (now clearly
*allowed*: real invariant over valid rows, near-zero cost, loud), and R2's
content check (allowed without needing the "it's a trust boundary"
argument, though that argument remains true).

**2026-07-30 clarification:** “recoverable by design” above was too broad.
Failing loud preserves safety by preventing externalization; it does not imply
that restarting repairs persistent invalid state. Transient failures may clear
on restart, while a durable invariant violation is terminal and may require
inspection or cockroach recovery.

**2026-07-30 containment amendment (superseded 2026-08-01):** terminal
publication and runtime externalization used one shared/exclusive gate:
phase one closed admission and requested shutdown; phase two took the
exclusive side and published the sticky exit-30 evidence. The structural
consolidation (`docs/plans/2026-08-terminal-containment-structure.md`)
replaced classification-at-death with classification-at-birth. The final
interim ordering is containment bit + shutdown first, then best-effort marker
recording (file rung + DB row), because recording may block. The next boot
refuses when a rung survives (exit 30, stored cause,
`clear-terminal-fault` to exit). Externalization sites check the containment
bit; the boot refusal and scoped I15 schema-freeze triggers are partial
backstops. The accepted authority-boundary ADR owns the structural successor.



---

## 3. Doc-drift ledger

**Landed 2026-06-11** in the doc-drift + restructure pass. Two partial notes:
the flusher item's *error-log behavior* itself remains §4/WP9 (only the README claim is fixed here);
the TLA item is resolved by scoping the claim in the README — widening the spec guards stays an optional follow-up.

All confirmed; each is a doc fix unless noted. The repo treats these documents
as normative, so drift here is load-bearing.

- [x] **AGENTS.md:85 vs code + recovery README** — claims `Proceed` calls
  `recover_aging_tip` "defensively"; the `Proceed` arm performs zero DB writes
  ([recovery/mod.rs:243-256](../../sequencer/src/recovery/mod.rs)), and
  `docs/recovery/README.md` correctly says the opposite. The
  `recover_aging_tip` doc comment ("and defensively from Proceed",
  [storage/recovery.rs](../../sequencer/src/storage/recovery.rs)) has the same
  stale claim. Two normative docs currently contradict each other.
- [x] **`docs/recovery/README.md` "everything past gold is doomed" taxonomy** —
  the three-row table (Silver-stale / Silver-poisoned / Pending no-op'd) assumes
  every non-gold closed batch was submitted. A batch closed after the
  submitter's last tick was never broadcast, is not "doomed", and is cascaded
  anyway (defensible convergence policy; soft confirmations are really lost).
  Document the fourth case and the policy choice.
- [x] **TLA+ over-approximation claim** (`docs/recovery/README.md` Formal
  Verification + `preemptive.tla` header) — the spec discards an aging Tip at
  `MAX_WAIT_BLOCKS` while the implementation acts at `danger_threshold`, and
  `Resolve` has no killed-Pending-frontier case; the implementation's enabled
  transitions are a *superset* of the model's for these actions, so "safety
  over-approximation" does not hold action-for-action. Either widen the spec
  guards to danger-threshold semantics or scope the claim and state the
  external arguments explicitly.
- [x] **`lifecycle.md` §8 + recovery.rs comments on the pending clear** — see F9.
- [x] **`flusher.rs` retry log + recovery README flush note** — the `attempt`
  counter increments on every healthy finality-wait pass, logging
  `error!("flush retry: previous attempt timed out")` ~60 times per healthy
  recovery; and README's "the outer flush_and_wait loop is unbounded" is wrong
  for send failures, which return a hard `FlushError` (the unbounded retry is
  the orchestrator respawn).
- [x] **AGENTS.md:251** — `SEQ_L1_READ_STALE_AFTER_BLOCKS` documented as
  "derived"; code uses an independent fixed default of 600
  ([config.rs:113-120](../../sequencer/src/runtime/config.rs)) and validates
  `stale < danger_threshold` (an operator raising the margin past the
  equivalent point gets a startup refusal the doc doesn't predict).
- [x] **AGENTS.md:143** — names a nonexistent `storage/internals` module.
- [x] **[`submitter/mod.rs:6-9`](../../sequencer/src/l1/submitter/mod.rs)** —
  states the scheduler checks "nonces are strictly increasing"; the scheduler
  requires exact equality and rejects without consuming. The module's central
  at-least-once safety argument rests on the correct rule (which the code
  implements); only the doc is wrong.
- [x] **README.md WS shape** — `direct_input` example omits `sender` and
  `block_number`, both serialized ([broadcast.rs](../../sequencer-core/src/broadcast.rs))
  and needed for deposit attribution.
- [x] **Threat model fallback-RPC tier** — describes a primary own-node plus
  semi-trusted fallback (Infura/Alchemy); the code has exactly one
  `SEQ_ETH_RPC_URL` used by all components with no failover. Either build the
  tier or fix the table — and either way document the single-consistent-node
  requirement from F5.
- [x] **[`batch.rs:36-37`](../../sequencer-core/src/batch.rs)** — claims L1
  classification is "by attempting SSZ decode"; classification is by sender
  everywhere (AGENTS.md, reader). Wire-format reference file, worth fixing.
- [x] **[`snapshot.rs:84-86`](../../sequencer/src/ingress/inclusion_lane/snapshot.rs)
  + `lifecycle.md` §3** — "the lane retries on its next pass" after a
  `create_dump` failure; the lane actually exits the process (fail-loud), and
  retry happens via orchestrator restart + catch-up.
- [x] **[`0001_schema.sql`](../../sequencer/src/storage/migrations/0001_schema.sql)**
  — comment references `populate_safe_accepted_batches_inner`, which no longer
  exists.
- [x] **Empty-batch consensus semantics undocumented** — empty batches are never
  stale and consume the nonce (consistent in fold + simulation, test-pinned);
  AGENTS.md defines staleness via `first_frame.safe_block`, undefined for
  zero-frame batches. Also "stale skip = no state change" is imprecise (the
  arrival still force-drains overdue directs). One paragraph in AGENTS.md.
- [x] **`find_first_batch_in_danger` comment is direction-inverted**
  ([storage/recovery.rs:369-370](../../sequencer/src/storage/recovery.rs)):
  "if a closed batch is in danger, the Tip is older still" — backwards; the
  closed frontier has the smaller `safe_block` and is the older one
  (non-decreasing frame safe_blocks). The conclusion (closed wins, cascade
  covers the Tip) is correct; the stated reason is inverted — and this is the
  monotonicity argument F9's fourth coupling rests on, so it should be stated
  correctly.
- [x] **`run_flush_and_cascade` crash-window comment overclaims stability**
  ([recovery/mod.rs:352-357](../../sequencer/src/recovery/mod.rs)): "the danger
  condition that fired before still fires after the restart" is false when the
  frontier batch landed gold during the flush — the restart can resolve to
  `Proceed` with a no-op'd Pending batch left valid. That outcome is safe
  (clean resubmission at a fresh slot), but the invariant as stated is wrong;
  rewrite as a case analysis.
- [x] **Recovery README genesis-sentinel paragraph is stale**
  (`docs/recovery/README.md:46-48`): says the implementation "can handle the
  nonce-0 case either by submitting a sentinel batch... or by special-casing" —
  it already handles it structurally (`open_fresh_tip_in_tx` roots a nonce-0
  batch when the valid path is empty). Update; also relevant to PLAN.md's
  explicit-nonce-sentinel wrinkle (§ "Reconstructing the resume nonce").

## 4. Robustness / operational ledger

- [ ] **Submitter confirmation-timeout path defeats pacing** — watch timeout maps
  to `Ok` → `Submitted(n)` → immediate re-tick re-sends the same payloads at the
  same nonces (usually "replacement underpriced", logged at error) before
  sleeping ([poster.rs:129-139](../../sequencer/src/l1/submitter/poster.rs),
  [worker.rs:146-151](../../sequencer/src/l1/submitter/worker.rs)). Return a
  distinct outcome that sleeps.
- [ ] **SQLITE_BUSY treated as fatal in the submitter** — 50 ms read
  `busy_timeout` + every non-poster error fatal = a transient BUSY burns a full
  process respawn + recovery pass ([worker.rs:140-144](../../sequencer/src/l1/submitter/worker.rs),
  [open.rs](../../sequencer/src/storage/open.rs)). Retry BUSY or use the
  writer-grade timeout for these reads.
- [ ] **Own-sender SSZ decode failure wedges the submitter tick** — the poster
  hard-fails where the scheduler and the simulation skip-and-continue
  ([poster.rs:230-231](../../sequencer/src/l1/submitter/poster.rs)); an operator
  manual tx from the submitter EOA stalls submission for the safe-lag duration.
  Skip undecodable own-sender payloads.
- [x] **SIGTERM not handled — fixed by the runtime-authority cutover.** The OS
  signal future now races SIGINT and SIGTERM, and either enters the same
  cooperative concurrent drain. The remaining process-level SIGTERM→0 test is
  tracked separately in B4.
- [x] **`Workers::spawn` error path leaks running workers — structurally
  removed.** All fallible/awaited preparation, including HTTP bind and worker
  preflight, occurs while zero tasks exist. `AdmittedRuntime::launch` is
  infallible and non-yielding; the occupied-port regression test proves zero
  worker launches and process-lock release.
- [ ] **`Debug` derived on key-bearing config structs** — `L1Config` /
  `RunConfig` carry the submitter private key; one future `?config` log leaks
  it. Redacting newtype ([config.rs:18-25](../../sequencer/src/runtime/config.rs)).
- [ ] **Flusher spurious error logs** — see §3 (same fix lands together).
- [ ] **POST /tx 500 bodies echo raw internal error strings**
  ([inclusion_lane/mod.rs:435-441](../../sequencer/src/ingress/inclusion_lane/mod.rs));
  the storage-error path already uses a generic message. Align.
- [x] **Large safe-input backlog latency — accepted capacity assumption
  (2026-08-02)** — the frontier advance executes the whole newly-safe range
  before returning to user-op processing. The supported deployment assumes an
  application promptly digests the complete accumulated newly-safe range in
  the supported catch-up/backlog envelope; scratch paging bounds memory and
  read-query size, not the atomic drain or logical turn. We deliberately do not
  add an ack deadline, preemption, or durable timeout-and-resume state machine.
  Revisit if application cost, L1 capacity/finality/catch-up behavior, or
  measured acknowledgement disruption disproves that assumption. Owned by the
  authority-boundary ADR §4.
- [ ] **WS session teardown on transient read errors / silent idle on
  beyond-head cursors** — no close frame on BUSY; `from_offset` past head idles
  forever ([subscribe.rs](../../sequencer/src/egress/api/subscribe.rs)).

## 5. Hardening & hygiene backlog (vetted, lower priority)

- **Fee determinism contract under-specified** — `fixed_mul`'s comment claims a
  debug_assert that doesn't exist (silent truncation; currently unreachable —
  verified exhaustively up to `MAX_EXPONENT`), and bit-identical fees require
  pinning the *multiplication order* (LSB-first with floor-after-each-multiply),
  not just the table values ([fee.rs](../../sequencer-core/src/fee.rs)).
  **Directly load-bearing for PLAN.md §2** (native == RISC-V determinism, and
  any future C++ app implementing the same semantics).
- **`trg_enforce_nonce_contiguity` NULL hole** — a dangling
  `parent_batch_index` makes the subquery NULL and the comparison no-op; only
  the FK pragma (per-connection, default OFF) catches it. Make the trigger
  NULL-safe.
- **Direct-input double-sequencing structurally unguarded** — no uniqueness on
  `safe_input_index` among *valid* rows (re-drain support); the one storage bug
  class that double-executes a deposit rests on lane discipline alone, while
  AGENTS.md:212 claims "FK + PK constraints catch the dangerous failure modes".
  Tighten the doc at minimum.
- **`seal_and_open_next_batch` accepts an unchecked `next_safe_block`** — safe
  only because the lane passes `head.safe_block`; assert equality or drop the
  parameter ([ingress.rs:411-431](../../sequencer/src/storage/ingress.rs)). The
  bare `close_frame_and_batch` has no production callers — gate to `cfg(test)`.
- **`INSERT OR IGNORE` in `populate_safe_accepted_batches`** advances `expected`
  even when the insert no-ops — dead defense that would desync silently instead
  of failing loud; use plain `INSERT`.
- **`safe_accepted_batches.first_frame_safe_block` / `.inclusion_block` are
  write-only columns** — no production reader; drop or mark audit-only. (R2's
  hash-at-seal lives on the `batches` row, not here — this table stays a pure
  acceptance log either way.)
- **Scheduler-side `AppError` handling diverges from the lane** — canonical
  logs-and-continues (eprintln), sequencer fail-louds; a contract violation
  produces divergence rather than detection. Decide one behavior; on the
  canonical side an explicit report/halt beats a guest-console line.
- **`direct_q` unbounded in the canonical scheduler** — adversarial deposit
  flood bounded in time (force-drain) but not bytes; a per-input cap or byte
  budget closes a (very expensive) guest-OOM vector.
- **`MAX_BATCH_METADATA_BYTES` (71) understates real SSZ per-op overhead**
  (~83+ with offsets); byte budgeting undercounts ~15% for max-payload ops.
- **Wallet nonce `wrapping_add`** — signature replay reopens after 2^32 included
  ops per sender; economically implausible, but wrap-vs-saturate deserves a
  justifying comment.
- **`from_snapshot_bytes` rejects duplicates but accepts unsorted entries** —
  enforce strictly-ascending addresses (subsumes the duplicate check) or drop
  the pretense of canonical-decode enforcement.

**Storage deep-dive batch (2026-06-11, verified against the tree):**

- **Gate the test-only public surface** — all callers are test-side, but
  **correction (2026-06-11):** four candidates are used by integration tests
  (`sequencer/tests/`), which cannot see `#[cfg(test)]` items — use a
  `test-support` feature for `promote_finalized`, `initialize_open_state`,
  bare `close_frame_and_batch`, `safe_input_end_exclusive`; **delete**
  `ordered_l2_txs_for_batch` and `latest_batch_index` outright (only callers
  are their own unit tests). Deleting those two makes `l1_submission.rs`
  collapse to the submitter's two real reads — at which point rename it; its
  own header already admits "despite the historical name, nothing in this
  file does writes". Details: S1 in the simplification doc + the
  test-coverage map's churn section.
- **`frames` lacks the immutability triggers `batches` got** — `fee` and
  `safe_block` are documented immutable but only convention-protected;
  asymmetric with the schema's own trigger philosophy. Add write-once triggers
  or document the asymmetry.
- **Two corruption philosophies in one module** — `decode_l2_tx_row` panics on
  malformed rows while `convert.rs` deliberately saturates "so corrupted rows
  don't crash the process". Pick one (fail-loud is the repo's stated
  preference; the saturating converters silently launder corruption).
- **Duplicated 25-line CASE/LEFT-JOIN row shape** between
  `ordered_l2_txs_page_from` (egress) and `ordered_l2_txs_for_batch`
  (l1_submission) — deleting the unused latter removes the duplication for
  free.
- **`recover_post_flush_inner` / `recover_aging_tip_inner` share an identical
  clear+reopen tail** — one helper taking a pivot-selection closure keeps the
  clear/reopen conditions (F9's couplings) in exactly one place.
- **The scheduler-nonce fold exists twice** — `populate_safe_accepted_batches`'
  expected-nonce threading and the submitter's `decide_submit_start` fold are
  the same exact-match simulation; co-locating both next to
  `scheduler_accepts` in `sequencer-core/protocol.rs` would put all
  scheduler-mirroring logic in one audited place. (Also relevant to PLAN.md
  §2's scheduler-library extraction — this is the natural seed of it.)
- **Workers re-open `Storage` per tick** (reader, submitter inside
  `spawn_blocking`) — a held connection per worker drops per-tick open
  overhead and narrows error surfaces. Low priority.
- **`partition.rs` returns `Vec<ContractError>` both callers collapse to the
  first error**, and `should_retry_with_partition` substring-matches error
  codes against `format!("{err:?}")` instead of alloy's structured JSON-RPC
  code — fragile (false positives only cause harmless bisection, but still).

## 6. Verified-clean ledger (refuted concerns — do not re-litigate without new evidence)

- **`scheduler_accepts` omits the two structural rejections**
  (`SafeBlockAboveInclusionBlock`, `NonMonotonicSafeBlocks`) — deliberate,
  documented self-trust; unreachable from honest paths (monotone frame
  safe_blocks; inclusion ≥ observed safe head); test-pinned by
  `frontier_accepts_future_safe_block_batch_by_design`. Note: the failure shape
  on violation is silent permanent divergence, so if the trust boundary ever
  moves (Byzantine RPC in scope), revisit — the checks are pure and cheap to
  simulate. **Update (WP3, 2026-06-12):** the worst case is now covered — a
  sim-accepted batch that fails the content-identity check (foreign at the
  expected nonce) freezes the frontier with a `CanonicalDivergence` marker
  instead of silently desyncing (R2's bonus coverage; test
  `sim_accepted_foreign_batch_freezes_frontier_with_divergence_marker`).
- **Wire fee exponents above `MAX_EXPONENT` panic `fee_to_linear`** — honest
  sequencer clamps at persistence (test-pinned); only the trusted submitter key
  can deliver batches. Out of scope per self-trust.
- **Stalled WS consumer pins permit/thread/reader** — real mechanics, but DoS is
  explicitly infrastructure-scoped in the threat model.
- **`validate_user_op` panic via attacker-controlled frame fee** — same
  clamp + self-trust scoping as above.
- **Operator `WalletConfig` silently dead on warm start** — deliberate,
  documented dump-wins semantics; required for determinism.
- **Snapshot bytes history-dependent (explicit zero balances)** — deterministic
  product of the transition function; format guarantees map-content
  determinism, not logical-state canonicalization.

## 7. Suggested work packages (PR-shaped)

| # | Package | Contents | Depends on |
|---|---------|----------|-----------|
| WP1 | Durability | R3 decision (`synchronous=FULL` + measurement), F4 ordering note | — |
| WP2 | Flush anchor | R1a watermark (schema + poster write + flusher condition + assert), F2 coherence assert, TLA/README updates | WP1 |
| WP3 | Divergence detection | R2 content-identity check + `CanonicalDivergence` refuse arm + tests; threat-model + recovery doc updates | — |
| WP4 | Reader hardening | F5 (same-view bound + on-chain input-index reconciliation), F6 chain-id on fallback | — |
| WP5 | Feed protocol | F7 history-version / invalidation contract + README — **partial**: context fields, L1 provenance, and the durable history/canonical-offset foundation are landed; the public protocol remains owned by the [Track 3 ordered handoff](../plans/2026-07-track3-feed-replay-design.md#7-ordered-implementation-handoff) | — |
| WP6 | Recovery hygiene | F8 timestamp clamp, F9 scoped clear or pinned coupling + regression test | — |
| WP7 | Doc-drift pass | §3 ledger in one commit (no behavior changes) — **done 2026-06-11**, incl. AGENTS.md/CLAUDE.md restructure + `docs/invariants.md` | — |
| WP8 | Trait contract | F10 required methods (PLAN.md §7 pulled forward) | — |
| WP9 | Ops pile | §4 items, individually trivial | — |
| WP10 | Exit-code contract | R4 projection in the binaries + per-variant class test + ops doc (incl. SIGTERM→0 from §4) | WP3 for the `CanonicalDivergence` arm; rest independent |
| WP11 | Storage hygiene | §5 storage batch: cfg(test) gating + dead-read deletion, frames triggers, NULL-safe contiguity trigger, plain INSERT, scoped pending clear (F9), CHECK drop (F8) | — |

PLAN.md updates (Y detection caveat, best-effort flush framing, R2 as Y's
backstop, `--flush-through-nonce` option, fee-determinism note for §2) landed
with the PLAN.md review pass (commit `ea74449`).

The simplification/refactoring queue (S1–S7, the do-not-simplify list, and the
combined S∪WP ordering) lives in
[`2026-06-10-simplification.md`](2026-06-10-simplification.md).
