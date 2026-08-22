# Simplification & Refactoring Queue — 2026-06 review

Companion to [`2026-06-10-correctness-review.md`](2026-06-10-correctness-review.md)
(the findings ledger). That document says what is *wrong*; this one says what is
*heavier than it needs to be*, ranked and sequenced. Sources: twelve module
reviews' refactor notes, the storage deep-dive, and the line-by-line passes.

## Verdict first

**No architectural restructure is needed.** The module layout (one file per
writer role, `*_in(tx)` free functions composing into larger transactions,
storage-owns-SQLite / lane-owns-filesystem) is sound and should be defended,
not redesigned. The "buckling under its own weight" feeling traces to three
specific, fixable weights:

1. **Invariants living between files with nothing pinning them** — addressed
   this review by [`docs/invariants.md`](../invariants.md) and the fail-loud
   policy; structurally dissolved over time as WP6/WP2/WP3 land.
2. **Test-only and dead surface presenting as production API** — a `Storage`
   facade ~30% wider than what production calls, with module docs describing
   consumers that don't exist. Pure deletion fixes this (S1).
3. **The same semantics written more than once** — the scheduler-acceptance
   fold in two places, a 25-line SQL row-shape in two places, the clear+reopen
   recovery tail in two places, the max-fee guard in two places. Each is a
   drift seed; each has a single-home fix (S2, S3, S6).

The one genuinely *new* abstraction worth building is the scheduler library
(PLAN.md §2) — everything below is pruning and consolidation that makes that
extraction smaller and safer.

## The queue (S-items, PR-shaped)

### S1 — Shrink the Storage surface to what production uses

All callers are test-side; zero production behavior change. **Correction
(2026-06-11, test-coverage map):** four of the candidates are used by
*integration tests* (`sequencer/tests/`), which compile as separate crates and
cannot see `#[cfg(test)]` items — plain gating would compile-break them.

- **Feature-gate** (e.g. a `test-support` cargo feature, or public seed
  helpers): `promote_finalized` (snapshot_endpoints.rs), `initialize_open_state`
  (3 integration files), bare `close_frame_and_batch`
  (batch_submitter_integration.rs), `safe_input_end_exclusive`
  (ws_broadcaster.rs). The lifecycle doc's "production must not call
  `promote_finalized`" landmine still becomes mechanically enforced — just by
  feature, not `cfg(test)`.
- **Delete** (verified: only callers are their own unit tests):
  `ordered_l2_txs_for_batch`, `latest_batch_index` — deleting the former also
  removes the duplicated 25-line CASE/LEFT-JOIN row shape (its twin in
  `egress.rs` becomes the only copy).
