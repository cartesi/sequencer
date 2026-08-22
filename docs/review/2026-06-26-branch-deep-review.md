# Deep branch review — setup/run split → cockroach recovery (2026-06-26)

A whole-branch review of the `feature/cockroach-recovery` stack (the setup/run
split, the scheduler-library extraction, the fold engine, and `setup
--recovery`). Two targets, in priority order: **(1) correctness** — design and
implementation bugs; **(2) simplification** — reduce weight via good
docs/specs and sharper abstractions, without an architectural restructure.

This ledger records outcomes. The rooting design is settled separately in
[`2026-06-25-cockroach-recovery-rooting.md`](2026-06-25-cockroach-recovery-rooting.md);
the standing simplification backlog is
[`2026-06-10-simplification.md`](2026-06-10-simplification.md).

## Target 1 — correctness: 4 findings, all fixed

All four landed in commit `79311d0`; revalidated on devnet (e2e 39/39, workspace
green, clippy/fmt clean).

| # | Sev | Finding | Fix |
|---|---|---|---|
| B1 | critical | `setup --recovery` partial-recovery re-run could silently re-anchor a tree whose durable root already carried a *different* `N'` (crash after the root tip, before `setup_complete`). | `open_tip_nonce()` re-entry guard in `fill_recovery_state`: a tip at a different nonce → fail-loud `SetupRecoveryError::PartialRecoveryMismatch` (exit 30). |
| B2 | high | A plain `setup` (genesis) run over recovery residue (anchor `≠ 0`) would root genesis at the recovery nonce. | Assert `anchor == 0` before `register_genesis_finalized_snapshot`; else `SetupRecoveryError::GenesisOverRecoveryResidue`. |
| B3 | medium | Review flagged the seed filter (`sender != submitter`) as a divergence bug. | **Re-examined → NON-bug**: the filter mirrors the scheduler's own sender-based classification (submitter inputs are batches, never directs-with-effect). Resolved by documenting the contract in `source_fold_inputs`, not by an over-firing assert. |
| B4 | medium | The fold's `(A,B]`/`(B,C]` disjointness assert used `>=`, admitting a replay input sharing the last seed block — looser than the disjoint-ranges docstring. | Tightened to `>` ("replay must start strictly after the last seeded direct"); added `replay_sharing_the_last_seed_block_panics`. |

## Target 2 — simplification: verdict and queue

**Verdict: no architectural restructure.** The branch's weight is best reduced
by naming contracts and consolidating duplicated semantics — consistent with the
standing simplification doc's verdict. The queue (P-items), and disposition:

| # | Item | Status |
|---|---|---|
| P1 | `docs/recovery/cockroach.md` — the recovery spec (flush → fold → fill, data dictionary, code map). | **Done** (`4e25a9d`). |
| P3 | Replace the `populate_frontier: bool` with a `FrontierMode` enum (`Populate` / `DeferUntilAnchorSet`), making the I15 deferral self-documenting at every call site. | **Done** (`8e7a134`). |
| P2 | Name the recovery checkpoint (`struct Checkpoint<A>` + `load`); extract `source_fold_inputs`; collapse `recover()` to six labelled steps. | **Done** (`dde8fa9`). |
| §2.2 | `docs/protocol/scheduler-semantics.md` — the authoritative I1 acceptance algorithm. | **Done** (`6006108`). |
| §2.3 | `docs/protocol/application-contract.md` — the authoritative `Application` FFI trait contract. | **Done** (`6006108`). |
| P4 | A `FoldInputSource` abstraction over seed/replay sourcing. | **Deferred — substantially addressed.** After P2 landed (`Checkpoint` + `source_fold_inputs`) and B4 tightened the boundary assert, the residual delta is a thin wrapper over a single call site. Adding it would *increase* surface for no readability gain — exactly the over-abstraction the simplification verdict warns against. Revisit only if a second fold-input source (e.g. a non-L1 checkpoint stream) ever appears. |

### Drift corrected alongside the specs (in `6006108`)

- Canonical-fold path `examples/canonical-app/src/scheduler/core.rs` →
  `sequencer-core/src/scheduler/mod.rs` (stale since the PR2 move), in AGENTS.md
  and invariants.md.
- invariants.md I1 cited a non-existent test
  (`frontier_accepts_future_safe_block_batch_by_design`) as pinning the
  `scheduler_accepts` structural-reject omission. The omission is sound by
  self-trust and is now documented in `scheduler-semantics.md`; the false
  citation was removed.
