# Cockroach recovery (`setup --recovery`)

The catastrophe path. When the local DB is lost or has diverged
([`CanonicalDivergence`](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen)),
there is no batch tree to cascade — the operator **wipes the data dir and
rebuilds canonical logical state from a trusted checkpoint plus L1**. It is an
operator-driven, one-shot `setup` mode, not a runtime action. There is no
automated database replacement, clone detector, distributed fence, or
partial-fill resume state machine: the supported repair is deliberately the
explicit fresh/wiped-directory flow.

Contrast with **[standard / preemptive recovery](README.md)** (the rest of
`docs/recovery/`): that runs *inside* a live sequencer, uses its own batch tree
to cascade a doomed suffix, and shares the flush machinery. Cockroach recovery
discards the tree entirely and reconstructs `(S', N')` by *folding* L1, then
records the recovered application's absolute executed-input base `K`.

The pure fold engine ([`sequencer-core/src/scheduler/fold.rs`](../../sequencer-core/src/scheduler/fold.rs))
is the same scheduler source compiled into the on-chain canonical machine — so
the reconstruction is consistent with L1 *by construction*, not by a parallel
re-implementation.

---

## Data dictionary

The fold/fill quantities and history metadata below drive the procedure.
Knowing where each is *born* is the key to reading the code.

| Symbol | Meaning | Where it comes from |
|---|---|---|
| **`S`** | The trusted checkpoint machine/app state at block `B`. | Loaded from the dump: `A::from_dump(checkpoint_dump_dir)`. |
| **`A`** | `S`'s last-executed safe block. The fridge is reconstructed from directs in `(A, B]`. | App query: `S.last_executed_safe_block()`. (Persisted in the dump — see `docs/snapshots/format.md`.) |
| **`B`** | The checkpoint's L1 inclusion block. `S` reflects every **batch** with inclusion `≤ B` and **no direct** in `(A, B]`. | Operator arg: `--checkpoint-block`. |
| **`N`** | The checkpoint's resume batch nonce (the scheduler counter at `B`). The bare-metal app *cannot* recompute it, so it rides as checkpoint metadata. | `info.toml`'s `next_batch_nonce` in the dump. |
| **`C`** | The post-flush safe head — the stopping block. The flush resolves every slot the provider remembers at safe depth `≤ C` (best-effort — see step 2). | Return of `flusher.flush_and_wait(...)`. |
| **`N'`** | The resume nonce the new sequencer submits at — `N` advanced by the accepted batches in `(B, C]`. Becomes the **batch-tree anchor**. | Output of `fold_replay(...)`. |
| **`E`, `g`** | The new history era and its recovery generation. `E` is UUIDv4; `g = 0`. | Minted with the baseline schema in one transaction, before external recovery work. |
| **`K`** | The first application-history offset available in `E`: `S'.executed_input_count()`. It is not the replacement DB's physical replay cursor. | Derived after the fold and bound atomically with the initial finalized snapshot row. |
| **`F`** | The exclusive `safe_inputs` cursor already represented by `S'`; standard recovery must never drain below it. | Captured after the recovery root sequences its `≤ C` cursor padding and bound atomically with `K` and the initial finalized snapshot row. |

**Invariant `A < B`** (checked at load): the checkpoint's executed state must
predate its inclusion block, or the `(A, B]` fridge range is ill-defined.

`N` and the `(A, B]` batch content are **operator-trusted inputs** — there is no
cheap on-chain oracle to validate the checkpoint against, and recovery
does **not** re-verify them. The only sound independent check would replay the
scheduler's nonce fold from genesis through `C` (a from-genesis frontier is the
one quantity independent of the trusted `N`) — i.e. re-fetch and reprocess all of
L1, which recovery deliberately does not do.

This is sound because the checkpoint is a **sequencer-produced finalized dump**,
where the tuple is self-consistent by construction: `info.toml`'s
`next_batch_nonce` is the closing batch's nonce + 1, and a dump only reaches
*finalized* once that batch is promoted (observed accepted on L1), so `N` is
exactly the scheduler's position at `B`. A wrong `N` can therefore arise only
from a corrupted or externally-produced checkpoint — already outside the trust
boundary, the same one that accepts `S` with no verifier.

