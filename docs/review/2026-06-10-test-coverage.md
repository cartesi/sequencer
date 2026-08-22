# Test Coverage Map — 2026-06 review

Third companion to [`2026-06-10-correctness-review.md`](2026-06-10-correctness-review.md)
(findings) and [`2026-06-10-simplification.md`](2026-06-10-simplification.md)
(refactor queue). This document answers: what does the suite pin, what does it
fail to pin, which tests will go red *by design* as the work packages land, and
which test-infrastructure levers exist or are missing.

## Verdict

**Recovery is the best-tested subsystem in the repo** — the startup dispatch
matrix is covered at both unit and e2e level (all five danger arms, threshold
boundaries on both sides, multi-generation cascades, nonce reuse across
generations, torn states, restart-loop convergence via `respawn_until_stable`,
and a post-test schema-invariant hook on every passing e2e scenario). The e2e
harness is also substantially stronger than its absence of documentation
suggests: libfaketime with *mid-run* wall-clock jumps, real process
spawn/respawn loops, TCP-proxy outage injection, Anvil mempool control
(`set_automine(false)` + `drop_all_pending_txs` — the delayed-inclusion
recipe), and DB surgery helpers.

**The one structural hole is the one the review predicted: the duality has no
direct mechanism.** Both sides are strongly tested *separately* — the canonical
side against the real RISC-V machine, the sequencer side against hand-written
expectations — but no test anywhere feeds **sequencer-produced batch bytes**
through **any** scheduler implementation and compares end states. Agreement is
transitive through two sets of hand-built fixtures. Details below; the fix is
two cheap tests (A1, A2).

## The duality scoreboard

