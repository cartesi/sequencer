# Sequencer Test Plan

A living document tracking the scenarios we need to exercise to have confidence the sequencer is correct under its threat model. This is **scenario-first** — it describes behaviors, not code paths. A behavior without a test is a liability regardless of how much code coverage the implementation has.

The project is **security-critical**. The open-batch-staleness bug was caught by an e2e test written for the behavior ("after a stale sequencer restarts, the invalid transfer must not reappear"), not by any code-level check. That experience is why this plan prioritizes *what should happen* over *what code runs*.

## Status markers

- `[x]` — scenario has a test, known to pass
- `[ ]` — planned, needs implementation
- `[!]` — test exists but is flaky, partial, or needs hardening
- `[?]` — coverage unclear, needs verification against existing tests
- `[-]` — out of scope under current tooling (see §14)

## Recent regression work

**Phase 1 — Security-review regressions** (completed): 19 new tests locking in the fixes from the staged security review. See §1.1 (V1), §7.3 (open-batch staleness), §8.5 (H3/H4 provider), §2.10 (H2 error body), §6.5/§8.3 (H7 chain-id cache path). Notably, the IPv6-loopback test caught a latent bug in the H4 fix itself (`host_str()` returns bracket-wrapped `[::1]` for IPv6 literals; original `matches!` check missed it).

**Phase 2 — Tooling + zone matrix** (completed):
- Built `tests/harness/src/proxy.rs` — programmable TCP proxy (`TcpProxy::spawn/disconnect/reconnect`) with 6 unit tests exercising the forwarder, disconnect, and reconnect paths. Handles both clean-EOF and RST close behavior (OS-dependent).
- Added `ManagedSequencer::set_l1_endpoint_override` so tests can route the sequencer through the proxy while still mining blocks directly on Anvil (bypassing the proxy) to simulate "L1 advanced while the gateway was down."
- 3 new e2e scenarios registered and **verified end-to-end with `just test-rollups-e2e`**: §11.1.1 (sequencer outage, pre-danger), §11.1.2 (sequencer outage, danger zone), §11.2.3 (provider outage, past-stale using the proxy). Full suite: 15/15 passing in ~53s.
- 3 H8 clap-validation regression tests locking `SEQ_SECONDS_PER_BLOCK >= 1`.

**Lessons surfaced by actually running the e2e suite:**
- Wallet-client nonce state: the harness's `WalletL2Client` initializes `next_nonce: 0`. In no-cascade restart scenarios (where on-chain nonce is preserved), post-restart submissions need explicit nonce-state plumbing. Current workaround: the pre-danger/danger-zone scenarios don't submit new work after the restart.
- Wall-clock fallback measures *real* seconds, not mined blocks. `anvil_mine(N)` advances the chain's block count in milliseconds of wall-clock time, so the fallback correctly reports "not yet in danger" even after mining 1250+ blocks. The block-time coupling assumption is documented in `docs/threat-model/README.md`.
- Built `ManagedSequencer::rewind_synced_at_ms` helper — rewrites `l1_safe_head.synced_at_ms` in the DB while the sequencer is stopped. Semantically equivalent to advancing the wall clock.

**Danger-check unification bug (fixed):**

The first e2e attempt at `provider_outage_wall_clock_refuses_boot_test` surfaced a real structural bug. Two code paths asked "is a batch in danger" with asymmetric scope:

- `check_danger_zone` (live submitter tick + wall-clock fallback at boot) — closed-and-nonced batches only.
- `detect_and_recover` (atomic cascade) — closed + open batches (post §7.3 fix).

The asymmetry meant an open batch could age past the danger threshold while L1 was unreachable and the preemptive path would miss it. Fixed by splitting the public API around the semantic distinction:

- **`Storage::check_danger_zone`** (closed-only) — zombie-detection check. Live submitter keeps using this: its response (shutdown → flush pending nonces → restart) only makes sense for submitted batches with potential zombie risk.
- **`Storage::check_any_unresolved_batch_in_danger`** (unified, closed + open) — wall-clock fallback uses this at startup when L1 is unreachable. Refuses to boot if any unresolved batch might be past-stale.
- **`detect_and_recover`** (at `MAX_WAIT_BLOCKS`) — uses `find_first_batch_in_danger` (unified). Handles actually-stale open batches via cascade.

Behind the scenes, all three share `find_first_batch_in_danger` and `find_closed_frontier_batch_in_danger` in `storage/recovery.rs`. The old one-step helpers `detect_stale_and_cascade` and `check_open_batch_staleness` are removed.

**Key insight from the failure:** a first attempted refactor unified ALL callers behind the unified helper. That broke the live submitter — it started crashing on aging open batches (which have no zombies to flush), causing a restart loop. The corrected split keeps "zombie danger" (closed-only) separate from "any danger" (unified), because their expected responses differ: zombie-danger → flush + shutdown; open-batch-danger → let the batch close naturally or refuse to boot.

**Tests landed:**
- `check_danger_zone_does_not_flag_open_batch_zombie` — regression for the submitter worker loop.
- `check_any_unresolved_flags_stale_open_batch` + `check_any_unresolved_does_not_flag_fresh_open_batch` — regressions for the unified helper.
- `provider_outage_wall_clock_refuses_boot_test` — e2e proving the full chain works end-to-end.