- Rename `l1_submission.rs` (whose own header admits "nothing in this file
  does writes") to match its real content — the submitter's two reads — and
  fix `storage/mod.rs`'s description of it.
- `seal_and_open_next_batch`: drop the `next_safe_block` parameter (callers
  always pass `head.safe_block`) or assert equality.
- `populate_safe_accepted_batches`: call `frontier_nonce` instead of
  re-deriving `latest.nonce + 1` inline.

### S2 — Collect the scheduler-mirroring logic in one place

The strategic item: it is PLAN.md §2's extraction, started small.

- Move the expected-nonce fold to `sequencer-core/protocol.rs` next to
  `scheduler_accepts`; make `populate_safe_accepted_batches` and
  `decide_submit_start` both consume it.
- Add the cross-referencing one-liners on both sides of the structural-reject
  omission (`scheduler_accepts` ↔ `batch_reject_reason_for_block`) so the
  asymmetry reads as intentional.
- Pin the fee-determinism contract in `fee.rs`: LSB-first binary
  exponentiation with floor-after-each-multiply (the two orders provably
  differ), and fix the `fixed_mul` comment that claims a nonexistent
  debug_assert.
- Later, with R2/WP3 landed: consider extracting a shared
  `batch_acceptance(batch, inclusion_block, expected) → Accept|Reject|Stale`
  consumed by *both* the canonical fold and the simulator, making structural
  drift impossible by construction. That is effectively PLAN PR2; don't do it
  piecemeal before then.

### S3 — One home for the recovery tail (land *before* WP6)

~~Extract `cascade_and_reopen(tx, pivot)`~~ **done (2026-06-11, with WP6 in
the same commit — the refactor existed to make the scoped clear a one-place
edit, and they landed together on the single-PR branch).** Both recovery
paths now share `cascade_and_reopen`; the pivot selection is the only
variation.

### S4 — Fail-loud batch (WP11 remainder; mandated by R5)

- `INSERT OR IGNORE` → plain `INSERT` in `populate_safe_accepted_batches`.
- NULL-safe `trg_enforce_nonce_contiguity` (missing-parent arm).
- Add the `frames` immutability triggers (`fee`, `safe_block`) — symmetric
  with `batches`, clearly allowed under R5.
- Decide the `convert.rs` philosophy: the saturating converters silently
  launder corrupt rows while `decode_l2_tx_row` panics; R5 says fail loud.
  Converting the saturations to checked-or-panic is a small, behavior-visible
  change (corrupt DB: silently-wrong → crash) — its own commit with tests.
- Drop or mark audit-only the write-only columns
  `safe_accepted_batches.{first_frame_safe_block, inclusion_block}`.

### S5 — Worker plumbing (bundle with WP9 ops items)

- Reader/submitter hold one `Storage` per worker instead of opening per tick.
- `partition.rs`: both callers collapse `Vec<ContractError>` to the first
  error — return one error; match the JSON-RPC error *code* structurally
  instead of substring-matching `format!("{err:?}")`.
- `runtime/workers.rs`: replace the string coupling between
  `WorkerExit::component_name()` and the components array with a structural
  key (a mismatch today means re-awaiting a consumed JoinHandle).
- Misclassified errors into `RunError::Io` (invalid key, provider build) →
  `BootstrapError` variants, so the R4 exit-code projection stays clean.
- Lane: collapse the overlapping `DrainSummary`/`ChunkOutcome` tri-states;
  precompute the byte target as an op count per head transition.
- Egress (declined-for-now items listed at the end stay declined).

### S6 — Canonical-app side polish (touch with extra care: it is the reference)

- Unify `force_execute_overdue` / `drain_directs_safe_at` into one
  `drain_while(predicate)` helper; pop-before-execute removes the payload
  clone.
- ~~Remove the duplicated in-frame `max_fee < fee_price` guard~~ **done
  (trait-pruning wave, 2026-06-11)** — the guard now lives only in the
  `validate_and_execute_user_op` free function (no longer an overridable
  trait default), which is always in the call path; duality tests pass.
- Drop the `ProcessResult`↔`ProcessOutcome` cross-`PartialEq` impls
  (test-only ergonomics; `result.outcome == X` is clearer).
- `to_user_op()` called twice per op in `execute_frame_user_ops` — call once.
- app-core: replace side-effecting match guards in the wallet with explicit
  decode-then-match; tidy `decode_portal_erc20_deposit`'s parameter.

### S7 — Fifteen-minute comment batch

One-liners that prevent future confusion, no behavior: `WriteHead.
max_batch_user_op_bytes` is resampled from *current* policy on restart;
`decide_submit_start` deliberately folds mined-but-unsafe nonces without
simulating staleness (transient submitter idle on a stale landing is expected);
the byte target can overshoot by one max-size op (and the schema CHECK only
bounds the target, not target + 1 op); the lane's "read together" comment on
head + drain cursor (two read txs — combine or soften); `executed_input_count`'s
real consumer is the snapshot byte comparison; `WalletConfig::devnet()` reusing
the Sepolia portal address is a deterministic-deploy assumption.

## Do-not-simplify list

Things that *look* like cleanup and would break registered invariants — the
refactorer-facing mirror of [`docs/invariants.md`](../invariants.md):

1. **Don't move filesystem work into `storage/snapshot_dumps.rs`** — the
   module boundary *is* the GC crash-ordering guarantee (I13).
2. **Don't reorder `check_danger`'s arms** or merge its two `find_*` helpers
   into one that consults the Tip first — the closed-frontier-first order
   keeps the dispatch table's meaning (since the F9 scoped clear it is no
   longer load-bearing for the pending clear; see I4's updated entry).
3. **Don't "deduplicate" promotion out of the drain transaction** — a
   standalone promotion re-opens the `lifecycle.md` §6 crash-loop wedge (I6).
4. **Don't filter own-batch rows out of `valid_sequenced_l2_txs`** (tempting
   to replace the three consumer-side sender checks with one view filter):
   the drain cursor is `MAX(safe_input_index)+1` over those very rows — the
   filter would rewind it and re-drain. Sender filtering must stay at the
   consumers (I11).
5. **Don't replace the rowid offset with count-based pagination** or add
   AUTOINCREMENT semantics assumptions — invalidated-batch holes and the
   0-sentinel both depend on current behavior (I10).
6. **Don't move GC back to the idle path or a dedicated worker** — promotion-
   coupled GC is starvation-proof and single-writer by design (lifecycle §7).
7. **Don't add internal retry loops to the flusher/submitter for provider
   errors** — the orchestrator respawn is the retry mechanism; internal
   retries would mask exactly the failures the danger machinery routes on.
8. **Don't unify the two staleness references** (inclusion vs current) — they
   are deliberately different formulas for different questions.

## Declined / deferred (with reasons)

- **Egress single-poller fan-out** (vs per-subscriber 20ms polling): correct
  scale-up path, no need at current subscriber counts. Revisit with load.
- **`LeaseGuard` shared release channel**: per-release writer connections are
  fine at current snapshot-polling traffic.
- **`finalized_state` ETag on `l2_tx_index`** instead of inclusion_block:
  structurally collision-proof but no reachable collision today.
- **Runtime exit-mapping generic** (the ~150 lines of five identical
  `From`/`from_shutdown`/waiter shapes): the module header explicitly defends
  the explicitness; the detector is genuinely special. Declined — fix only the
  string coupling (S5).
- **Stale-skip on-chain report**: would help forensics but is a scheduler
  protocol change; queue behind the scheduler-library extraction.
- **`Storage::read` commit-vs-rollback**: no behavioral difference; not worth
  churn.
- **TLA state-count freshness** (`157M` in the README): verify and update the
  number only alongside an actual TLC re-run; pin the TLC version then.

## Suggested combined order (S-items ∪ work packages)

1. **WP1** durability flip + benchmarks (one line; unblocks WP2)
2. **WP8** trait defaults required (tiny, independent — PLAN §7 pulled forward)
3. **S1** surface shrink (makes everything after smaller)
4. **S3** recovery-tail refactor → **WP6** scoped clear + CHECK drop on top
5. **WP2** wallet-nonce watermark (needs WP1)
6. **WP3** content-identity check + `CanonicalDivergence` (independent)
7. **S2** scheduler-mirror colocation (sets up PLAN PR2)
8. **S4** fail-loud batch (WP11)
9. **WP4** reader hardening, **WP5** feed generation id, **WP10** exit codes,
   **WP9 + S5** ops/plumbing bundle, **S6/S7** opportunistic
10. PLAN **PR1** (setup/run split) and **PR2** (scheduler library) on the
    cleaned base.