Per AGENTS.md Testing Guidance ("any invariant the two sides share should have
at least one test that exercises both"):

**Two-sided today:**
- EIP-712 domain: `v1_regression_*` (host) ↔ `scheduler_emits_transfer_notice_from_guest` (guest signature recovery).
- Nonce exact-equality / reject-without-consume: guest `scheduler_rejected_batch_does_not_consume_nonce`, `scheduler_reports_wrong_nonce_batch_from_guest` ↔ host `advance_expected_batch_nonce_matches_scheduler_nonce_rule`, `populate_safe_accepted_batches_handles_large_nonce_gap` / `_out_of_order`.
- Staleness skip without nonce consumption: guest `scheduler_stale_batch_is_skipped_without_consuming_nonce` ↔ host `safe_accepted_frontier_skips_stale_payloads`.
- The structural-reject **asymmetry**, pinned on both sides: guest rejects (`scheduler_reports_non_monotonic_safe_blocks` / `_safe_block_above_inclusion`) ↔ host deliberately accepts (`frontier_accepts_future_safe_block_batch_by_design`).
- A partial end-to-end round trip exists: `nonce_zero_recovery_invalidates_then_accepts_at_nonce_zero_test` lands a *sequencer-produced* recovery batch in `safe_accepted_batches` — sequencer bytes through the **simulation**, not through the scheduler.

**One-sided or absent:**
- **Sequencer-produced bytes through a real scheduler implementation: absent** (the most direct I1 test).
- **Guest fee path: never exercised** — every canonical-test batch uses `fee_price = 0`, `max_fee = 0`. The `max_fee < fee_price` skip and gas charging never run on the guest.
- **Host==guest fee arithmetic: no cross-check** — canonical-test never asserts post-gas balances; a `fee_to_linear` divergence on the guest would be silent state divergence (and review §5 pins this as load-bearing for PLAN §2).
- Drain attribution / flattened replay order on the *same* batch bytes: each side separately only.
- The native `Scheduler<A>` fold is used by **zero tests outside canonical-app's own suite**.

## Owed tests (the gap ledger)

### A — writable today, no new infrastructure

- [ ] **A1. Native duality agreement test** (the I1 mechanism): run the
  sequencer (or seed storage directly), take its actual `pending_batches` SSZ
  bytes + the same direct-input stream, fold them through the native
  `Scheduler<WalletApp>`, and compare final balances/nonces against the
  sequencer's app state (snapshot or replayed feed). Closes "enforced by
  review + tests only. No mechanism."
- [ ] **A2. Guest fee-path test** in canonical-test: nonzero `fee_price`,
  user ops above/below it, **assert post-gas balances**. Gates S6's removal of
  the duplicated max-fee guard ("verify with the duality tests" — that test
  doesn't exist yet) and gives the first host==guest fee-arithmetic check.
- [ ] **A3. I4 arm-ordering dedicated test**: both `ClosedBatchInDanger` and
  `TipInDanger` genuinely in danger, assert Closed wins. Today the ordering is
  pinned only *incidentally* (a fixture whose Tip happens to be equally old).
- [ ] **A4. F9 coupling regression pair** (owed by WP6): (a) a gold-accepted-
  during-startup-sync but never-promoted pending survives recovery / is
  handled; (b) the promote-after-clear wedge state is unrepresentable.
- [ ] **A5. I2's second half**: `close_frame_only` at an *advanced* safe head
  — assert the new frame row and the encoded wire frame carry the NEW
  safe_block together with the drained directs. (Membership is pinned;
  the stamp is not.)
- [ ] **A6. Fail-loud halves**: `CatchUpError::NoSnapshot` (no test references
  it), `InclusionLaneError::NoOpenTip`, I7's tx-failure half (recipe:
  pre-insert a dumps row with a colliding prefix so the seal tx fails on
  UNIQUE → batch must stay the open Tip), F4's dangling-row *state*
  (delete the directory under a referenced row → assert the loud failure
  shape; the WAL-rewind cause stays unsimulable).
- [ ] **A7. Wallet pins before S6**: insufficient-balance transfer/withdrawal
  silent no-op (gas charged, nonce bumped, no output — the side-effecting
  match-guard arm S6 refactors, currently unpinned), the
  `AppError::Internal` cannot-pay-gas arm, and a wallet-level
  replay-determinism test (execute → dump → replay same sequence into fresh
  app → state equality).
- [ ] **A8. EstimatedBatchInDanger e2e** — the one dispatch-matrix row without
  an e2e. Recipe (verified writable): mine ~800 blocks (observed age below
  the 900 stale gate), `set_faketime_offset(+30min)` without mining, respawn
  → assert refusal + `invalidated == 0`.
- [ ] **A9. PLAN Q1/Q2 unit tests** against the existing `Scheduler<A>`
  (construct the fridge at B, feed (B,C], assert N and the drain set) — no Y
  CLI needed; the e2e variants wait for PLAN PR4/PR5.
- [ ] **A10. Fourth-shape cascade policy pin**: a *young* never-submitted
  closed batch is cascaded anyway (convergence-over-preservation — now a
  normative README claim with no test).
- [ ] **A11. `recover_aging_tip` torn/no-Tip entry** (only the post-flush
  variant is tested) — protects the S3 unification from dropping the reopen
  on one path.
- [ ] **A12. F8 reproduction/fix pin**: backward-stepped clock + `seal_batch`
  / cascade against the cross-column CHECKs (today: reproducible, unwitnessed;
  after WP6 drops them: "cascade succeeds with backward clock" is the pin).

### B — originally blocked on the feature (land with the WP)

- [x] **B1 (WP2)**: watermark positives — flush submits no-ops covering
  `[latest, watermark+1)` even when `pending == safe`; post-flush
  `safe > watermark` assert. Landed as
  `flush_covers_watermark_slots_the_pool_forgot`.
- [ ] **B2 (WP3)**: storage/reducer coverage has landed: content mismatch and
  a foreign batch at the expected nonce persist `CanonicalDivergence`, freeze
  the frontier, and rank Refuse ahead of every reducer arm. The requested
  process-level `respawn_until_stable` scenario remains open.
- [ ] **B3 (WP5)**: the new feed contract (`HistoryVersion` claim /
  invalidation signal) — note the current suite *pins the divergent behavior*
  (see churn).
- [ ] **B4 (WP10)**: per-variant exit-code assertions — current failure-path
  E2Es that inspect exit status assert only `!success()`;
  `ExitStatus::code()` is already available from `wait_for_exit`. SIGTERM
  handling and unit-level exit projection are landed; a process-level
  SIGTERM→0 assertion and exact per-variant e2e exit codes remain open.
- [x] **B5 (WP4)**: F6 wrong-chain detection is pinned at both the reader and
  run-bootstrap boundaries; F5 input-index reconciliation has explicit gap,
  truncated-tail, and topic/payload mismatch refusal tests.

### C — blocked on a missing harness lever (build the lever first)

| Lever | Unblocks | Effort |
|---|---|---|
| Pending-tx capture + re-inject (`txpool_content`/raw-tx before `drop_all_pending_txs`, `eth_sendRawTransaction` later) | **F1 zombie e2e** — the review's headline scenario; WP2's adversarial case | small |
| Snapshot observability (DB readers for `pending_snapshots`/`finalized_snapshot`/`dumps`, `/latest_snapshot` client, dump-dir inspection) | A4/F9 e2e, lifecycle e2e (currently **zero** e2e coverage of take/promote/GC/leases; warm-resume-from-dump is exercised by every restart test but never asserted — a silent fallback to genesis replay would pass everything, just slower) | small |
| Bare second-Anvil spawn (`--chain-id <other>`) + `set_l1_endpoint_override` mid-run | F6 e2e | small |
| Kill-at-log-marker (deterministic crash between flush completion and cascade commit) | the crash-window case analysis in `run_flush_and_cascade` (the Proceed-with-noop'd-Pending branch has no witness) | medium |
| Split-view / response-rewriting L7 proxy (TcpProxy is byte-level) | F2 coherence e2e, F5 clamped-`get_logs` e2e — **realistically: unit-level provider mocks instead**, e2e validates only the passing path | medium–large |
| SQLITE_BUSY injection | §4 BUSY items (submitter fatal, WS teardown) | medium |
| fsync/power-loss/WAL-rewind fault injection | true F3/F4 reproductions | **out of scope** — accept; the state-construction variants (A6) cover the detectable halves |

### D — explicitly accepted as untested

Power-loss WAL rewind (above); the flusher's spurious retry log (lands
test-free with WP9); detector L1ViewStale-arm runtime exit (single code path
treats all non-Safe alike; a log-field assertion would do if wanted).

## Churn forecast (consult before each WP — reds listed here are by design)

- **WP2 (watermark)**: `flush_is_noop_when_no_pending_nonces` **pins the F1
  hole as correct behavior** — red by design. The other flusher tests are
  signature churn. E2e: `delayed_inclusion_cascades_on_restart_test` and
  `nonce_zero_recovery_…` rely on the early-return for fast recovery boots —
  the widened flush adds no-ops + safe-wait; expect timeout tuning (and the
  harness may need a block ticker during recovery boots). The three
  `respawn_until_stable` convergence tests get timing exposure (flaky-red
  risk, not hard-red).
- **WP3 (content check)**: a *family* of fixtures fabricates accepted
  own-sender payloads that don't content-match local batches
  (`submitter_frontier_tracks_accepted_prefix`,
  `populate_safe_accepted_batches_resumes_from_latest_row`,
  `frontier_accepts_future_safe_block_batch_by_design`, two `check_danger_*`
  tests, four `tick_once_*` worker tests) — these will trip
  `CanonicalDivergence` by design; fixtures must seed payloads through the
  real encode path or seed matching closed batches. The stale/duplicate/gap
  tests **survive** (the check gates on full acceptance). Also:
  `check_danger_refuses_when_l1_view_is_stale`'s "first arm" claim becomes
  second place (marker outranks it).
- **WP5 (feed contract)**: `ws_reconnect_at_invalidated_offset_skips_cleanly_test`
  and `ws_subscribe_from_future_offset_waits_silently_test` pin exactly the
  behavior being replaced — red by design. If the envelope changes, every e2e
  fails at parse until `WsClient`/`ReplayWalletApp` update (mechanical).
  First-frame assertions in `ws_broadcaster.rs` are conditional churn.
- **WP6 (scoped clear)**: `aging_tip_cascade_clears_pending_dumps` red by
  design (scoped clear deletes nothing on the RecoverTip path);
  `post_flush_cascade_clears_pending_dumps` passes by coincidence (pivot
  nonce 0 ⇒ scoped == blanket) — **must gain the discriminating case**;
  `clear_pending_removes_all_pending_rows` + a lease-test staging call need
  mechanical updates. F8's CHECK drop is test-silent (nothing pins the
  CHECKs).