If that boundary is ever violated, the two wrong-`N` shapes are **not**
symmetric:
- a wrong-**low** `N` (or a mis-stated `(A, B]`) surfaces loudly at `run` — the
  rebuilt root collides with the still-live L1 batches in `[N', M)` and trips the
  content-identity check (I15);
- a wrong-**high** `N` is **not** caught — the `[M, N)` history was never reached
  on-chain, so nothing collides; the off-chain frontier accepts the local batch
  against itself while the real scheduler ignores it. This is why the checkpoint
  must be a trustworthy finalized dump, not merely "some app bytes".

This is a concrete boundary of the content-identity check: it is complete for at/above-anchor accepted
batch content identity, not checkpoint/application correctness or arbitrary
canonical divergence. Absence of `canonical_divergence` is not a proof that a
checkpoint outside the trust boundary was valid.

---

## The procedure: flush → fold → fill

Opening the fresh replacement DB first commits one baseline transaction: the
schema and a UUIDv4 era with generation zero. Because neither the folded
application nor the recovery-root
cursor exists yet, `base_executed_input_count` and `base_safe_input_index` start
NULL. That era remains externally unexposed until setup completes.

```
              ┌─ load S, derive A & N, require A < B
   checkpoint │
   (S @ B,  N)│   flush wallet nonce ───────────────► C  (post-flush safe head)
              │        │
              │        ▼
   L1 ────────┼──► re-sync safe_inputs through C   (frontier population OFF)
              │        │
              │        ▼
              │   source  seeds = (A,B] directs    (drop batches: in S)
              │           replay = (B,C] stream
              │        │
              │        ▼
              │   fold_replay(S, N, seeds, replay, C)  ──►  (S', N')
              │        │
              ▼        ▼
        fill: anchor = N', root tip @ N', finalized snapshot of S',
              replay cursor past the ≤C directs   ──►  run boots here
```

1. **Load `S`; derive `A`, `N`; require `A < B`.** Read the dump
   (`from_dump` + `info.toml`); `A = S.last_executed_safe_block()`.
2. **Flush → `C`.** Settle the wallet nonce (keyed L1 no-ops; this is where
   cockroach recovery composes with the standard flush). `C` is the stopping
   point: directs beyond `C` are `run`'s job, not the fold's.
   **This flush is best-effort by construction:** the wiped DB carries no
   wallet-nonce watermark, so the durable-anchor half of the completion test
   is vacuous — the flush resolves only the slots the provider remembers,
   and a zombie tx the local node forgot but the network still holds is
   unresolvable here (plain `setup`'s detection gate shares the same false
   negative). The content-identity check is what makes this acceptable: such
   a zombie landing at/above `N'` is detected and freezes the frontier
   instead of silently diverging, and the repair is another wipe-and-rerun —
   cockroach recovery recovers from the failure of its own flush. If ever
   needed, an operator-supplied flush floor taken from the old DB's
   watermark is a sound option: the value is fail-safe under corruption
   (too high wastes a few no-ops; too low degrades to exactly best-effort),
   so reading it from an untrusted half-destroyed DB does not violate the
   don't-trust-local-state premise.
3. **Re-sync `safe_inputs`; flush-view coherence.** The reader syncs to the *live* safe
   head `H1` (normally `> C` — real time passed while the flush awaited safe
   finality); refuse only if it *lags* `C` (a load-balanced RPC replica could
   serve a stale view). So `safe_inputs` ends up holding directs through `H1`,
   not just `C` — step 6 is careful to drain only the `≤ C` ones. **Gold
   frontier population is OFF for recovery's syncs** — the tree is empty until
   step 6, so populating the frontier would mark every L1 batch `Foreign` and
   falsely freeze it. The frontier is deferred to `run`'s first sync.
4. **Source the fold inputs.** Seeds = the `(A, B]` **directs** (drop
   `sender == batch_submitter` — those are batches, already in `S`); replay =
   the full `(B, C]` stream. The seed filter *is* the scheduler's own
   sender-based classification.
