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
  boundaries are *encouraged*: a loud crash is recoverable by design
  (orchestrator respawn + startup recovery), while a silently-tolerated bug
  that externalizes (a signed batch, an ack, a feed event) is state divergence
  — theft-equivalent and unrecoverable at runtime.
- **Never handle gracefully what cannot happen.** No fallback branches, no
  re-deriving a neighbor's answer to double-check it, no `Option`-handling for
  can't-be-`None`. One contract, one source of truth, no second code path.
- **Never absorb silently.** No `INSERT OR IGNORE`, saturating decode, or
  `unwrap_or_default` on data the contracts make impossible; use the loud
  variant of the same operation.
- An assertion must check a **real invariant** — true in every legitimate
  execution, including crash-recovery, replays, and clock steps — never an
  environmental assumption. (Cautionary tale: `sealed_at_ms >= created_at_ms`
  was CHECK-enforced, wall-clock regression is legitimate, and the constraint
  wedged recovery — review F8.)

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
  the omission is documented in `scheduler-semantics.md`, not currently
  test-pinned.)
- **Enforced by:** review + tests only. No mechanism.
- **Depended on by:** everything — the gold frontier, recovery's cascade pivot,
  promotion, soft-confirmation honesty.
- **Breaks:** silent permanent scheduler/sequencer divergence.
- The expected-nonce fold is homed next to `scheduler_accepts` as
  `advance_expected_batch_nonce`; `decide_submit_start` consumes it, while
  `populate_safe_accepted_batches` keeps a deliberate inline copy (its advance
  interleaves with storage-only side effects that can't move below the protocol
  layer — see the call-site comment).

### I2. Drain attribution: drained directs land in the new frame

- **Holds:** at a safe-frontier advance, the newly-drained directs are
  sequenced into the **new** frame, which is stamped with the **new**
  `safe_block` (`close_frame_in`, `storage/ingress.rs`). Frame K's wire content
  is therefore "directs ≤ S_K, then ops validated on top".
- **Enforced by:** `close_frame_in` ordering; lane convention.
- **Depended on by:** the duality (scheduler's drain-before-ops equals the
  flattened replay order); catch-up; the feed.
- **Breaks:** ops validated against a state the scheduler won't reproduce —
  divergence.

### I3. Frame `safe_block`s are non-decreasing along the spine

- **Holds:** every frame opens at the current safe frontier, which only
  advances.
- **Enforced by:** lane flow + `append_safe_inputs`' monotonicity asserts
  (`storage/l1_inputs.rs`).
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
  skip the flush *because* nothing closed is doomed). **No longer load-bearing
  for the pending clear**: since the F9 fix (2026-06-11) the clear is scoped
  to `nonce >= pivot.nonce` in `cascade_and_reopen`, so a valid in-flight
  closed batch's pending survives any cascade by construction, regardless of
  arm order.
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
  (`close_frame_only_promoting`).
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
- **Enforced by:** `Workers::spawn` order (`ensure_finalized_snapshot`,
  `ensure_open_tip`) + recovery's in-tx reopen.
- **Depended on by:** catch-up's unconditional load path
  (`CatchUpError::NoSnapshot` is fail-loud, not a branch); the lane's
  `NoOpenTip` fail-loud load.
- **Breaks:** startup crash (loud — by design).

### I9. Acceptance identity: "accepted nonce N" means "our valid batch N"

- **Holds:** by nonce **and content** since WP3 (review R2, 2026-06-12): every
  fully-accepted landing is compared against the local valid closed batch at
  that nonce — `keccak256(landed bytes)` vs the hash stamped at seal by the
  same encode path the submitter broadcasts.
- **Enforced by:** prevention — the flush resolving every wallet-nonce slot
  before a cascade reuses a nonce, anchored by the persisted watermark (I14);
  detection — the content-identity check in
  `populate_safe_accepted_batches`, which on violation persists the
  `canonical_divergence` marker and freezes the frontier (I15).
- **Depended on by:** the gold frontier, cascade pivot selection, promotion,
  local-state ↔ canonical-state agreement.
- **Breaks:** was silent divergence (review F1's zombie, F3's power-loss
  re-seal); now a detected `CanonicalDivergence` refusal whose remedy is
  cockroach recovery.

