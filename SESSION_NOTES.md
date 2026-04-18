# Session Handoff — 2026-04-18 / 2026-04-19

Ephemeral note for the next agent. Delete after absorbing.

## TL;DR

This session landed **seventeen new e2e tests** (19 → 36 passing) across
four batches:

1. §11 outage matrix + recovery critical path (8 tests).
2. Tier A e2e follow-up — WS cursor edges, direct-input drain corners,
   replay determinism, input reader retry (6 tests).
3. Tier A bootstrap edges — first-boot-no-cache, chain-id mismatch via
   live RPC, nonce-0 first-batch recovery (3 tests).

Plus the harness primitives that unlocked them. All work under `tests/`.

- **T7 (libfaketime dynamic)** was already in place from the prior session.
- **T8 (orchestrator-restart)** added: `RespawnAttemptOutcome` /
  `RespawnPolicy` / `respawn_and_watch` / `respawn_until_stable`.
- **T2 (Anvil runtime toggle)** added: `set_automine(bool)` +
  `drop_all_pending_txs` (via `anvil_setAutomine` / `anvil_dropAllTransactions`).
- **`reset_l1_safe_head_synced_at_ms`** added for §7.8.2.
- **`observe_for(Duration)`** added for §7.3.5-style negative controls.

**§7.1.1 deliberately left `[-]` (out of scope).** See "Decisions"
below.

## State of the tree

- **New/modified files** (uncommitted; user plans a squash-later strategy):
  - `tests/harness/src/sequencer.rs`
  - `tests/harness/src/rollups.rs`
  - `tests/harness/src/lib.rs`
  - `tests/e2e/src/test_cases.rs` — 17 new scenarios.
  - `tests/TEST_PLAN.md` — rows flipped; new T2 + T8 tooling rows.
- **Tests**: 36 e2e passing (`just test-rollups-e2e`). Unit/integration
  suite not re-run this session (no sequencer code changed).
- **Lint**: `cargo fmt --all --check` + `cargo clippy --all-targets
  --all-features -- -D warnings` clean.

## Tests landed (this session, both iterations combined)

Outage matrix / recovery critical path:

| Row | Test | Shape |
|-----|------|-------|
| §11.4.1 | `provider_outage_short_hiccup_no_recovery_test` | Brief proxy disconnect, no L1/wall-clock advance; POST /tx keeps working, zero invalidation |
| §11.3.2 | `both_down_danger_zone_sequencer_first_refuses_boot_test` | Both stopped, advance into danger zone, sequencer respawn refuses while L1 still unreachable |
| §11.3.3 | `both_down_danger_zone_proxy_first_restart_cycle_recovers_test` | Both stopped, advance into danger zone, proxy reconnects first; `respawn_until_stable` drives to convergence with cascade |
| §11.1.5 | `sequencer_outage_danger_zone_coupled_restart_cycle_recovers_test` | Coupled wall+L1 advance into danger; orchestrator loop converges |
| §11.2.2-followup | `provider_outage_danger_zone_mid_run_exit_then_restart_cycle_recovers_test` | Mid-run DangerZone exit + reconnect + restart cycle → cascade |
| §7.8.2 | `first_boot_l1_unreachable_never_synced_refuses_boot_test` | `synced_at_ms == 0` branch of wall-clock fallback refuses to boot |
| §11.1.4 | `delayed_inclusion_cascades_on_restart_test` | Mempool-held submission, dropped, advance past MAX_WAIT, respawn cascades |
| §7.3.5 | `aging_open_tip_tolerated_by_zombie_check_test` | Submitter's closed-only zombie check tolerates aging open Tip; fires on subsequent auto-close |

Tier A e2e follow-up (WS / drain / replay / input reader):