5. **Fold `(S, N)` → `(S', N')`.** The engine seeds the fridge from `(A, B]`,
   replays `(B, C]` (force-executing overdue directs, applying accepted
   batches, draining covered fridge directs), drains the leftover fridge at `C`,
   and advances the nonce to `N'`.
6. **Fill the DB.** Derive `K = S'.executed_input_count()`. **Anchor the batch tree
   at `N'`** (the root tip *is* `N'` — there is no sentinel batch); open the root
   tip at frame `safe_block = C` and sequence **only the `≤ C` safe inputs** so
   the replay cursor starts past them. The drain is sender-unfiltered — it
   includes the `≤ C` batch-submitter rows alongside the user directs the fold
   folded into `S'`, exactly as the genesis tip drains its whole span; those
   rows are sequenced (cursor padding), never executed, so the
   `sender != batch_submitter` *seed* filter does not reappear here. Capture the
   root's exclusive safe-input cursor as `F`, then snapshot `S'` as finalized at
   `C` and bind `(K, F)` in the same SQLite transaction; setup completion
   refuses until both bases and the finalized snapshot exist. The `(C, H1]`
   directs the resync pulled in past `C` stay **undrained** — `run`'s lane leads
   and executes them exactly once as the safe frontier advances `C → H1`.
   (Draining them here instead would skip them on catch-up while `S'` never
   executed them — a vanished deposit / divergence; this is why the fill uses a
   `≤ C`-capped `open_recovery_tip`, not the generic whole-table drain.) `run`
   boots from this state. Those padding rows advance physical `l2_tx_index`,
   but not `K` and receive no `executed_inputs` mapping: they are physical
   cursor attribution for inputs already reflected in `S'`, not newly executed
   application history. The first executable direct above `F` receives logical
   offset `K`; applying it moves the recovered application to `K + 1`.

---

## Code map

| Step | Code |
|---|---|
| entry / branch | [`setup()`](../../sequencer/src/commands/setup/mod.rs) branches on `config.recovery` after the shared prefix (identity pin + initial sync) |
| 1. load + `A < B` | [`recover()` step 1](../../sequencer/src/commands/setup/mod.rs) — `from_dump`, `read_info`, `CheckpointNotBeforeBlock` |
| 2. flush → `C` | `recover()` step 2 — `MempoolFlusher::flush_and_wait` (see [`recovery/flusher.rs`](../../sequencer/src/recovery/flusher.rs)) |
| 3. re-sync + coherence | `recover()` step 3 — `set_frontier_mode(DeferUntilAnchorSet)` + `sync_to_current_safe_head` + `ResyncBehindFlushView` |
| 4. source seeds/replay | `recover()` step 4 — `Storage::safe_inputs_in_block_range` + the `sender != submitter` filter + `to_fold_input` |
| 5. fold | [`fold_replay`](../../sequencer-core/src/scheduler/fold.rs) |
| baseline history | [`baseline_migration`](../../sequencer/src/storage/open.rs) — UUIDv4 era + generation zero + NULL rebuild base in one baseline transaction |
| 6. fill | [`fill_recovery_state`](../../sequencer/src/commands/setup/fill.rs) — anchor + `open_recovery_tip` (`≤ C`-capped drain at frame `safe_block = C`) + atomic finalized-snapshot/`K` bind |
| anchor mechanism | [`trg_enforce_nonce_contiguity`](../../sequencer/src/storage/migrations/0001_schema.sql) + `compute_next_nonce` + the anchor-aware frontier in [`safe_accepted_batches.rs`](../../sequencer/src/storage/safe_accepted_batches.rs) |

---

## Load-bearing constraints & invariants

- **`A < B`** — checked at load ([`SetupRecoveryError::CheckpointNotBeforeBlock`]).
- **Disjoint fold ranges** — seeds `(A, B]`, replay `(B, C]`, strictly disjoint;
  the fold's always-on asserts enforce ascending order *within* each and a
  strict block boundary *between* (directs at block `B` are seeds; the replay
  starts strictly after).
- **Frontier deferral** — recovery's syncs never populate the gold frontier (the
  tree is empty); `run`'s first sync populates it once `anchor = N'` is set, so
  the folded `< N'` batches are skipped as trusted collapsed history rather than
  flagged `Foreign`. See the anchor-aware-frontier note on
  [I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen).