**Still open from Phase 1**:
- §6.5.1 / §8.3.1 (H7 RPC-path) — needs real InputBox contract, deferred to `tests/e2e/` harness
- §2.10.1 (H1 rusqlite leak) — needs failpoint injection (tool T5)
- §8.4.1 (preemptive_margin_blocks) — runtime `assert!`; could be a `#[should_panic]` test

**Deferred design-review items:**
- [ ] **TLA+ spec alignment with the danger-check split.** The `preemptive.tla` spec models "danger zone detection" at a high level. After the `check_danger_zone` vs `check_any_unresolved_batch_in_danger` split (surfaced by the open-batch-in-danger bug), we should re-read the spec to confirm:
  - Whether the spec makes the zombie-vs-aging distinction explicit, or whether both callers are modeled as one "DangerFired" action.
  - If the spec has the same unification flaw as the pre-fix code (i.e., treats any batch-in-danger as triggering flush + shutdown), whether that is a gap in the spec or a gap in the implementation.
  - Whether the open-batch case is covered by a dedicated action or elided as part of the Tip→Pending→Silver lifecycle.
  - Update the spec if needed; leave a short note in `docs/recovery/` if the implementation is strictly more cautious than the spec.

## Test layers

| Layer | Purpose | Examples | Runs where |
|-------|---------|----------|-----------|
| **Unit** | Pure functions, data structures, per-module invariants | `fee.rs`, `batch.rs` SSZ round-trip, `storage/recovery.rs` inline tests | `cargo test --lib` |
| **Integration** | Crate-level wiring with mocks or Anvil | `sequencer/tests/*.rs`, inclusion-lane tests | `cargo test` (Anvil optional) |
| **E2E** | Full binary + Anvil + harness, real RPC, real DB | `tests/e2e/src/test_cases.rs` | `cargo test -p rollups-e2e` |
| **Formal** | Bounded model checking | `docs/recovery/preemptive.tla` | `tlc` |

The existing convention is documented in [`AGENTS.md`](../AGENTS.md). This plan should coexist with that guide, not replace it.

---

## 1. Wire Compatibility (Sequencer ↔ Scheduler)

These are the **cross-boundary** invariants. Any divergence here is catastrophic: the scheduler is the canonical authority, and a mismatch breaks every honest transaction.

| # | Scenario | Layer | Status | Notes |
|---|----------|-------|--------|-------|
| 1.1 | Sign a `UserOp` with `sequencer_core::build_input_domain(chain_id, app)`, decode with the same constructor, assert recovered sender matches signer | Integration (`sequencer/tests/e2e_sequencer.rs::v1_regression_shared_domain_recovers_signer`) | `[x]` | **V1 regression.** Plus a negative test that a `name:None` domain recovers a DIFFERENT address — catches any reintroduction of the V1 bug. |
| 1.2 | Sign with chain_id=X, attempt recover with chain_id=Y → recovered address ≠ signer | Integration (`v1_regression_domain_fields_all_affect_recovery`) | `[x]` | Cross-chain replay protection |
| 1.3 | Sign with app=X, attempt recover with app=Y → recovered address ≠ signer | Integration (same test) | `[x]` | Cross-app replay protection |
| 1.4 | SSZ encode a `Batch`, decode, re-encode → byte-identical | Unit | `[?]` | Determinism; may already be covered by ssz-derive tests |
| 1.5 | SSZ decode fails cleanly on truncated payload, garbage bytes, malformed offsets → returns `DecodeError`, never panics | Unit | `[ ]` | Property-test candidate |
| 1.6 | `MAX_WAIT_BLOCKS` constant is the same value on sequencer and scheduler sides at link time | Unit | `[x]` | Shared via `sequencer_core::MAX_WAIT_BLOCKS` — structural guarantee, no runtime check needed |
| 1.7 | S-malleability neutralized: signing the same op twice produces low-s and high-s forms; both recover the same sender | Unit | `[ ]` | Already guaranteed by alloy; test confirms the guarantee at our boundary |

---

## 2. `POST /tx` — Public Attack Surface

### 2.1 Happy path

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.1.1 | Valid signature, correct sender, correct nonce, sufficient balance → admitted, returns sender + nonce in 200 body | `[x]` | `deposit_transfer_withdrawal_test` |
| 2.1.2 | Soft confirmation arrives on WS within 500 ms of successful POST | `[?]` | Check e2e tests assert this |

### 2.2 Signature validation

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.2.1 | Forged signature (valid format, wrong key) → 422, no state change | `[x]` | `forged_signature_rejected_test` |
| 2.2.2 | Signature wrong hex length → 400 before crypto work | `[ ]` | |
| 2.2.3 | Signature valid bytes, invalid parity byte → 422 | `[ ]` | |
| 2.2.4 | Signature recovers a different address than claimed `sender` field → 422 | `[ ]` | Implicit in forged test but worth making explicit |