| Row | Test | Shape |
|-----|------|-------|
| §4.4.2 | `ws_reconnect_at_invalidated_offset_skips_cleanly_test` | Reconnect at a previously-observed offset that got invalidated; cursor skips cleanly and delivers only post-recovery events |
| §4.1.3 | `ws_subscribe_from_future_offset_waits_silently_test` | Pin the "subscribe beyond head waits silently" contract (consistent with `from_offset=0` on an empty head) |
| §7.4.2 | `recovery_drains_safe_but_undrained_direct_input_test` | Deposit that was safe but never-drained before the sequencer stopped lands in the recovery batch's first frame on respawn |
| §7.4.3 | `recovery_batch_opens_empty_when_no_direct_inputs_pending_test` | Negative control: no deposits → recovery batch opens empty, cascade still fires on aged empty initial Tip |
| §10.1.1 | `replay_matches_live_for_mixed_workload_test` | 3-user mixed workload; post-restart WS catch-up produces per-user state identical to the live replay |
| §5.4.1 / §5.4.2 | `provider_outage_input_reader_retries_after_reconnect_test` | T1 proxy disconnect + L1 deposit (bypassing proxy) + reconnect → reader's retry loop catches up without crashing |

Tier A bootstrap edges (the final batch this session):

| Row | Test | Shape |
|-----|------|-------|
| §8.1.2 | `first_boot_no_cache_l1_unreachable_refuses_boot_test` | `clear_l1_bootstrap_cache` after a normal boot, then respawn through a disconnected proxy. Bootstrap discovery has nothing to fall back to → refuses boot. Distinct from §7.8.2 (wall-clock fallback): hits the *earlier* `InputReader::new` discovery step. |
| §8.2.1 / §8.3.1 / §6.5.1 | `chain_id_mismatch_via_live_rpc_refuses_boot_test` | H7 RPC-path regression. Spawns the full sequencer binary against real Anvil with a mismatched `--chain-id` (override via new `set_chain_id_override` harness method); bootstrap-time RPC check returns `RunError::ChainIdMismatch`. Reset-and-respawn proves the failed attempt didn't poison the cache. The previous integration-level scaffolding in `sequencer/tests/chain_id_validation.rs` (cache path) stays — these complement each other. |
| §7.5.1 / §7.5.2 | `nonce_zero_recovery_invalidates_then_accepts_at_nonce_zero_test` | Nonce-0 first-batch recovery edge. Uses T2 to ensure the first-ever batch's L1 submission is dropped before reaching the chain. Cascade fires; recovery batch reuses nonce 0 (parent NULL — no genesis sentinel). Then drives 150 transfers + 2 explicit L1 confirmations to land the recovery batch in `safe_accepted_batches` at the reused nonce, proving §7.5.2 (`populate_safe_accepted_batches_inner` cursor handles reuse). |

## Harness primitives added

Inline-documented in `tests/harness/src/sequencer.rs`:

- `respawn_and_watch(stabilization) -> RespawnAttemptOutcome` — classifies a
  single respawn attempt as `Stable` / `RespawnFailed(String)` /
  `ExitedPostRespawn(ExitStatus)`.
- `respawn_until_stable(policy) -> Vec<RespawnAttemptOutcome>` — loops
  `respawn_and_watch`, advancing L1+wall by `policy.advance_per_retry`
  between failed attempts. Required for the danger-zone-to-cascade
  convergence path (closed batch only cascades once it ages past
  `MAX_WAIT_BLOCKS`, so each retry needs L1 + wall-clock drift).
- `set_automine(bool)` + `drop_all_pending_txs()` — T2. Toggle Anvil's
  auto-mining and flush its mempool without respawning Anvil or affecting
  other tests. Chosen over `--no-mining` spawn flag precisely because
  it's runtime-toggleable.
- `reset_l1_safe_head_synced_at_ms()` — zeros the DB's
  `l1_safe_head.synced_at_ms` while the sequencer is stopped, to simulate
  "never synced L1" without reconstructing a truly-blank DB.
- `observe_for(grace) -> Option<ExitStatus>` — watches the child for
  `grace` without consuming its exit handle. Returns `None` if still
  alive (safe to continue), `Some(status)` if the child exited within
  the window. Used by §7.3.5 as a negative-control "stayed up" check.
- `clear_l1_bootstrap_cache()` — DELETE on `l1_bootstrap_cache`. Used
  by §8.1.2 to mimic a never-bootstrapped DB, and by §8.2.1 to force
  the live-RPC chain-id check (bypasses the cache-path that would
  catch the mismatch first).
- `set_chain_id_override(Option<u64>)` — overrides the `--chain-id`
  argument the sequencer is spawned with on the next respawn. Used
  by §8.2.1 / §8.3.1 to inject a deliberately wrong chain id and
  exercise the bootstrap-time RPC mismatch path.
