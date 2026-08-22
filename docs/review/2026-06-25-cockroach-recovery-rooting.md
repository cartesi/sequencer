# Settled design — Cockroach-recovery batch-tree rooting (2026-06-25)

PR5 (`setup --recovery`) rebuilds a wiped DB and must resume submitting at nonce
`N'` without replaying history, so the rebuilt batch tree is rooted at `N'`, not
0. `run`'s submitter submits `valid_closed_batches` with `nonce >= frontier`
(`= N'` after recovery), and a tip's nonce is structural (`parent.nonce + 1`),
so the tree **must** have a valid batch at `N'`. `trg_enforce_nonce_contiguity`
ABORTed any parentless root with `nonce != 0`, blocking this.

Settled with Gabriel (free rein on schema — no migration/back-compat concern),
after an adversarial design panel (run `wuiha00k9`).

## Decision: the batch-tree **anchor**, not a sealed sentinel batch

A `batch_tree_anchor` singleton holds the nonce the single parentless root
carries (default `0`; `setup --recovery` writes `N'` before the
`setup_complete` marker). This generalizes the rule the code already had —
`parent = None ⇒ nonce 0`, present in both `compute_next_nonce` and the trigger
— to `parent = None ⇒ nonce == anchor`. `run`'s first tip **is** the anchored
root; there is no sentinel batch row.

Schema delta (`0001_schema.sql`):
- New `batch_tree_anchor` singleton (default 0).
- `trg_enforce_nonce_contiguity` parentless arm: `nonce == (SELECT nonce FROM
  batch_tree_anchor)` — an **exact** match, *tighter* than the old "must be 0"
  (a buggy root at any wrong nonce, incl. 0 on a recovered deployment, ABORTs)
  — **plus** an at-most-one-**valid**-parentless-root guard scoped to
  `invalidated_at_ms IS NULL`.
- `trg_batch_tree_anchor_write_once`: the anchor is frozen once `setup_complete`
  exists (re-anchoring a live deployment would strand its spine).
- `compute_next_nonce(None)` reads the anchor.

Normal (anchor 0) deployments are **byte-identical** to pre-PR5. The invariant
is *strengthened*, not relaxed: see [I16](../invariants.md).

### Why the single-root guard is scoped to valid rows

A fully-torn cascade (every batch invalidated, e.g. the recovered root itself
goes bad) re-roots via the existing `open_fresh_tip_in_tx` `parent = None` path —
at the anchor `N'`. The old invalidated root is still a parentless *row*, so the
guard must count only `invalidated_at_ms IS NULL` roots, or the re-root would
ABORT. This mirrors the pre-existing "second parentless root on a fully-torn
cascade" shape (which the code already produced at nonce 0).

## Runner-up: sealed `N'-1` sentinel batch — rejected

Insert one sealed, payload-less batch at `N'-1` (the schema even anticipated it:
"`payload_hash` ... NULL ... on recovery sentinels"); `run`'s tip parents off it.

**Fatal, unguarded flaw** (panel finding, verified): the sentinel is a *valid
closed* batch at `N'-1`. `first_non_gold_closed_batch` selects any
`valid_closed_batches WHERE nonce >= frontier_nonce` as a cascade pivot. Nothing
in the schema stops a runtime cascade from selecting and invalidating the
sentinel; once gone, `run`'s tip re-roots parentless at 0 and the **unchanged**
contiguity trigger ABORTs all of recovery. Its safety rested entirely on "the
frontier never drops to `N'-1`" — an unenforced assumption, exactly the
"defense-in-depth against our own bugs" posture the codebase prefers (R5). It
also relaxes the trigger *more loosely* ("any sealed parentless nonzero") and
needs a new INSERT-already-sealed write path. The anchor has none of these.

## Two map findings that simplified the fill

- **The gold frontier auto-populates.** `populate_safe_accepted_batches`
  simulates scheduler acceptance over `safe_inputs` from genesis on *every* sync
  (cheap — nonce/staleness only, no app exec), so after recovery's sync-through-C
  `frontier_nonce` already equals `N'`. No synthetic frontier row, no FK-referent
  problem. ~~Recovery uses it as a fail-loud **cross-check**: `fold N' ==
  frontier_nonce` or abort.~~ **(Superseded — see the 2026-06-26 update and the
  Resolution below: recovery *defers* frontier population, so there is no
  cross-check; `N` is trusted metadata.)**