- The expected-nonce fold is no longer "slated for colocation": it is homed in
  `protocol.rs` as `advance_expected_batch_nonce` (consumed by
  `decide_submit_start`); `populate_safe_accepted_batches` keeps a deliberate,
  documented inline copy. AGENTS.md/invariants.md updated to match.

## Open / owed (non-blocking)

- **~~Content-free recovery-time nonce cross-check~~ — resolved: `N` is
  trusted.** Settled after the external-review follow-up: recovery does not
  re-verify `N`, and the dead `ResumeNonceMismatch` variant was removed. The
  once-proposed `advance_expected_batch_nonce` cross-check is circular (it seeds
  from the `N` it would check); only a from-genesis replay is independent, and
  that is deliberately not built. The earlier "a wrong checkpoint nonce is still
  caught loudly at `run`" was too strong — only wrong-low `N` is caught at run,
  wrong-high is not, which is acceptable because a sequencer-produced finalized
  dump cannot carry a wrong `N`. See the
  [rooting-doc Resolution](2026-06-25-cockroach-recovery-rooting.md#resolution--n-is-trusted-no-recovery-time-verifier)
  and [`cockroach.md`](../recovery/cockroach.md#data-dictionary).
- **Frontier-`nonce`/anchor asymmetry (external review)** — **fixed**:
  `frontier_nonce` now defaults to the batch-tree anchor (not 0) on an empty
  accepted table, so a recovered deployment's submitter starts at `N'` instead
  of re-submitting its first post-recovery batch each tick until the first
  accepted row lands. The anchor-read query is consolidated into one
  `&Connection` helper shared by `frontier_nonce`, `populate_safe_accepted_batches`,
  and `compute_next_nonce`.
- **Full-tear cascade on a recovered (anchor = `N'`) tree** — a storage-level
  test of re-rooting at `N'` after every batch is invalidated. Unit coverage
  exists for the anchor mechanics; this end-to-end shape is still owed.

## Follow-on multi-agent adversarial sweep (2026-06)

A whole-branch adversarial review (8 subsystem finders → 3 perspective-diverse
skeptics per finding: code-accuracy / reachability / already-mitigated).
**4 confirmed, 3 refuted.** All four fixed:

- **[P1, fixed] `setup --recovery` dropped deposits in the `(C, H1]` window.** The
  post-flush resync runs to the live safe head `H1` (normally `> C`), but the
  fold folds only `≤ C` into `S'`. `fill_recovery_state` used the generic
  `ensure_open_tip`, which drains the **whole** synced table — sequencing the
  `(C, H1]` directs as already-executed (cursor + snapshot `l2_tx_index` past
  them) while `S'` never executed them. They were then skipped by catch-up and
  never re-led → vanished locally while the scheduler drains them on-chain
  (divergence / lost deposit). A genuinely new class, distinct from the three
  prior re-entry-guard gaps; the round-trip e2e never injected a `(C, H1]`
  deposit. **Fix:** `open_recovery_tip` caps the drain at `C` (frame
  `safe_block = C`), leaving `(C, H1]` undrained so `run` leads + executes them
  once as the frontier advances `C → H1`. Test:
  `fill_recovery_state_leaves_post_c_directs_undrained`. cockroach.md steps
  3/6 + constraints updated.
- **[P2, fixed] Wrong-chain RPC during a startup-recovery sync looped instead of
  failing terminal.** The reader's `ChainIdMismatch` surfaced as
  `BootstrapError::Recovery(RecoveryError::InputReader(..))` → the `Recovery(_)`
  catch-all → exit 1 (unclassified) → restart loop on the wrong chain. **Fix:**
  explicit terminal arm in `bootstrap_exit_code`, mirroring the worker/boot
  arms; exit-code test added.
- **[P3, fixed] cockroach.md "Flush note" link** pointed at README.md; the
  section is in PLAN.md. Retargeted.
- **[P3, fixed] format.md Versioning section** still named `WalletSnapshotV1`
  current / `V2` hypothetical; this branch shipped `V2`. Updated (current = V2,
  example future = V3).

Refuted (correctly): the `(A, B]` seed-boundary "unstated dependency"
(self-conceded non-bug), the early-return F5-witness skip (premise impossible —
inputs cannot precede the InputBox deployment block), and an `assert_tree_invariants`
unit-test helper hard-coding `nonce 0` (a test-only coverage gap, not a runtime
bug — a fair minor cleanup if revisited).