- **WP8 (required trait methods)**: `SweepTestApp` (3 workers.rs tests) and
  any stub relying on the defaults — compile-break; add the two explicit
  methods.
- **S1 (surface shrink)**: see the correction below — the integration-test
  seeds compile-break if gated as originally written.
- **S3 (recovery-tail extraction)**: **anti-churn** — the entire
  `recover_post_flush` / `tip_staleness` / pending-clear suites must stay
  green; any red there means the refactor changed behavior.
- **S5 (error taxonomy)**: `invalid_private_key_error_does_not_echo_key_material`
  retargets variant, keeps the no-echo assertion.
- **S6 (wallet match guards)**: claimed behavior-neutral but the
  insufficient-balance no-op arm has **no pin** — land A7 first.
- **PLAN PR1 (setup/run split)**: `first_boot_no_identity_l1_unreachable_…`
  and `chain_id_mismatch_via_live_rpc_…` test bootstrap paths that move into
  `setup`; rewrite as setup-phase tests; harness spawn flow changes.

## Corrections to earlier review claims (recorded for honesty)

1. **S1's "verified all callers are in test modules" was wrong.** Integration
   tests compile as separate crates and cannot see `#[cfg(test)]` items — and
   `sequencer/tests/` *does* call `initialize_open_state` (3 files),
   bare `close_frame_and_batch` (batch_submitter_integration),
   `safe_input_end_exclusive` (ws_broadcaster), and `promote_finalized`
   (snapshot_endpoints). Revised S1: gate those four behind a `test-support`
   cargo feature (or provide public seed helpers); outright deletion remains
   correct for `ordered_l2_txs_for_batch` and `latest_batch_index` (their only
   callers are their own unit tests).
2. **The old "T2 Anvil --no-mining is the big unbuilt lever" note is stale**:
   `set_automine(false)` + `drop_all_pending_txs` exist and power the
   delayed-inclusion e2e today.

## On resurrecting TEST_PLAN.md

Recommendation: **don't resurrect the full scenario matrix** — it rotted once
and its job is better served by three things that already exist or now exist:
the e2e suite's compile-time zone-math asserts and post-test schema-invariant
hook ("plan as code"), the per-WP test obligations in the review ledger, and
this document's owed-test checkboxes (a dated, finite list — not an
ever-growing matrix). If a standing artifact is wanted later, graduate the
"Owed tests" section into `docs/testing.md` once the A-list is burned down.