- `count_safe_accepted_batches() -> (count, min_nonce)` — read-only
  snapshot of `safe_accepted_batches`. Used by §7.5.2 to verify that
  the recovery batch's L1 submission lands and gets accepted at its
  expected (reused-zero) nonce.

## Decisions worth remembering

### §7.1.1 — skipped, marked `[-]`

Originally on the Tier A list. After investigating:

- **Unique submitter-side code path it would exercise** (live
  `check_danger_zone` firing on closed-in-danger batch): **already
  covered** by §7.3.5. Both tests reach the same submitter state
  (closed batch in `batches`, not in `safe_accepted_batches`); the
  setup story differs (§7.3.5 = aged Tip auto-closes; §7.1.1 = mempool
  lost submission), but the code path through
  `BatchSubmitterError::DangerZone` is identical.
- **Other unique path**
  (`populate_safe_accepted_batches_inner`'s `batch_age_is_stale`
  continue, i.e., the scheduler's "skip past-stale inclusion" logic):
  has a unit test. Hard to exercise e2e because Anvil's `anvil_mine(N)`
  mines any pending tx into the first mined block — you can't hold a
  tx in the mempool while L1 advances.
- **Bonus obstacle**: the submitter's
  `wait_for_confirmations` timeout is `(confirmation_depth + 1) × 2 ×
  ETHEREUM_BLOCK_TIME_SECS`, hard-coded against
  `ETHEREUM_BLOCK_TIME_SECS = 12s`. Minimum 24 s at depth 0. Tokio's
  `Instant`-based timers aren't intercepted by libfaketime on macOS, so
  we can't fast-forward through that wait.

Verdict: the effort-to-value ratio doesn't justify adding the test. If
T3 ever lands (sub-second poll interval + config-tunable
`ETHEREUM_BLOCK_TIME_SECS`), §7.1.1 becomes a small marginal win; until
then, treat §7.3.5 + §11.1.4 as covering the delayed-inclusion space.

### `set_faketime_offset` wants `"+Ns"`, not `"+2h5m"`

I initially wrote §7.3.5 using `"+2h5m"` for the wall-clock jump past
`max_batch_open`. The test hung in `wait_for_exit`; libfaketime
doesn't parse combined unit forms reliably. Fix: use `"+7500s"` (same
format `advance_wall_and_mine` writes). Safer default going forward.

### `§7.3.5`'s `observe_for` invariant

The 8 s observation window isn't arbitrary — it must span at least one
full `batch_submitter_idle_poll_interval_ms` (default 5 s) + input
reader poll (~2 s). If someone lowers those defaults in the future,
consider whether §7.3.5's window is still large enough (it currently
has ~1 s of headroom).

### `§11.1.5`'s `outcomes.len() >= 2` assertion

Load-bearing: without it, a future change that made the first respawn
converge (e.g., startup recovery cascading at `danger_threshold`
instead of `MAX_WAIT_BLOCKS`) would silently turn this test into a
trivial single-respawn test, losing the flush/shutdown-path coverage.

### §11.1.4's re-enable-auto-mining-before-respawn step

Also load-bearing: the startup flusher submits a no-op at the stuck
wallet-nonce slot and needs auto-mining on to see it confirm.
Otherwise the flush hangs. Don't reorder the setup.

### §4.4.2's "reconnect across invalidation" reframing