### I10. Replay-offset sentinel: `0` means "from genesis"

- **Holds:** `valid_ordered_l2_tx_head` returns 0 on an empty stream, and
  catch-up pages with `offset > cursor` — sound because `sequenced_l2_txs`
  rowids start at 1 and rows are never deleted (invalidated rows are filtered,
  not removed), so offsets are globally increasing and 0 is never a real
  offset.
- **Enforced by:** SQLite rowid semantics + append-only convention.
- **Depended on by:** catch-up, the feed cursor, snapshot `l2_tx_index`.
- **Breaks:** first transaction skipped or double-applied on replay.

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
  (SQLite-only) and the lane's FS half (`inclusion_lane/snapshot.rs`) — the
  module boundary *is* the ordering guarantee.
- **Depended on by:** `from_dump` at catch-up; the serving endpoints.
- **Breaks:** resume crash-loop. (Power-loss caveat until review R3 lands:
  a non-fsynced row delete can rewind past a completed unlink — review F4.)

### I14. Watermark ≥ wallet nonce of every tx ever broadcast

- **Holds:** since WP2 (review R1a, 2026-06-11) — the watermark commits
  durably (`synchronous=FULL`) before any broadcast at a new nonce,
  uniformly for batch txs and flush no-ops.
- **Enforced by:** write-before-broadcast — `EthereumBatchPoster::submit_batches`
  raises through `WalletNonceWatermarkSink` before its first send;
  `MempoolFlusher::flush_and_wait` likewise before its no-ops, and refuses to
  complete until `safe >= watermark + 1`.
- **Depended on by:** flush completeness, TLA+ Implementation Constraint 1,
  cascade soundness (I9).
- **Breaks:** zombie txs evade the flush — review F1.

### I15. Divergence marker present ⇒ acceptance frontier frozen

- **Holds:** since WP3 (review R2, 2026-06-12). A fully-accepted landing that
  fails the content-identity check writes the `canonical_divergence`
  singleton **in the same transaction** as the sync that detected it, and
  `populate_safe_accepted_batches` returns early whenever the marker exists —
  so no acceptance row, no promotion, and no gold-frontier advance can ever
  happen past a detected divergence.
- **Enforced by:** the marker guard at the top of
  `populate_safe_accepted_batches` + `check_danger`'s first arm
  (`CanonicalDivergence`, ranked ahead of every other arm) + the
  `Refuse(CanonicalDivergence)` startup dispatch.
- **Depended on by:** standard recovery never running on a diverged frontier
  (a flush+cascade there would compound the divergence); the lane never
  promoting a diverged landing; the remedy being cockroach recovery only.
- **Breaks:** silent permanent scheduler/sequencer divergence — the
  theft-equivalent failure the whole review centered on (F1/F3 residuals).
- **Anchor-aware frontier (PR5, 2026-06-26):** the content-identity check fires
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

- **Holds:** since PR5 (cockroach recovery, 2026-06-25). Every batch's nonce is
  `parent.nonce + 1`, except the single parentless root, which carries the
  `batch_tree_anchor` nonce — `0` for a genesis deployment, `N'` for a
  cockroach-recovered one (`setup --recovery` writes the anchor before the
  `setup_complete` marker). `run`'s first tip *is* that root (there is no
  separate sentinel batch). A fully-torn cascade re-roots parentless at the
  same anchor via `open_fresh_tip_in_tx`'s `parent = None` path, after
  invalidating the old root — so only one *valid* parentless root ever exists,
  invalidated ones coexisting.
- **Enforced by:** `trg_enforce_nonce_contiguity` — its parentless arm is an
  *exact* match `nonce == (SELECT nonce FROM batch_tree_anchor)` (tighter than
  the pre-PR5 "must be 0"), plus an at-most-one-valid-parentless-root guard
  scoped to `invalidated_at_ms IS NULL`; `compute_next_nonce(None)` reads the
  same anchor; `trg_batch_tree_anchor_write_once` freezes the anchor once
  `setup_complete` exists. Normal (anchor 0) deployments are byte-identical to
  pre-PR5.
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