- **Anchored root** — the rebuilt tree has exactly one valid parentless root,
  carrying `N'` ([I16](../invariants.md#i16-the-batch-tree-has-exactly-one-valid-parentless-root-carrying-the-deployments-anchor-nonce)).
- **No double-execution, no lost directs** — the `≤ C` directs are sequenced
  (cursor advances) but the finalized snapshot's `l2_tx_index` is set *after*
  sequencing, so `run`'s catch-up (`offset > l2_tx_index`) skips them; they are
  already in `S'`. Symmetrically, the `(C, H1]` directs are **not** sequenced
  here (`open_recovery_tip` caps the drain at `C`), so the cursor sits below them
   and `run`'s lane leads + executes them exactly once — neither double-executed
   nor lost. The durable `base_safe_input_index = F` also survives invalidation
   of the recovery root: subsequent standard recovery derives its drain cursor
   as `max(F, max valid attribution + 1)` and cannot re-sequence or re-execute
   the `≤ C` prefix after the root's padding leaves the valid view.
- **History base is application state, not cursor padding** — rebuild baseline
  leaves `(K, F)` NULL; fill derives `K` from `S'.executed_input_count()` and
  `F` from the root's exclusive safe-input cursor, then binds both with the
  initial finalized snapshot. Setup requires all three before completion. Physical
  `l2_tx_index` remains a rowid replay cursor and may be greater because it also
  covers sequenced-but-not-executed batch-envelope padding. Those padding rows
  are deliberately absent from the canonical mapping; new executable history
  is attributed contiguously from `K`.

---

## Crash-safety & idempotency

`setup --recovery` is a **strict one-shot** on a freshly-wiped DB:

- It **refuses** (terminal, exit 30) if `setup_complete` already exists — the
  model is "delete the data dir and re-run", not resume-a-live-deployment.
- The `setup_complete` marker is the linearization point, written **last**; it
  requires non-NULL `K`, non-NULL `F`, and the finalized snapshot.
- A crash *before* the marker is handled fail-loud, not by blind resume:
  - Retaining an incomplete DB retains the UUIDv4 era minted by its baseline
    transaction. An early retry therefore reuses that still-unexposed era; it
    does not rotate merely because the command restarted.
  - A **completed** fill (finalized snapshot present — the last write) re-runs as
    a safe no-op once the root Tip's `N'` matches. Its atomically bound
    finalized snapshot/`(K, F)` tuple is authoritative; retry never compares
    that stored base with a later fold at a newer `C`.
  - A **same-`N'`** re-run of a fill that crashed *mid-fill* (root tip exists, no
    finalized snapshot) is **refused** (`PartialRecoveryIncomplete`). It is *not*
    idempotent: a re-sync may have advanced `C` with new directs (which leave
    `N'` unchanged) that resuming would leave unsequenced, leaving the snapshot
    cursor behind `S'` and double-draining them on `run`. Wipe and re-run.
  - A **different-`N'`** re-run (a different checkpoint, or the same one after
    `C` advanced with new accepted *batches*) is **refused**
    (`PartialRecoveryMismatch`): the durable root tip carries the old nonce and
    cannot be silently re-anchored. Wipe and re-run.
  - `setup --recovery` over **foreign residue** — a finalized snapshot with no
    root tip, left by a crashed plain `setup` (which writes the genesis snapshot
    before its marker) — is **refused** (`RecoveryOverResidualSnapshot`). A
    completed cockroach fill always has both a snapshot and a root tip; keeping
    the old snapshot would mark setup complete over genesis instead of `(S', N')`.
    Wipe and re-run.
  - A subsequent **plain `setup`** over recovery residue is **refused**
    (`GenesisOverRecoveryResidue`, anchor `≠ 0`) — it must not root genesis at
    the recovery nonce.

Any fail-loud partial-fill refusal requires the explicit operator wipe/retry.
That fresh baseline necessarily mints another era, while the discarded era was
never exposed by a completed setup. This narrow completed-fill no-op is not a
general resumable rebuild protocol.

(See the deep-review remediations: these guards close the partial-recovery
revalidation gap, including the same-`N'`/advanced-`C` double-drain — external
review 2026-06.)