The original TEST_PLAN phrasing ("live subscriber at the time of
invalidation") is structurally impossible — invalidation fires inside
`run_preemptive_recovery`, after the sequencer exits (DangerZone or
stop), so the WS socket always dies before the cascade. The
meaningful test is the reconnect arc: captured offset → kill →
cascade → reconnect at captured offset → cursor skips cleanly. Row
in TEST_PLAN is updated to match.

### §10.1.1 complements, doesn't replace, `restart_and_replay_test`

The existing `restart_and_replay_test` already does a restart + WS
catch-up + assert-replay-state for a single-user workload, and it
pins the specific balances. §10.1.1 adds a distinct test because the
property being asserted is *general* (any live workload must replay
deterministically), not the particular expected values, and because
it sweeps a wider multi-sender / multi-op workload. Keep both — the
single-sender test catches value regressions; the mixed-workload
test catches replay-divergence regressions.

### Wallet endpoints don't survive respawn

`runtime.endpoint()` rebinds to a fresh local port on every respawn
(see `build_local_endpoint`). Any `WalletL2Client` / `WsClient`
created BEFORE a respawn still holds the old endpoint string and
will fail with "tcp connect failed" on the next call.

Idiom: re-create both via `runtime.wallet_l2(...)` and `runtime.ws(...)`
after every respawn. Caught this in §7.5.x during development; the
post-recovery transfer phase failed until the wallet was recreated.

### §7.5.2's confirmation timing

The submitter's `wait_for_confirmations` is hard-coded against
`ETHEREUM_BLOCK_TIME_SECS = 12` and waits for `confirmation_depth +
1 = 3` confirmations. With Anvil's instamine, the submission lands
at 1 confirmation (the block carrying it). To unblock the wait
without sitting through the 72 s timeout, §7.5.2 explicitly mines 2
extra blocks via `mine_l1_blocks(2)` after the submission. If T3
ever lands and `confirmation_depth` becomes test-tunable, this
manual mining can go away.

## Open items

### Tier A — remaining recovery-critical-path work

Nothing. §7.1.1 closed as `[-]`; the critical path is fully covered.

### Tier B — tooling quality-of-life

- **T3** — plumb `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS` through
  `ManagedSequencerConfig`. Would shorten §11.1.4's 7 s sleep and
  §7.3.5's 8 s observation window, and open up §7.1.1 as a cheap test
  (if combined with a config-tunable `ETHEREUM_BLOCK_TIME_SECS` at the
  poster layer). Medium work.

### Tier C — broader e2e coverage (mostly done)

The remaining `tests/e2e` gaps are very small after this session:

- **§4.3.1** — 65th WS subscriber rejected. Already covered at
  integration level (`ws_subscribe_rejects_when_subscriber_limit_is_reached`
  in `sequencer/tests/ws_broadcaster.rs`); duplicating at e2e is
  marginal and CI fd-limit-prone.
- **§9.1.3 / §9.1.4** — shutdown during batch submission / input
  reader poll. Timing-sensitive; would need T2 + careful mid-flight
  signaling. Lower priority than what's left in other layers.
- **§2.1.2** — soft-confirmation latency budget (POST → WS within 500
  ms). Useful as a regression guard but flaky on slow CI; probably
  needs a generous bound.

Everything else of value at the e2e layer has landed.

### Tier D — better at other layers

- **§2.3.1–5** (API body hardening) — better in
  `sequencer/tests/e2e_sequencer.rs`. Spinning up the full e2e stack
  for a 400/413 check is wasteful.
- **§12.1.1** (schema CHECKs) — unit tests in `storage/`.
- **§7.7.4/5** (flusher H5/H6) — better in
  `batch_submitter_integration.rs`; assertions are on tx field
  values, not end-to-end flows.

### Tier E — needs sequencer-side work (out of scope here)

- **T5 failpoints** — gates §2.10.1 / §5.3.1 / §7.2.2 / §7.6.3.
- **TLA+ alignment** — docs/spec sync with the parent-pointer schema
  refactor.

## Commit hygiene

The user has opted for a squash-later strategy on this branch. As of
handoff, all work is uncommitted. Natural squash boundaries if the
user changes their mind:

1. **T8 + five §11.x tests** (orchestrator-restart + matrix closure)
2. **T2 + §11.1.4** (delayed-inclusion)
3. **§7.8.2** (first-boot L1 down)
4. **§7.3.5** (aging Tip negative control)
5. **TEST_PLAN + SESSION_NOTES updates** bundled through each above

## Context a new agent will need

All doc pointers from prior handoffs remain accurate. Specific to this
session:

- New harness primitives are documented inline in
  `tests/harness/src/sequencer.rs` (`respawn_and_watch`,
  `respawn_until_stable`, `set_automine`, `drop_all_pending_txs`,
  `reset_l1_safe_head_synced_at_ms`, `observe_for`).
- `cargo run -p rollups-e2e -- <test_name>` runs a single scenario;
  `just test-rollups-e2e` runs all 27 (~145 s).
- Before running tests fresh in a clean worktree: `just setup` +
  `just canonical-build-machine-image`. Both were run earlier in this
  session.