- **"Pre-burn" is harmless.** A batch's wire payload is frames (`user_ops` +
  `safe_block`) with no direct list; on-chain drains are driven by `safe_block`.
  So the locally-sequenced `≤ C` directs are pure local bookkeeping (cursor +
  catch-up skip) and never reach L1. `run`'s first batch (frame `safe_block ≥ C`)
  re-drains them on-chain exactly once, matching `S'`.

## Idempotency / re-run

`setup --recovery` is a **strict one-shot**: it refuses (terminal,
`SetupRecoveryError::AlreadySetUp`) on a DB that is already set up — the model is
"wipe the data dir and re-run". A crash *before* the `setup_complete` marker
leaves no marker and re-runs cleanly: `fill_recovery_state` is guarded by
finalized-snapshot existence and uses a unique-per-attempt dump dir. (This
resolves the PR3-review checkpoint-vs-marker note — chosen over the
persist-checkpoint-and-re-detect alternative.)

## Tests owed (beyond the landed unit tests)

The fold/fill components are unit-tested (anchor root/reject/single-root/freeze;
`fill_recovery_state` roots at `N'` + skips pre-executed directs + idempotent
re-run; exit-code mapping). Still owed: a storage-level **full-tear cascade on a
recovered (anchor = `N'`) tree** re-rooting at `N'`.

## Update (2026-06-26) — e2e landed, and it caught a real I15 bug

The affirmative **e2e recovery → run round-trip** (`setup_recovery_round_trip_test`,
rollups-e2e) now exists and **passes on the devnet** (39/39): build state →
promote a real finalized checkpoint → wipe → `setup --recovery` → `run` boots →
a post-recovery transfer at the continuing nonce is accepted (proving `S'`
preserved the sender's nonce + balance) → the rebuilt tree's anchoring at `N'`
is validated structurally. (Driving a *promoted* checkpoint on the devnet
needed a new `SEQ_MAX_BATCH_OPEN_SECONDS` knob — no prior test sealed + promoted
a batch.)

It caught a genuine bug the rooting panel's "the frontier auto-populates"
assumption missed: **the content-identity check (I15) self-diverged during
recovery.** The recovered tree has no local batches below `N'` (folded into
`S'`), so `populate_safe_accepted_batches` saw every L1 batch as "foreign" and
froze the frontier, poisoning the rebuilt DB. Fixed (commit a37708e) with the
**anchor-aware frontier**: the frontier begins at the anchor, so `< N'` landings
are trusted collapsed history (skipped by nonce-mismatch, never content-checked)
and `setup --recovery` defers frontier population to `run`'s first sync.
Adversarially reviewed (4 lenses, all "i15-preserved", no blockers); runtime
foreign/zombie detection is byte-identical for genesis/running deployments. See
[I15](../invariants.md) (anchor-aware-frontier note).

## Resolution — `N` is trusted; no recovery-time verifier

The recovery-time `fold N' == frontier_nonce` cross-check was dropped (the
frontier isn't populated during recovery) and is **not owed back**. Two reasons,
established by a follow-up review of the external feedback:

1. **The "content-free" replacement is circular.** Recomputing `N'` from the
   synced `(B, C]` batch nonces via `protocol::advance_expected_batch_nonce`
   seeds the fold from the very `N` it would check, so a wrong-high `N` baked
   into a quiet `(B, C]` re-derives itself and passes. The only check
   independent of `N` is a *from-genesis* scheduler-nonce replay — re-fetching
   all of L1 — which we deliberately do not do.
2. **A legitimate checkpoint can't be wrong.** A sequencer-produced finalized
   dump's `next_batch_nonce` is self-consistent by construction (closing nonce
   + 1, finalized only once promoted on L1), so `N` is trusted the same way `S`
   is. See [`cockroach.md`](../recovery/cockroach.md#data-dictionary).

Correction to the earlier note: "a wrong checkpoint nonce is still caught loudly
at `run`" was **too strong**. Only a wrong-**low** `N` is caught at `run` (the
rebuilt root collides with live L1 history → content-identity check). A
wrong-**high** `N` silently diverges if the checkpoint trust boundary is
violated. The dead `SetupRecoveryError::ResumeNonceMismatch` variant was
removed.