### 2.3 Body / format

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.3.1 | Body exceeds `max_body_bytes` (default 4 KB) → 413 before JSON parse | `[ ]` | Regression for `DefaultBodyLimit` enforcement |
| 2.3.2 | Body is not JSON → 400 with `"invalid JSON"` (H2 regression: must NOT leak serde internals) | `[ ]` | **Hardening regression test** |
| 2.3.3 | Body is JSON but missing fields → 400, doesn't leak deserialization error text | `[ ]` | H2 regression |
| 2.3.4 | Content-Type other than `application/json` → 400 with `"missing content type"` | `[ ]` | H2 regression |
| 2.3.5 | User op `data` field exceeds `max_user_op_data_bytes` → 400 before signature verify | `[ ]` | |

### 2.4 Nonce rules

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.4.1 | First tx with nonce 0 → accepted, next expected becomes 1 | `[x]` | `deposit_transfer_withdrawal_test` |
| 2.4.2 | Tx with nonce too low (e.g., replay) → 422 `InvalidNonce`, no state change | `[?]` | `rejected_user_op_not_broadcast_test` may cover |
| 2.4.3 | Tx with nonce too high (gap) → 422 `InvalidNonce`, no state change | `[ ]` | |
| 2.4.4 | `InvalidNonce` response does NOT get broadcast on WS | `[x]` | `rejected_user_op_not_broadcast_test` |

### 2.5 Fee rules (V3 regression)

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.5.1 | `max_fee < current_frame_fee` → 422 `InvalidMaxFee` | `[x]` | `fee_below_minimum_rejected_test` |
| 2.5.2 | `max_fee == current_frame_fee` → accepted (boundary) | `[ ]` | |
| 2.5.3 | Rejection handled by trait-default `validate_and_execute_user_op` (V3 regression) | `[x]` | Unit test in `app-core/wallet.rs` |

### 2.6 Balance rules

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.6.1 | `balance < fee_to_linear(current_fee)` → 422 `InsufficientGasBalance`, no state change | `[?]` | |
| 2.6.2 | Rejected op does NOT broadcast | `[?]` | |

### 2.7 Admission control

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.7.1 | Queue full → `429 OVERLOADED` with body `"queue full"` | `[ ]` | Hard to trigger reliably; maybe property test |
| 2.7.2 | Queue-full response does not leak per-sender info | `[ ]` | Hardening |

### 2.8 Concurrency

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.8.1 | Two concurrent POSTs for same (sender, nonce) → exactly one admitted, one gets `InvalidNonce` | `[x]` | `concurrent_user_ops_test` |
| 2.8.2 | Rejected concurrent op produces no state change | `[?]` | |

### 2.9 Shutdown semantics

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.9.1 | Mid-request shutdown: in-flight requests get 503 or clean error | `[x]` | `shutdown_during_inflight_test` |
| 2.9.2 | Post-shutdown POST → 503 immediately | `[?]` | |

### 2.10 Error-body hardening (regression tests for security review findings)

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.10.1 | DB-error response body contains `"internal storage error"`, not rusqlite text | `[-]` | **H1 regression** deferred — requires failpoint injection (tool T5). Code review + code is trivial (`format!` removed in favor of fixed string). |
| 2.10.2 | Malformed JSON response body is from fixed taxonomy, doesn't reflect bytes | `[x]` | **H2 regression** in `e2e_sequencer.rs::api_rejects_malformed_json_as_bad_request` — asserts `"message":"invalid JSON"` AND that attacker-submitted bytes don't appear in response. |
| 2.10.3 | Missing Content-Type produces fixed `"missing content type"` message | `[x]` | H2 regression in `api_rejects_missing_content_type_with_fixed_message` |

---

## 3. Inclusion Lane (Hot Path)

### 3.1 Chunk commit semantics

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 3.1.1 | Ack returns AFTER chunk is durably committed to SQLite, not merely enqueued | `[x]` | `ingress/inclusion_lane/tests.rs` |
| 3.1.2 | Storage failure during chunk commit → every pending op gets `Err`, lane crashes, no partial ack | `[x]` | Covered by existing lane tests |
| 3.1.3 | Chunk commit triggers autoincrement insert into `sequenced_l2_txs` via SQL trigger | `[x]` | `trg_sequence_user_op` — verified by integration tests |

### 3.2 Frame rotation

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 3.2.1 | Frame closes on direct-input drain and opens a new one at the current safe_block | `[?]` | |
| 3.2.2 | New frame's `fee_price` sampled from `batch_policy_derived.recommended_fee` at rotation | `[?]` | |
| 3.2.3 | Frame fee stays fixed for the frame's lifetime even if policy is updated mid-frame | `[ ]` | Regression for "frames.fee immutable" invariant |

### 3.3 Batch closure

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 3.3.1 | Batch closes when `max_batch_user_op_bytes` target is reached | `[x]` | `batch_closes_when_max_user_op_bytes_is_reached` |
| 3.3.2 | Batch closes when deadline (`max_open_time`) elapses | `[x]` | `batch_closes_when_max_open_time_is_reached` |
| 3.3.3 | Closed batch becomes eligible for nonce assignment | `[?]` | |

### 3.4 Single-writer invariant

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 3.4.1 | Inclusion lane is sole writer of open batch/frame state; no cross-task races | `[-]` | Structural, enforced by `&mut self` and single-task spawn; not testable at runtime |

### 3.5 Direct-input draining

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 3.5.1 | Direct input arriving between two user ops is drained before the next frame's ops (ordering) | `[x]` | `direct_input_not_safe_yet_test`, `safe_inputs_already_available_are_sequenced_before_later_user_ops` |
| 3.5.2 | Multiple direct inputs in the same block drained in `safe_input_index` order | `[x]` | `multi_deposit_same_block_test` |

---

## 4. WS Subscribe / L2 Feed

### 4.1 Happy subscription

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 4.1.1 | Subscribe `from_offset=0` → receive all historical events then live | `[x]` | Many tests |
| 4.1.2 | Subscribe `from_offset=N` (N < head) → receive tail only | `[x]` | `reconnect_from_offset_test` |
| 4.1.3 | Subscribe `from_offset=future` → waits for new events, doesn't error | `[ ]` | Property of the cursor query |

### 4.2 Catch-up bounds

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 4.2.1 | Catch-up window exceeded (>50000 events behind) → WS close code 1008, reason `"catch-up window exceeded"` | `[ ]` | Hard to produce 50000 events in a test; maybe reduce cap for test builds |
| 4.2.2 | Close reason is a constant string, not attacker-influenced | `[ ]` | Hardening regression |

### 4.3 Subscriber limit

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 4.3.1 | 65th concurrent subscriber → rejected at handshake | `[ ]` | |

### 4.4 Invalidation visibility

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 4.4.1 | After cascade-invalidation, subscribing `from_offset=0` does NOT deliver events from invalidated batches | `[x]` | `recovery_after_stale_batches_test` (regression for open-batch bug) |
| 4.4.2 | Subscriber live at the time of invalidation: next events come from the recovery batch only | `[ ]` | |

### 4.5 Data exposure

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 4.5.1 | Broadcast message contains only `sender`, `fee`, `data`, `offset`, `kind` — no DB internals, no debug info | `[?]` | Structural; unit-test the `BroadcastTxMessage` serializer |
| 4.5.2 | No timing side channel exposes internal batch-close decisions | `[-]` | Out of scope (timing attacks) |

---

## 5. L1 Input Reader

### 5.1 Event ingestion

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 5.1.1 | `InputAdded` event at safe block N → row in `safe_inputs` with block_number=N | `[?]` | Covered by deposit e2e |
| 5.1.2 | Multiple events in one `eth_getLogs` response ingested in order | `[?]` | |
| 5.1.3 | Zero events in a safe-head advance → `l1_safe_head.block_number` advances, `synced_at_ms` updates | `[ ]` | |

### 5.2 Sender classification

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 5.2.1 | Event from batch-submitter address → NOT stored as direct input (opaque to safe_inputs) | `[?]` | |
| 5.2.2 | Event from any other address → stored verbatim as direct input regardless of payload bytes | `[?]` | |

### 5.3 Safe-head atomicity

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 5.3.1 | Event insert + safe_head update are atomic (same transaction); crash mid-insert leaves both unchanged | `[ ]` | Could test via injected mid-tx panic |

### 5.4 RPC error handling

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 5.4.1 | Transient `Provider` error → reader retries, does not crash | `[ ]` | Needs proxy to toggle RPC |
| 5.4.2 | Provider times out → reader logs and retries | `[ ]` | Needs proxy |
| 5.4.3 | Storage error during insert → reader fails loudly (fail-stop) | `[ ]` | |

### 5.5 Long-range partition

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 5.5.1 | Range that triggers `SEQ_LONG_BLOCK_RANGE_ERROR_CODES` splits in half, both halves succeed | `[ ]` | |
| 5.5.2 | Range splits down to 1 block and still fails → bubbles up cleanly | `[ ]` | |

---

## 6. Batch Submitter

### 6.1 Nonce management

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 6.1.1 | Nonce derived from `Latest` account nonce each tick — no local state | `[x]` | `batch_submitter_integration.rs` |
| 6.1.2 | Multiple pending batches → submitted at contiguous nonces starting from `Latest` | `[x]` | Same |
| 6.1.3 | After confirmation, next tick's `Latest` reflects the increment | `[?]` | |

### 6.2 Confirmation depth

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 6.2.1 | `SEQ_BATCH_SUBMITTER_CONFIRMATION_DEPTH=2` means tx watched until `depth+1=3` confirmations | `[?]` | |
| 6.2.2 | Confirmation timeout returns `Ok` (not error); next tick reassesses | `[?]` | |

### 6.3 Fee handling

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 6.3.1 | Batch submission uses `estimate_eip1559_fees()` result | `[?]` | |
| 6.3.2 | "Replacement underpriced" is not a stall (just retry next tick with current estimate) | `[?]` | Documented in security review as expected behavior |

### 6.4 Provider outage

See §11 matrix rows for full outage behavior.

### 6.5 Chain-id validation at startup (H7 regression)

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 6.5.1 | Sequencer configured with `--chain-id=X`, RPC returns Y → startup returns `RunError::ChainIdMismatch`, no panic, no DB writes | `[!]` | **H7 regression (RPC path) deferred** — `chain_id_validation.rs` has a scaffolded test, but it requires a real InputBox contract deployed to Anvil (chain-id check fires AFTER `InputReader::new`'s bootstrap contract call). Proper coverage lives in `tests/e2e/` harness which has `just setup` deployments. |
| 6.5.2 | L1 unreachable at startup with cache present, cached chain_id matches config → boots | `[x]` | Positive control in `chain_id_match_does_not_produce_mismatch_error` |
| 6.5.3 | L1 unreachable at startup with cache present, cached chain_id differs → returns `RunError::ChainIdMismatch`, no panic | `[x]` | **H7 regression (cache path)**: `chain_id_mismatch_from_cache_returns_typed_error` |

---

## 7. Recovery Procedure (CRITICAL)

The largest and most sensitive section. The open-batch bug demonstrates that design gaps here have silent-corruption consequences. Every transition in the recovery state machine needs a test.

### 7.1 Detection paths

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.1.1 | Frontier batch (nonce-bearing, closed, accepted) crosses `MAX_WAIT_BLOCKS` by inclusion staleness → cascade-invalidated on next check | `[ ]` | Needs `--no-mining` to hold batch submission |
| 7.1.2 | Open batch (not yet closed) crosses `MAX_WAIT_BLOCKS` by current staleness → cascade-invalidated | `[x]` | `recovery_after_stale_batches_test` (**the bug we caught**) |
| 7.1.3 | Batch in danger zone but not yet stale → flush triggers, but no cascade | `[ ]` | See §11 zone matrix |
| 7.1.4 | Batch pre-danger-zone → no flush, no cascade | `[ ]` | See §11 zone matrix |

### 7.2 Cascade invalidation

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.2.1 | Stale batch N cascades to all batches with `batch_index >= N` | `[x]` | `storage/recovery.rs` unit tests |
| 7.2.2 | Cascade is a single atomic SQL transaction; crash mid-cascade leaves DB unchanged | `[ ]` | Needs failpoint injection |
| 7.2.3 | `valid_*` views hide invalidated batches immediately after cascade | `[x]` | Covered by inline tests |
| 7.2.4 | `batch_nonces` rows for invalidated batches are NOT deleted (nonces can be reused) | `[x]` | Covered by `detect_and_recover_does_not_false_match_after_nonce_reuse` |

### 7.3 Open-batch-only case (NEW regression zone — V4 + open-batch fix)

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.3.1 | Sequencer stops before batch closure, L1 advances past MAX_WAIT_BLOCKS, restart invalidates open batch | `[x]` | `recovery_after_stale_batches_test` (e2e) + `open_batch_stale_by_current_safe_block_is_invalidated` (unit) |
| 7.3.2 | Same scenario with NO direct inputs pending → recovery batch opens, empty frame | `[x]` | Implicit in `open_batch_stale_by_current_safe_block_is_invalidated` (no deposits seeded) |
| 7.3.3 | Closed-and-nonced batch stale + open batch also stale → both in one cascade | `[x]` | `closed_unsubmitted_stale_and_open_stale_both_cascade` |
| 7.3.4 | `check_open_batch_staleness` returns `None` when open batch is NOT stale → no false positive cascade | `[x]` | **Critical negative test**: `open_batch_not_yet_stale_is_not_invalidated` + boundary tests (`open_batch_exactly_at_threshold_is_invalidated`, `open_batch_one_block_below_threshold_is_not_invalidated`) |

### 7.4 Re-drain direct inputs

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.4.1 | Direct input was drained into invalidated batch → re-drained into recovery batch | `[x]` | `recovery_redrains_direct_inputs_and_replay_sees_them_once` |
| 7.4.2 | Direct input that was already safe but NOT yet drained → included in recovery batch's first frame | `[ ]` | |
| 7.4.3 | No direct inputs pending → recovery batch opens empty | `[ ]` | |
| 7.4.4 | A subscriber seeing events across recovery sees each direct input exactly once | `[x]` | Implicit in 7.4.1 |

### 7.5 Nonce-0 edge case

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.5.1 | First-ever batch (nonce 0) goes stale before any batch reaches Gold → recovery invalidates and opens fresh batch 0 | `[ ]` | No genesis sentinel in our impl; must handle natively |
| 7.5.2 | After 7.5.1, scheduler accepts the recovery batch at nonce 0 (nonce space reused) | `[ ]` | |

### 7.6 Idempotency & crash-safety

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.6.1 | Run `detect_and_recover` twice on the same state → second run is no-op | `[x]` | `detect_and_recover_is_idempotent` |
| 7.6.2 | Crash AFTER cascade INSERT but BEFORE `open_recovery_batch_in_tx` → on restart, a recovery batch is opened (torn state) | `[x]` | `detect_and_recover_opens_batch_after_torn_invalidation` |
| 7.6.3 | Crash AFTER open_recovery_batch → restart finds valid open batch, does nothing | `[ ]` | |
| 7.6.4 | The entire recovery procedure (populate + assign + detect + open) runs in a single `Immediate` transaction | `[x]` | Structural, verified by reading |
| 7.6.5 | `populate_safe_accepted_batches` is resumable (cursor-tracked, `INSERT OR IGNORE`) | `[x]` | |
| 7.6.6 | `assign_batch_nonces` is idempotent (`INSERT OR IGNORE`) | `[x]` | |

### 7.7 Mempool flusher

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.7.1 | Pending wallet-nonce slot → flusher submits a no-op that consumes the slot | `[x]` | Existing Anvil-backed flusher tests |
| 7.7.2 | No pending slots → flush is instant no-op | `[x]` | |
| 7.7.3 | Flusher no-op competes with a batch tx at the same nonce; one of them lands, slot is consumed | `[x]` | |
| 7.7.4 | Flusher fee bump satisfies Ethereum's ≥10% replacement rule (H5 regression) | `[ ]` | Explicit assertion that both `max_fee_per_gas` and `priority_fee` are bumped |
| 7.7.5 | Flusher `confirmation_timeout` derives from `seconds_per_block` config (H6 regression) | `[ ]` | |
| 7.7.6 | Flusher outer loop runs without timeout; inner watch-timeout re-enters the loop | `[x]` | Verified in review |
| 7.7.7 | Flusher survives extended provider outage — retries forever, completes when provider returns | `[ ]` | Needs proxy |

### 7.8 Wall-clock fallback

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.8.1 | L1 unreachable, elapsed wall time estimates `missed_blocks > danger_threshold` → recovery triggers | `[x]` | `provider_outage_wall_clock_refuses_boot_test` in `tests/e2e`. Validated end-to-end: proxy disconnected → `anvil_mine(1500)` + `rewind_synced_at_ms(5h)` → respawn fails with `L1UnreachableInDangerZone` → proxy reconnect + respawn succeeds + cascade fires. |
| 7.8.2 | `l1_safe_head.synced_at_ms == 0` (never synced) → treat as danger zone, return `L1UnreachableInDangerZone` error | `[ ]` | First-boot-with-L1-down case; would need `ManagedSequencer` to accept a pre-spawn L1 endpoint override (currently only respawn honors it). |
| 7.8.3 | `SystemTime::now()` backward jump → `saturating_sub` handles cleanly, no panic | `[ ]` | Clock-skew regression |
| 7.8.4 | `SEQ_SECONDS_PER_BLOCK=0` rejected at config parse (H8 regression) | `[x]` | Clap integration tests at §8.4.2 |

---

## 8. Startup / Bootstrap

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 8.1.1 | First boot, L1 reachable → discovers InputBox + genesis + chain_id from L1, writes bootstrap cache | `[?]` | Covered by normal e2e |
| 8.1.2 | First boot, L1 unreachable → returns error (`"L1 unreachable and no bootstrap cache"`) | `[ ]` | |
| 8.2.1 | Restart, L1 reachable → validates RPC chain_id against config before any DB write (H7 regression) | `[!]` | **H7 regression (RPC path) deferred** — see §6.5.1 |
| 8.2.2 | Restart, L1 unreachable, cache present → uses cache, validates cached chain_id | `[x]` | `restart_and_replay_test` + `chain_id_match_does_not_produce_mismatch_error` |
| 8.3.1 | Chain-id mismatch (config vs RPC) → `RunError::ChainIdMismatch`, no DB contamination | `[!]` | See §6.5.1 — cache-path test passes, RPC-path test deferred |
| 8.3.2 | Chain-id mismatch (config vs cache) → `RunError::ChainIdMismatch`, no DB contamination | `[x]` | **H7 regression (cache)**: `chain_id_mismatch_from_cache_returns_typed_error` |
| 8.4.1 | `SEQ_PREEMPTIVE_MARGIN_BLOCKS >= MAX_WAIT_BLOCKS` rejected at startup | `[ ]` | Runtime `assert!` — could be `#[should_panic]` test via full `run()` call; not yet written |
| 8.4.2 | `SEQ_SECONDS_PER_BLOCK=0` rejected by clap parser | `[x]` | **H8 regression**: `run_config_rejects_seconds_per_block_zero` + `run_config_accepts_seconds_per_block_one` + `run_config_default_seconds_per_block_is_12` in `runtime/config.rs` |
| 8.5.1 | Private-key parse failure does not echo key bytes in error (H3 regression) | `[x]` | **H3 regression**: `create_signer_provider_does_not_echo_key_bytes_on_invalid_hex` + `_on_odd_length` in `l1/provider.rs::tests` |
| 8.5.2 | `http://` URL for non-loopback host rejected (H4 regression) | `[x]` | **H4 regression**: `create_client_rejects_http_for_remote_host` |
| 8.5.3 | `http://127.0.0.1:8545` accepted (loopback exception) | `[x]` | `create_client_accepts_http_for_127_0_0_1` + `_for_localhost` + `_for_ipv6_loopback` (caught a bug in the H4 fix: bracket-wrapped IPv6 literal) |

---

## 9. Shutdown

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 9.1.1 | `runtime.stop()` drains pending user ops with explicit `Err(Unavailable)`; no silent drops | `[x]` | `shutdown_during_inflight_test` |
| 9.1.2 | Post-shutdown POST → 503 immediately (before consuming channel slot) | `[?]` | |
| 9.1.3 | Shutdown during batch submission: in-flight tx either completes or is abandoned cleanly | `[ ]` | Needs proxy or controlled timing |
| 9.1.4 | Shutdown during L1 input reader poll: reader exits cleanly, no corrupt safe-head state | `[ ]` | |

---

## 10. Application Trait Contract

Derived from the `Application Trait Contract` section in [`AGENTS.md`](../AGENTS.md).

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 10.1.1 | An input that executed successfully live MUST succeed on replay (catch-up) | `[ ]` | Property test: for all inputs accepted live, replay must accept |
| 10.1.2 | `AppError::Internal` during catch-up → lane crashes, sequencer fails to start | `[x]` | `catch_up.rs` error handling |
| 10.1.3 | `ExecutionOutcome::Invalid` during catch-up → skipped cleanly | `[x]` | |
| 10.2.1 | `validate_user_op` is pure: no mutations, no time dependence, no randomness | `[-]` | Enforced by code review; can't test directly |
| 10.2.2 | No state mutation from `current_user_nonce` or `current_user_balance` | `[-]` | Same |

---

## 11. Outage × Zone Matrix

The two primary failure dimensions: **who is offline** (sequencer or its RPC) and **how stale did L1 get during the outage** (pre-danger, danger, past-stale). Each cell needs a deterministic test. Use `--no-mining` + explicit `anvil_mine(N)` to hit zone boundaries exactly.

The danger threshold is `MAX_WAIT_BLOCKS - preemptive_margin`. With `MAX_WAIT_BLOCKS = 1200` and `preemptive_margin = 75` (default), boundaries are:
- **Pre-danger:** advance < 1125 blocks
- **Danger zone:** 1125 ≤ advance < 1200
- **Past-stale:** advance ≥ 1200

For deterministic tests, pick margins well inside each zone (e.g., 500 / 1150 / 1250).

### 11.1 Sequencer outage (anvil stays up, sequencer killed)

| # | Zone | Expected behavior | Status |
|---|------|-------------------|--------|
| 11.1.1 | Pre-danger (500) | No recovery. Sequencer resumes; pending batches submit normally. | `[x]` `sequencer_outage_pre_danger_no_recovery_test` |
| 11.1.2 | Danger zone (1150) | Preemptive recovery triggers. Flush runs (no-op if nothing pending). No cascade. Sequencer resumes. | `[x]` `sequencer_outage_danger_zone_no_cascade_test` |
| 11.1.3 | Past-stale, open batch (1250) | Open batch invalidated via `check_open_batch_staleness`. Recovery batch opened. Resume. | `[x]` `recovery_after_stale_batches_test` |
| 11.1.4 | Past-stale, closed+submitted batch (1250) | Closed batch invalidated via `detect_stale_and_cascade`. Recovery batch opened. Resume. | `[ ]` | Needs `--no-mining` (T2) to deterministically close + submit a batch before the outage |

### 11.2 Provider outage (proxy disconnects, sequencer stays up, anvil advances behind the proxy)

| # | Zone | Expected behavior | Status |
|---|------|-------------------|--------|
| 11.2.1 | Pre-danger (500) | Sequencer retries. Wall-clock estimate < threshold. Reconnect → sync, resume. | `[ ]` | Needs proxy |
| 11.2.2 | Danger zone (1150) | Wall-clock estimate enters danger zone. Recovery triggers. Flush blocks on proxy. Reconnect → flush completes → no cascade → resume. | `[ ]` | Needs proxy |
| 11.2.3 | Past-stale (1250) | Wall-clock estimate past stale. Recovery + flush block on proxy. Reconnect → flush + cascade. | `[x]` `provider_outage_past_stale_cascades_test` — stops sequencer, disconnects proxy, advances L1, verifies restart refuses while proxy is disconnected (wall-clock fallback past stale → `L1UnreachableInDangerZone`), then reconnects and verifies cascade |

### 11.3 Combined: outage both sides at once

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 11.3.1 | Sequencer stopped, proxy disconnected, anvil mines 1250 blocks, BOTH reconnect → recovery triggers correctly | `[x]` | Effectively covered by §11.2.3 — the "sequencer stopped + proxy disconnected" path is tested end-to-end there |

---

## 12. Storage Layer

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 12.1.1 | Schema CHECK constraints enforced: `safe_inputs.sender` length 20, `frames.fee >= 0`, XOR on `sequenced_l2_txs`, etc. | `[ ]` | One test per CHECK |
| 12.1.2 | FK cascade: deleting a `batches` row (should be impossible via PK) doesn't orphan children | `[-]` | Structural; writes are append-only |
| 12.2.1 | `valid_batches` correctly filters by `invalid_batches` | `[x]` | Implicit in recovery tests |
| 12.2.2 | `valid_batch_nonces` correctly filters | `[x]` | |
| 12.2.3 | `valid_sequenced_l2_txs` correctly filters | `[x]` | |
| 12.3.1 | Multi-statement writers wrap in `Immediate` transaction; partial failure leaves DB unchanged | `[?]` | |
| 12.3.2 | `trg_sequence_user_op` does not fire if outer user_ops INSERT rolls back | `[?]` | |
| 12.4.1 | Rowid pagination correctly skips invalidated rows via `valid_sequenced_l2_txs` view | `[x]` | Implicit in WS catch-up after recovery |

---

## 13. Fee Model

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 13.1.1 | `fee_to_linear(0) = 1`, `fee_to_linear(MAX_EXPONENT)` does not panic | `[x]` | `sequencer-core/src/fee.rs` unit tests |
| 13.1.2 | `fee_to_linear(MAX_EXPONENT + 1)` panics loudly (assert_eq message) | `[x]` | |
| 13.1.3 | `fee_from_linear(U256::MAX)` saturates to `MAX_EXPONENT` | `[x]` | |
| 13.1.4 | Round-trip `fee_from_linear(fee_to_linear(n))` within 1% | `[x]` | |
| 13.1.5 | `log_fee_ratio` handles `num < denom` via negation | `[x]` | |
| 13.2.1 | `batch_policy_derived.recommended_fee` clamps at `MAX_EXPONENT` at Rust read boundary | `[x]` | `query_batch_policy` test |
| 13.2.2 | High `log_gas_price` via `set_log_gas_price` → clamped, doesn't panic | `[x]` | `high_gas_price_clamps_recommended_fee_to_max_exponent` |
| 13.3.1 | `set_alpha` CHECK constraint rejects configs where `log_batch_size_target >= log_max_batch_bytes` | `[x]` | |
| 13.3.2 | `set_alpha(0, _)` or `set_alpha(_, 0)` panics with clear message | `[?]` | |

---

## 14. Out-of-scope under current tooling

Documented here so we are deliberate about what we *aren't* testing at the e2e level. These remain covered at the code-review + formal-verification level per the [threat model](../docs/threat-model/README.md) and [recovery spec](../docs/recovery/README.md).

| Threat | Why not e2e | Covered by |
|--------|-------------|-----------|
| Adversarial mempool: a previously-submitted tx lands long after we gave up | Anvil auto-mines everything in the mempool when `anvil_mine` is called; we cannot "hold" a specific tx indefinitely | TLA+ spec (157M states) + Part 6 code review |
| Replacement-by-nonce races | Same — we cannot model two builders racing | TLA+ + code review |
| Byzantine L1 / RPC (lying about events or `safe`) | Out of scope per threat model | Threat model + code review |
| Reorgs beyond safe depth | Anvil doesn't do reorgs | Threat model excludes |
| Timing side channels in WS feed | Timing attacks out of scope | Threat model excludes |
| DoS / resource exhaustion | Explicitly out of scope | Threat model excludes |

To cover the adversarial-mempool gap at e2e level we would need a **mock L1** with programmable inclusion logic (a custom JSON-RPC server that accepts txs but selectively mines them). Significant investment; not planned.

---

## Tooling dependencies

Coverage of the above requires the following test-harness additions. Each unlocks a row of the matrix:

| # | Tool | Unlocks | Status |
|---|------|---------|--------|
| T1 | TCP proxy with `disconnect()` / `reconnect()` | §11.2, §11.3, §7.7.7, §5.4 | `[x]` Built — `tests/harness/src/proxy.rs`; 6 unit tests; `ManagedSequencer::set_l1_endpoint_override` routes sequencer through it |
| T2 | Anvil `--no-mining` mode | §7.1.1, §7.1.3, §7.1.4, §11.1.4, §11.2.1, §11.2.2 (all cells with precise zone control) | `[ ]` Not built — would unlock closed-batch scenarios and finer-grained zone timing |
| T3 | Shorter poll intervals for tests (sub-second `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`) | Reduces raciness in §11, §7.7, §6 | `[ ]` Not built |
| T4 | `wait_for_recovery_complete` helper (poll a health / debug endpoint) | Replaces sleep-based waits throughout §11, §7 | `[ ]` Not built |
| T5 | Injectable failpoints (SQLite error, sub-transaction crash) | §7.2.2, §7.6.2 done; §7.6.3, §2.10.1 (H1) need more | `[?]` Partial — inline tests already induce some |
| T6 | Smaller `MAX_WAIT_BLOCKS` for test builds (optional optimization) | Shortens mine-1200-blocks tests | `[-]` Probably not needed — 1200 empty blocks mines in <1s |
| T7 | Direct `synced_at_ms` DB writer | §7.8.1, §7.8.2 — wall-clock-refuses-to-boot path (real seconds must elapse for the fallback to fire; anvil-mine doesn't count) | `[x]` `ManagedSequencer::rewind_synced_at_ms(ms_ago)` — rewrites the DB timestamp while the sequencer is stopped. `libfaketime`-free. Unblocks future wall-clock tests once a deterministic batch-close mechanism (T2) is available. |

---

## How to use this document

1. **Adding a test:** find the relevant row, flip `[ ]` to `[x]` when the test is written and passing.
2. **Adding a scenario:** add a new row under the relevant section. Include the status marker and one-line rationale.
3. **Before merging a bug fix:** find the scenario that should have caught it. If there isn't one, add it.
4. **Before a security review:** scan for `[!]` and `[?]` rows — these are the areas where confidence is weakest.
5. **For changes to tooling (T1-T6):** update the dependency table; flip status markers on unlocked rows.

## Relationship to other docs

- [`AGENTS.md`](../AGENTS.md) — architecture, invariants, coding conventions
- [`docs/threat-model/README.md`](../docs/threat-model/README.md) — what's in and out of scope
- [`docs/recovery/README.md`](../docs/recovery/README.md) — recovery design + TLA+ spec
- [`SECURITY_TODO.md`](../SECURITY_TODO.md) — open security findings
- This doc — what should be tested to gain confidence those invariants hold in practice
