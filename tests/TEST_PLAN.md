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
- §2.10.1 (H1 rusqlite leak) — needs failpoint injection (tool T5)
- (§6.5.1 / §8.3.1 (H7 RPC-path) closed by `tests/e2e` in commit `6f47b38`.)

**Phase 3 — Unit-test hygiene** (in progress):
- Shared `TestDb` / `temp_db` unified: `storage::test_helpers` promoted to `pub(crate)` and reused across 4 inline test modules; `sequencer/tests/common/mod.rs` added for integration tests. 6 local `temp_db` clones removed.
- `storage/recovery.rs`'s 38 flat tests split into 8 nested sub-modules (`invalid_batches`, `detect_and_recover`, `tip_staleness`, `check_danger_zone`, `check_any_unresolved`, `boundary`, `schema_invariants`, `tree_invariants`). Test names now self-locate (e.g. `tests::tip_staleness::open_batch_exactly_at_threshold_is_invalidated`).
- `sequencer-core/src/batch.rs` unit tests added (was zero tests): §1.4 SSZ roundtrip for `Batch`/`Frame`/`WireUserOp`, cross-call determinism, and §1.5 decode robustness (empty, below-header, truncated, invalid offset, garbage fuzz). 12 new tests.
- Stale markers cleaned: §1.4 `[?]`→`[x]`, §1.5 `[ ]`→`[x]`, §2.4.2 `[?]`→`[x]`, §2.7.1 `[ ]`→`[x]`, §5.1.1 `[?]`→`[x]`.

**SSZ library finding (Phase 3):** `ethereum_ssz::Decode::from_ssz_bytes` silently accepts trailing bytes after a valid `Batch` encoding. Not a security issue under our threat model (only the trusted batch-submitter sender is classified as `Batch` at L1; the scheduler also authenticates by msg_sender). Flagging for visibility: if any future path decodes a non-authenticated payload as `Batch`, this would need a pre-decode length check or a wrapper that enforces full-consumption. Referenced in §1.5 notes.

**Landed in Phase 3** (cumulative, unit-layer):
- §1.4, §1.5 — batch SSZ roundtrip + decode robustness (`sequencer-core/src/batch.rs`).
- §1.7 — S-malleability: malleable variant cannot recover a different address (alloy/k256 regression lock).
- §7.4.2, §7.4.3 — undrained safe input reaches recovery batch; empty recovery first frame. **Also covered at e2e in `6f47b38`** — both layers retained for defense in depth.
- §7.5.1 — first-batch-stale → nonce 0 reused after torn cascade. **Also covered at e2e in `6f47b38`.**
- §7.6.3 — post-`open_recovery_batch` crash → restart is no-op over persisted state.
- §7.7.4, §7.7.5 — flusher fee-bump and timeout helpers extracted + H5/H6 regression-locked.
- §8.4.1 — `preemptive_margin_blocks` validation extracted + `#[should_panic]` covered.

**Prioritized unit-layer gaps still open:**
- §7.2.2, §7.6 crash-atomicity rows — require failpoint injection (tool T5, not built).
- §7.7.7 — flusher survives extended provider outage (requires proxy tool, built for §11 but not wired here).

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
| 1.4 | SSZ encode a `Batch`, decode, re-encode → byte-identical | Unit (`sequencer-core/src/batch.rs::tests::ssz_roundtrip_*`) | `[x]` | Covers empty batch, populated batch, empty-user-ops frame, wire user op, and cross-call determinism |
| 1.5 | SSZ decode fails cleanly on truncated payload, garbage bytes, malformed offsets → returns `DecodeError`, never panics | Unit (`sequencer-core/src/batch.rs::tests::ssz_decode_*`) | `[x]` | Covers empty payload, sub-header lengths, truncated valid batch, invalid offset, and garbage-pattern fuzz. **Known library behavior:** `ethereum_ssz` silently accepts trailing bytes after a valid batch. Not a security issue under our threat model (only the trusted batch-submitter sender is classified as `Batch`), but worth noting if the scheduler side ever decodes a non-authenticated payload as `Batch`. |
| 1.6 | `MAX_WAIT_BLOCKS` constant is the same value on sequencer and scheduler sides at link time | Unit | `[x]` | Shared via `sequencer_core::MAX_WAIT_BLOCKS` — structural guarantee, no runtime check needed |
| 1.7 | S-malleability neutralized: signing the same op twice produces low-s and high-s forms; both recover the same sender | Unit (`sequencer/src/ingress/api.rs::tests::s_malleable_signature_cannot_recover_a_different_address`) | `[x]` | Constructs the malleable variant (`s' = n - s`, flipped parity) and asserts recovery either errors (EIP-2 rejection) or yields the same address. Regression lock against alloy/k256 behavioral drift. |

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
| 2.2.1 | Forged signature (valid format, wrong key) → 400 `INVALID_SIGNATURE`, no state change | `[x]` | `forged_signature_rejected_test` (e2e). **Note on status code**: observed contract is 400 `INVALID_SIGNATURE` for all signature-class rejections (not 422). Prior TEST_PLAN text said 422; updated to match reality. |
| 2.2.2 | Signature wrong hex length → 400 before crypto work | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_signature_with_wrong_hex_length` — passes a 4-byte signature (`0xdeadbeef`); rejection fires from `validate_hex_lengths` before any crypto runs. |
| 2.2.3 | Signature valid bytes, invalid parity byte → 400 `INVALID_SIGNATURE` | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_signature_with_invalid_parity_byte` — sends a 65-byte signature with the parity byte set to `0xFF`. Observed `"cannot recover sender"` path. Defensively asserts the rejection is *not* from the hex-length or payload-size gates. |
| 2.2.4 | Signature recovers a different address than claimed `sender` field → 400 `INVALID_SIGNATURE` | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_sender_claim_that_mismatches_signature_recovery` — key A signs the op, request claims sender is B; asserts `sender mismatch` + `INVALID_SIGNATURE` code. Complements the e2e `forged_signature_rejected_test` (which covers the full end-to-end shape including the empty WS); this one pins the direct API response. |

### 2.3 Body / format

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.3.1 | Body exceeds `max_body_bytes` (default 4 KB) → 413 before JSON parse | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_oversized_json_body_before_parsing` — uses a small `max_body_bytes` (256) to make the 413 trigger fast; asserts status `PAYLOAD_TOO_LARGE`. Regression for `DefaultBodyLimit` enforcement. |
| 2.3.2 | Body is not JSON → 400 with `"invalid JSON"` (H2 regression: must NOT leak serde internals) | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_malformed_json_as_bad_request` — sends a malformed body containing the bytes `0x1234`; asserts response message is exactly `"invalid JSON"` AND that `0x1234` does not appear in the body (no input echo). |
| 2.3.3 | Body is JSON but missing fields → 400, doesn't leak deserialization error text | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_json_with_missing_fields_using_fixed_envelope` — sends `{}`; parses the response envelope and asserts `message == "invalid JSON"` and `code == "BAD_REQUEST"`; sweeps for serde leak vocabulary (`"missing field"`, `"expected"`, `"deserializ"`, `"line "`, `"column "`). H2 regression. |
| 2.3.4 | Content-Type other than `application/json` → 400 with `"missing content type"` | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_missing_content_type_with_fixed_message` — sends a valid JSON body without the header; asserts the fixed `"missing content type"` envelope message. H2 regression. |
| 2.3.5 | User op `data` field exceeds `max_user_op_data_bytes` → 400 before signature verify | `[x]` | Two complementary tests: `api_rejects_user_op_payloads_above_application_limit` (oversized data + valid signature → 400 with `"user op payload too large"`, body echoes the limit) and `api_payload_size_check_fires_before_signature_recovery` (oversized data + correctly-shaped *garbage* signature → still gets the size-class error, never a signature error — proves the validation order in `validate_payload_size` runs before `recover_sender`, so signature recovery isn't a DoS amplifier on huge bodies). |

### 2.4 Nonce rules

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.4.1 | First tx with nonce 0 → accepted, next expected becomes 1 | `[x]` | `deposit_transfer_withdrawal_test` |
| 2.4.2 | Tx with nonce too low (e.g., replay) → 422 `InvalidNonce`, no state change | `[x]` | `rejected_user_op_not_broadcast_test` |
| 2.4.3 | Tx with nonce too high (gap) → 422 `InvalidNonce`, no state change | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_user_op_with_nonce_gap` — submits nonce 7 when the expected nonce is 0; asserts 422 + nonce-class message. Complement to §2.4.2 (nonce too low); together they pin strict-equality on `current_user_nonce`. |
| 2.4.4 | `InvalidNonce` response does NOT get broadcast on WS | `[x]` | `rejected_user_op_not_broadcast_test` |

### 2.5 Fee rules (V3 regression)

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.5.1 | `max_fee < current_frame_fee` → 422 `InvalidMaxFee` | `[x]` | `fee_below_minimum_rejected_test` |
| 2.5.2 | `max_fee == current_frame_fee` → accepted (boundary) | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_accepts_user_op_with_max_fee_equal_to_current_frame_fee` — submits `max_fee = 1060` (exactly the bootstrapped frame's fee); asserts 200. Paired with §2.5.1 (`fee_below_minimum_rejected_test`), pins the comparator as strict `<` (not `<=`). |
| 2.5.3 | Rejection handled by trait-default `validate_and_execute_user_op` (V3 regression) | `[x]` | Unit test in `app-core/wallet.rs` |

### 2.6 Balance rules

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.6.1 | `balance < fee_to_linear(current_fee)` → 422 `InsufficientGasBalance`, no state change | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_rejects_user_op_when_balance_below_gas_cost` — fresh signer with no deposit (balance = 0) submits a user-op; asserts 422 + `"insufficient balance for gas"` (the `InvalidReason::InsufficientGasBalance` Display text from `sequencer_core::application`). Exercises `WalletApp::validate_user_op`'s balance check in app-core. |
| 2.6.2 | Rejected op does NOT broadcast | `[x]` | Covered indirectly by `rejected_user_op_not_broadcast_test` (e2e) which asserts the WS no-message-after-reject invariant on the bad-nonce variant. The broadcast filter in the lane is rejection-class-agnostic (any `SequencerError` rejection path → no WS event), so bad-nonce coverage applies to the insufficient-gas path too. A dedicated insufficient-gas test would add belt-and-suspenders and could land alongside §2.6.1. |

### 2.7 Admission control

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.7.1 | Queue full → `429 OVERLOADED` with body `"queue full"` | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_returns_429_when_queue_is_full` |
| 2.7.2 | Queue-full response does not leak per-sender info | `[ ]` | Hardening |

### 2.8 Concurrency

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.8.1 | Two concurrent POSTs for same (sender, nonce) → exactly one admitted, one gets `InvalidNonce` | `[x]` | `concurrent_user_ops_test` |
| 2.8.2 | Rejected concurrent op produces no state change | `[x]` | `sequencer/tests/e2e_sequencer.rs::api_concurrent_same_nonce_leaves_exactly_one_committed` — two `tokio::spawn`-ed POSTs with byte-identical bodies (same sender, same nonce) join concurrently; asserts exactly one 200 + one 422 with a nonce-class message. Complements `concurrent_user_ops_test` (distinct-sender happy path, at e2e) by pinning the rejected-branch outcome specifically. |

### 2.9 Shutdown semantics

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 2.9.1 | Mid-request shutdown: in-flight requests get 503 or clean error | `[x]` | `shutdown_during_inflight_test` |
| 2.9.2 | Post-shutdown POST → 503 immediately | `[x]` | `sequencer/src/ingress/api.rs::tests::submit_tx_rejects_when_shutdown_has_started` — requests shutdown on the `ShutdownSignal`, then submits; asserts `StatusCode::SERVICE_UNAVAILABLE` with code `UNAVAILABLE`. |

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
| 3.2.3 | Frame fee stays fixed for the frame's lifetime even if policy is updated mid-frame | `[x]` | `storage/ingress.rs::tests::frame_fee_is_immutable_for_the_lifetime_of_the_frame` — opens a frame at default fee (1060), calls `set_log_gas_price(100)` mid-frame (derived policy now recommends 1160), asserts the open frame's persisted `frames.fee` is still 1060 AND the `WriteHead.frame_fee` mirror is stable; then closes the frame and asserts the *next* frame opens at 1160 (policy flows in at close). Regression for "frames.fee immutable" invariant. |

### 3.3 Batch closure

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 3.3.1 | Batch closes when `max_batch_user_op_bytes` target is reached | `[x]` | `batch_closes_when_max_user_op_bytes_is_reached` |
| 3.3.2 | Batch closes when deadline (`max_open_time`) elapses | `[x]` | `batch_closes_when_max_open_time_is_reached` |
| 3.3.3 | Closed batch becomes eligible for nonce assignment | `[x]` | `storage/l1_submission.rs::tests::closed_batch_becomes_eligible_for_submission_with_assigned_nonce` — asserts `load_pending_batches(0)` is empty before close and returns `[batch_index=0, nonce=0]` after `close_frame_and_batch`; also asserts the new open Tip (batch 1) is NOT eligible. Pins the open→closed→eligible transition + the genesis nonce invariant. |

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
| 4.1.3 | Subscribe `from_offset=future` → waits for new events, doesn't error | `[x]` `ws_subscribe_from_future_offset_waits_silently_test` | Pins the contract: subscribe with offset well beyond current head succeeds, delivers nothing until an event with a greater offset arrives. Consistent with `from_offset=0` on an empty head — we don't want the wait-for-new-events path to differ based on whether history happens to exist. |

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
| 4.4.2 | Reconnect after a cascade at a previously-observed offset that got invalidated → cursor delivers only post-recovery events. Complement to §4.4.1: that test reconnects at `from_offset=0` (trivial walk of the valid view); this tests the non-zero case where the client's last-seen offset is *itself* now hidden by `valid_sequenced_l2_txs`. A WS connection can't span invalidation — the sequencer exits (DangerZone or stop) first and the socket dies — so the scenario is specifically "client had last_seen=N before the break, reconnects at N post-recovery, query `WHERE offset > N` against the valid view skips cleanly past N". | `[x]` `ws_reconnect_at_invalidated_offset_skips_cleanly_test` | Captures the transfer's offset pre-cascade, reconnects at that offset post-recovery, asserts (a) delivered event's offset is strictly greater and (b) reconnect-at-invalidated matches reconnect-at-zero. |

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
| 5.1.1 | `InputAdded` event at safe block N → row in `safe_inputs` with block_number=N | `[x]` | Covered by `deposit_transfer_withdrawal_test` (deposit e2e) |
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
| 5.4.1 | Transient `Provider` error → reader retries, does not crash | `[x]` `provider_outage_input_reader_retries_after_reconnect_test` | Routes through T1 proxy. Disconnect → deposit on L1 (bypasses the proxy) → mine 20 blocks for safe depth → reader keeps retrying with connection errors for ≥5 s (`observe_for` asserts no exit) → reconnect → reader pulls the backlog → WS delivers the deposit event. |
| 5.4.2 | Provider times out → reader logs and retries | `[x]` | Covered by the same test — T1's `disconnect()` simulates any provider failure mode (connection refused / closed socket / pending read timeout); at e2e level there's no clean way to distinguish a refused connection from a timeout, and the retry path is identical. |
| 5.4.3 | Storage error during insert → reader fails loudly (fail-stop) | `[ ]` | |

### 5.5 Long-range partition

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 5.5.1 | Range that triggers `SEQ_LONG_BLOCK_RANGE_ERROR_CODES` splits in half, both halves succeed | `[ ]` | Not cheaply testable at e2e: the proxy (T1) is a dumb TCP pass-through and can't selectively error based on RPC params / block-range size. Clean coverage would need either an HTTP-inspecting proxy (substantial new tooling) or a mock `Provider` (alloy's trait surface is large; non-trivial scaffolding) or a closure-refactor of `get_input_added_events` (production-code change for testability). The interesting logic — error-code matching in `error_message_matches_retry_codes` — is already unit-tested; the recursion itself is a standard bisect over that predicate. Low regression risk without dedicated coverage. |
| 5.5.2 | Range splits down to 1 block and still fails → bubbles up cleanly | `[ ]` | Same blocker as §5.5.1. Covered by inspection: the termination condition `if start_block >= end_block { return Err(...) }` in `get_input_added_events` is a 3-line bisect guard. |

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
| 6.5.1 | Sequencer configured with `--chain-id=X`, RPC returns Y → startup returns `RunError::ChainIdMismatch`, no panic, no DB writes | `[x]` Covered at e2e level by `chain_id_mismatch_via_live_rpc_refuses_boot_test` (see §8.2.1). The `tests/e2e/` harness's deployed-InputBox setup is what made this feasible. |
| 6.5.2 | L1 unreachable at startup with cache present, cached chain_id matches config → boots | `[x]` | Positive control in `chain_id_match_does_not_produce_mismatch_error` |
| 6.5.3 | L1 unreachable at startup with cache present, cached chain_id differs → returns `RunError::ChainIdMismatch`, no panic | `[x]` | **H7 regression (cache path)**: `chain_id_mismatch_from_cache_returns_typed_error` |

---

## 7. Recovery Procedure (CRITICAL)

The largest and most sensitive section. The open-batch bug demonstrates that design gaps here have silent-corruption consequences. Every transition in the recovery state machine needs a test.

### 7.1 Detection paths

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.1.1 | Frontier batch (nonce-bearing, closed, accepted) crosses `MAX_WAIT_BLOCKS` by inclusion staleness → cascade-invalidated on next check | `[-]` | Scoped out: the unique submitter-side path (live `check_danger_zone` firing on a closed-in-danger batch) is already covered by §7.3.5. The *other* unique path — `populate_safe_accepted_batches_inner`'s inclusion-stale skip (the `batch_age_is_stale` continue) — has unit coverage and is hard to exercise e2e: Anvil's `anvil_mine(N)` includes any pending tx in the first mined block, so you can't mine empty blocks past a held mempool tx. Also, the submitter's live-exit path is gated by `wait_for_confirmations`'s 24–72 s timeout (hard-coded against ETHEREUM_BLOCK_TIME_SECS, not config-tunable). Would become cheap if that timeout became test-configurable (T3-adjacent). |
| 7.1.2 | Open batch (not yet closed) crosses `MAX_WAIT_BLOCKS` by current staleness → cascade-invalidated | `[x]` | `recovery_after_stale_batches_test` (**the bug we caught**) |
| 7.1.3 | Batch in danger zone but not yet stale → flush triggers, but no cascade | `[ ]` | See §11 zone matrix |
| 7.1.4 | Batch pre-danger-zone → no flush, no cascade | `[ ]` | See §11 zone matrix |

### 7.2 Cascade invalidation

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.2.1 | Stale batch N cascades to all batches with `batch_index >= N` | `[x]` | `storage/recovery.rs` unit tests |
| 7.2.2 | Cascade is a single atomic SQL transaction; crash mid-cascade leaves DB unchanged | `[ ]` | Needs failpoint injection |
| 7.2.3 | `valid_*` views hide invalidated batches immediately after cascade | `[x]` | Covered by inline tests |
| 7.2.4 | Nonce reuse works automatically via parent-pointer (new Tip's `parent.nonce + 1` equals the invalidated suffix's first nonce) | `[x]` | Covered by `detect_and_recover_does_not_false_match_after_nonce_reuse`, `nonce_reuse_after_cascade_with_valid_ancestor`, `nonce_is_reused_after_torn_cascade` |

### 7.3 Open-batch-only case (NEW regression zone — V4 + open-batch fix)

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.3.1 | Sequencer stops before batch closure, L1 advances past MAX_WAIT_BLOCKS, restart invalidates open batch | `[x]` | `recovery_after_stale_batches_test` (e2e) + `open_batch_stale_by_current_safe_block_is_invalidated` (unit) |
| 7.3.2 | Same scenario with NO direct inputs pending → recovery batch opens, empty frame | `[x]` | Implicit in `open_batch_stale_by_current_safe_block_is_invalidated` (no deposits seeded) |
| 7.3.3 | Closed-and-nonced batch stale + open batch also stale → both in one cascade | `[x]` | `closed_unsubmitted_stale_and_open_stale_both_cascade` |
| 7.3.4 | `check_open_batch_staleness` returns `None` when open batch is NOT stale → no false positive cascade | `[x]` | **Critical negative test**: `open_batch_not_yet_stale_is_not_invalidated` + boundary tests (`open_batch_exactly_at_threshold_is_invalidated`, `open_batch_one_block_below_threshold_is_not_invalidated`) |
| 7.3.5 | **Aging Tip while sequencer is UP and L1 is reachable**: Tip ages past `danger_threshold` without crossing `MAX_WAIT_BLOCKS`. Submitter's zombie check (closed-only) must NOT trigger shutdown loop; Tip closes/invalidates by natural policy; no doomed soft confirmations are issued. Closes the gap the schema refactor was designed to prevent. | `[x]` `aging_open_tip_tolerated_by_zombie_check_test` | Decoupled L1/wall-clock advance: `mine_l1_blocks(1150)` jumps L1 into the danger zone while the wall clock stays put so the Tip remains open. `observe_for(8s)` asserts the sequencer keeps running (would catch any regression that unifies the zombie check across open + closed batches). Then `set_faketime_offset("+7500s")` (past `DEFAULT_MAX_BATCH_OPEN` = 7200s) forces the inclusion lane's natural time-based close; submitter's next tick exits with `DangerZone`. Asserts `counts.invalidated == 0` (danger zone, below MAX_WAIT → no cascade). |

### 7.4 Re-drain direct inputs

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.4.1 | Direct input was drained into invalidated batch → re-drained into recovery batch | `[x]` | `recovery_redrains_direct_inputs_and_replay_sees_them_once` |
| 7.4.2 | Direct input that was already safe but NOT yet drained → included in recovery batch's first frame | `[x]` | **e2e:** `recovery_drains_safe_but_undrained_direct_input_test` — stops the sequencer before any user activity, deposits on L1 (bypasses the sequencer's process), advances past MAX_WAIT. Respawn's startup recovery syncs safe head, sees the previously-invisible deposit in `safe_inputs`, cascades the aged empty initial Tip, opens a recovery batch whose `leading_range` includes the never-drained deposit. Distinct from §7.4.1 (`recovery_after_stale_batches_test`), which re-drains an already-drained-into-invalidated-batch input. **Unit:** `storage/recovery.rs::tests::tip_staleness::undrained_safe_input_appears_in_recovery_batch_first_frame` — covers the same recovery-drain branch via direct Storage-layer setup (no harness/Anvil). |
| 7.4.3 | No direct inputs pending → recovery batch opens empty | `[x]` | **e2e:** `recovery_batch_opens_empty_when_no_direct_inputs_pending_test` — negative control for §7.4.2: same shape, no L1 deposit. `leading_range = [0, 0)` → recovery batch's first frame is empty → WS(0) sees nothing. Cascade still fires on the aged empty initial Tip. **Unit:** `storage/recovery.rs::tests::tip_staleness::recovery_batch_opens_empty_when_no_direct_inputs_pending`. |
| 7.4.4 | A subscriber seeing events across recovery sees each direct input exactly once | `[x]` | Implicit in 7.4.1 |

### 7.5 Nonce-0 edge case

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.5.1 | First-ever batch (nonce 0) goes stale before any batch reaches Gold → recovery invalidates and opens fresh batch 0 | `[x]` | **e2e:** `nonce_zero_recovery_invalidates_then_accepts_at_nonce_zero_test` — uses T2 (auto-mining off + drop) to ensure the first-ever batch's L1 submission never lands. Cascade fires → recovery batch opens with `parent_batch_index = NULL` and reused `nonce = 0`. Structural invariants (NULL parent → nonce 0, contiguous valid-path nonces) verified by post-test `assert_schema_invariants`. **Unit:** `storage/recovery.rs::tests::tip_staleness::first_batch_stale_recovery_reuses_nonce_zero` — asserts the same `nonce = 0` / `parent_batch_index = NULL` invariants directly at the Storage layer via raw SQL. |
| 7.5.2 | After 7.5.1, scheduler accepts the recovery batch at nonce 0 (nonce space reused) | `[x]` | Same e2e test as §7.5.1 — drives 150 transfers into the recovery batch to size-trigger close + submit, then explicitly mines L1 blocks for confirmations. Asserts `safe_accepted_batches` has a row with `MIN(nonce) = 0` — proving `populate_safe_accepted_batches_inner` accepts a reused-nonce batch after cascade. |

### 7.6 Idempotency & crash-safety

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.6.1 | Run `detect_and_recover` twice on the same state → second run is no-op | `[x]` | `detect_and_recover_is_idempotent` |
| 7.6.2 | Crash AFTER cascade INSERT but BEFORE `open_recovery_batch_in_tx` → on restart, a recovery batch is opened (torn state) | `[x]` | `detect_and_recover_opens_batch_after_torn_invalidation` |
| 7.6.3 | Crash AFTER open_recovery_batch → restart finds valid open batch, does nothing | `[x]` | `storage/recovery.rs::tests::tip_staleness::detect_and_recover_after_post_recovery_crash_is_no_op` — drops Storage between calls to model a restart over the persisted DB. Distinct from §7.6.1's back-to-back same-handle idempotence. |
| 7.6.4 | The entire recovery procedure (populate + detect + open) runs in a single `Immediate` transaction | `[x]` | Structural, verified by reading |
| 7.6.5 | `populate_safe_accepted_batches` is resumable (cursor-tracked, `INSERT OR IGNORE`) | `[x]` | |
| 7.6.6 | Nonce assignment is structural (not a discrete step); `insert_new_batch` derives nonce from `parent.nonce + 1` at creation time | `[x]` | `trg_enforce_nonce_contiguity` verifies; `schema_rejects_bad_nonce_contiguity` covers the trigger path |

### 7.7 Mempool flusher

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.7.1 | Pending wallet-nonce slot → flusher submits a no-op that consumes the slot | `[x]` | Existing Anvil-backed flusher tests |
| 7.7.2 | No pending slots → flush is instant no-op | `[x]` | |
| 7.7.3 | Flusher no-op competes with a batch tx at the same nonce; one of them lands, slot is consumed | `[x]` | |
| 7.7.4 | Flusher fee bump satisfies Ethereum's ≥10% replacement rule (H5 regression) | `[x]` | Extracted `bumped_replacement_fees()` helper in `recovery/flusher.rs`; covered by `replacement_fee_bump_exceeds_ten_percent_for_max_fee`, `replacement_fee_bump_doubles_priority_fee`, `replacement_fee_floor_is_positive_even_when_base_is_zero`, `replacement_fee_bump_saturates_at_u128_max`. |
| 7.7.5 | Flusher `confirmation_timeout` derives from `seconds_per_block` config (H6 regression) | `[x]` | Extracted `derive_timeouts()` helper; covered by `timeouts_derive_from_seconds_per_block` (tests 1/2/12 s/block) and `confirmation_timeout_is_ten_times_safe_poll_interval` (structural invariant). |
| 7.7.6 | Flusher outer loop runs without timeout; inner watch-timeout re-enters the loop | `[x]` | Verified in review |
| 7.7.7 | Flusher survives extended provider outage — retries forever, completes when provider returns | `[x]` | `sequencer/src/recovery/flusher.rs::tests::flush_surfaces_provider_error_under_disconnect_and_completes_on_reconnect` — spawns a `TcpProxy` (from `rollups-harness`, added as sequencer dev-dep) in front of Anvil; seeds pending wallet-nonce state; disconnects proxy and asserts `flush_and_wait` returns `FlushError::Provider` fast (no internal retry); reconnects proxy + starts mining; asserts a fresh flusher call completes and the nonce-0 slot reaches safe. **Implementation note pinned by the test**: `flush_and_wait` does NOT retry internally; "retries forever" in this row is the *orchestrator restart loop* (covered at e2e by §11.1.5 / §11.2.2-followup's `respawn_until_stable`). This test pins the flusher's error surface under disconnect + its completion on reconnect — the two ends of what the orchestrator is looping over. |

### 7.8 Wall-clock fallback

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 7.8.1 | L1 unreachable, elapsed wall time estimates `missed_blocks > danger_threshold` → recovery triggers | `[x]` | `provider_outage_wall_clock_refuses_boot_test` in `tests/e2e`. Validated end-to-end: proxy disconnected → `anvil_mine(1500)` + `faketime '+5h'` → respawn fails with `L1UnreachableInDangerZone` → proxy reconnect + respawn succeeds + cascade fires. Migrated from the now-removed `rewind_synced_at_ms` helper to faketime. |
| 7.8.2 | `l1_safe_head.synced_at_ms == 0` (never synced) → treat as danger zone, return `L1UnreachableInDangerZone` error | `[x]` `first_boot_l1_unreachable_never_synced_refuses_boot_test` | Normal boot seeds the bootstrap cache; `ManagedSequencer::reset_l1_safe_head_synced_at_ms` then rewrites `synced_at_ms` to 0 on disk while the sequencer is stopped. Respawning with the proxy disconnected triggers the wall-clock fallback's `synced_at_ms == 0` branch → `L1UnreachableInDangerZone`. Scope limit: the separate "truly first-ever boot (no bootstrap cache)" path is tested elsewhere; this one pins the wall-clock branch specifically. |
| 7.8.3 | `SystemTime::now()` backward jump → `saturating_sub` handles cleanly, no panic | `[x]` | `wall_clock_backward_jump_no_panic_test` in `tests/e2e`. Uses `faketime '-1h'` with proxy disconnected to force the wall-clock-fallback path with `now < last_sync_ms`. |
| 7.8.4 | `SEQ_SECONDS_PER_BLOCK=0` rejected at config parse (H8 regression) | `[x]` | Clap integration tests at §8.4.2 |

---

## 8. Startup / Bootstrap

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 8.1.1 | First boot, L1 reachable → discovers InputBox + genesis + chain_id from L1, writes bootstrap cache | `[?]` | Covered by normal e2e |
| 8.1.2 | First boot, L1 unreachable → returns error (`"L1 unreachable and no bootstrap cache"`) | `[x]` `first_boot_no_cache_l1_unreachable_refuses_boot_test` | Distinct from §7.8.2 (wall-clock fallback): this hits the *earlier* `InputReader::new` discovery step. Harness `clear_l1_bootstrap_cache` empties the cache table after a normal boot; respawn through a disconnected proxy hits the no-cache + L1-unreachable code path. Verifies reversibility: reconnect proxy, respawn succeeds. |
| 8.2.1 | Restart, L1 reachable → validates RPC chain_id against config before any DB write (H7 regression) | `[x]` `chain_id_mismatch_via_live_rpc_refuses_boot_test` | **H7 regression (RPC path).** Spawns the full sequencer binary against real Anvil with mismatched `--chain-id` (override on `ManagedSequencer`); asserts respawn fails with `RunError::ChainIdMismatch`. Reset-to-correct-chain-id respawn succeeds — proves the failed attempt didn't poison the bootstrap cache. Complements the cache-path test in `sequencer/tests/chain_id_validation.rs`. |
| 8.2.2 | Restart, L1 unreachable, cache present → uses cache, validates cached chain_id | `[x]` | `restart_and_replay_test` + `chain_id_match_does_not_produce_mismatch_error` |
| 8.3.1 | Chain-id mismatch (config vs RPC) → `RunError::ChainIdMismatch`, no DB contamination | `[x]` Same test as §8.2.1 — `chain_id_mismatch_via_live_rpc_refuses_boot_test` covers both since they're the same code path with different framings. |
| 8.3.2 | Chain-id mismatch (config vs cache) → `RunError::ChainIdMismatch`, no DB contamination | `[x]` | **H7 regression (cache)**: `chain_id_mismatch_from_cache_returns_typed_error` |
| 8.4.1 | `SEQ_PREEMPTIVE_MARGIN_BLOCKS >= MAX_WAIT_BLOCKS` rejected at startup | `[x]` | Validation extracted to `runtime::compute_danger_threshold` and covered by `runtime::tests::margin_equal_to_max_wait_panics`, `margin_greater_than_max_wait_panics`, plus positive-control tests for 0, default (75), and just-below-max-wait. |
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
| 10.1.1 | An input that executed successfully live MUST succeed on replay (catch-up) | `[x]` `replay_matches_live_for_mixed_workload_test` | Diverse multi-sender workload (Alice/Bob/Charlie, two interleaved deposits, transfers in both directions, two withdrawals). Post-restart WS catch-up assembles a fresh replay; test asserts per-user balance + nonce + executed-input-count equality against the live replay. Any Application non-determinism or catch-up bug diverges the two replays immediately. Complements `restart_and_replay_test` (narrower single-sender workload, implicit equality). |
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
| 11.1.2 | Danger zone (1150), decoupled wall clock | Narrow: only L1 advances; wall clock stays put. No closed batch past frontier is stale → no flush, no cascade, sequencer resumes. | `[x]` `sequencer_outage_danger_zone_no_cascade_test`. Uses `mine_l1_blocks` directly (no wall-clock advance) because coupled advance triggers the aged-Tip-auto-close → flush-cycle path covered by §11.1.5 below. |
| 11.1.3 | Past-stale, open batch (1250) | Open batch invalidated via staleness check. Recovery batch opened. Resume. | `[x]` `recovery_after_stale_batches_test`. Uses `advance_wall_and_mine` — coupled wall-clock+L1 advance models real outage semantics. |
| 11.1.4 | Past-stale, closed+submitted batch (1250) | Closed batch invalidated. Recovery batch opened. Resume. | `[x]` `delayed_inclusion_cascades_on_restart_test` | Uses T2. Setup: deposit + 150 transfers force a size-triggered batch close while auto-mining is disabled, so the submitter's L1 tx lands in a held mempool. Stop sequencer → `drop_all_pending_txs` → `advance_wall_and_mine(1250 * 12s)` (genuinely empty blocks since mempool is empty) → re-enable auto-mining → respawn. Startup recovery detects the closed batch is past `MAX_WAIT_BLOCKS` and cascades; flush runs against the (now live) auto-miner. WS replay asserts the transfers are rolled back. |
| 11.1.5 | Danger zone (1150), **coupled wall+L1 advance** | Realistic: outage advances both L1 and wall clock. On respawn the aged Tip auto-closes, the resulting closed batch IS in danger, submitter triggers flush+shutdown, orchestrator restarts, post-flush recovery completes, sequencer is healthy. | `[x]` `sequencer_outage_danger_zone_coupled_restart_cycle_recovers_test` — drives the full orchestrator loop via `respawn_until_stable` (T8). First respawn exits with `DangerZone` after the aged Tip closes; each retry advances L1 by ~100 blocks (~20 min) until the closed batch ages past `MAX_WAIT_BLOCKS` and startup recovery cascades. Asserts the loop requires at least two attempts (not a cheap no-op) and that a cascade-invalidation actually fired. |

### 11.2 Provider outage (proxy disconnects, sequencer stays up, anvil advances behind the proxy)

| # | Zone | Expected behavior | Status |
|---|------|-------------------|--------|
| 11.2.1 | Pre-danger (500), sequencer stays UP, load applied | Sequencer retries. Wall-clock estimate < threshold. Inclusion lane continues accepting user ops **and closes batches by size**. Reconnect → sync, resume. | `[x]` `provider_outage_pre_danger_sequencer_continues_test` — submits ~150 transfers during the outage, asserts `count_batches().sealed` strictly increased. |
| 11.2.2 | Danger zone (3h55min), sequencer UP, self-exits | Running sequencer's wall-clock fallback detects danger mid-run → exits with `DangerZone`. Startup wall-clock fallback refuses subsequent boot while proxy still disconnected. No invalidation (not past-stale). | `[x]` `provider_outage_danger_zone_sequencer_self_exits_test` — uses dynamic faketime (file-based) to shift the running sequencer's clock into the danger zone without a respawn. Stops at the "refuse to reboot" assertion. |
| 11.2.2-follow-up | Danger zone → mid-run exit → reconnect → restart cycle | Completes §11.2.2: proxy reconnects, `respawn_until_stable` drives the orchestrator loop (advancing L1 each retry) until the aged closed batch crosses `MAX_WAIT_BLOCKS` and cascade fires. Asserts Stable convergence + cascade-invalidation. | `[x]` `provider_outage_danger_zone_mid_run_exit_then_restart_cycle_recovers_test` — uses T8 (`respawn_until_stable`). |
| 11.2.3 | Past-stale (1250) | Wall-clock estimate past stale. Recovery + flush block on proxy. Reconnect → flush + cascade. | `[x]` `provider_outage_past_stale_cascades_test` — stops sequencer, disconnects proxy, advances L1, verifies restart refuses while proxy is disconnected (wall-clock fallback past stale → `L1UnreachableInDangerZone`), then reconnects and verifies cascade |

### 11.3 Combined: outage both sides at once

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 11.3.1 | Sequencer stopped, proxy disconnected, anvil mines 1250 blocks, BOTH reconnect → recovery triggers correctly | `[x]` | Effectively covered by §11.2.3 — the "sequencer stopped + proxy disconnected" path is tested end-to-end there |
| 11.3.2 | Both stopped, advance to danger zone, then turn on sequencer ONLY (proxy still disconnected) | `[x]` `both_down_danger_zone_sequencer_first_refuses_boot_test` | Realistic datacenter-outage-recovery scenario: sequencer boots while L1 is still unreachable, wall-clock fallback sees past-danger → `L1UnreachableInDangerZone`. Stops at the refuse-boot assertion (no cascade yet — we're below MAX_WAIT). Complement to §11.2.3 in the danger-zone window instead of past-stale. |
| 11.3.3 | Both stopped, advance to danger zone, proxy returns FIRST (sequencer still down), then sequencer → normal sync, startup sees aged batches and handles them | `[x]` `both_down_danger_zone_proxy_first_restart_cycle_recovers_test` | Tests the "L1 recovered before us" reconnect ordering. Uses T8: first respawn exits with `DangerZone` after the aged Tip closes, `respawn_until_stable` advances L1 by 100 blocks per retry until cascade fires on a subsequent respawn. |

### 11.4 Short-duration provider hiccups (heal-within-pre-danger)

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 11.4.1 | Sequencer running, proxy disconnects for a few seconds (pre-danger), reconnects. Sequencer retries, resumes without any recovery action. | `[x]` `provider_outage_short_hiccup_no_recovery_test` | Most-common production fault — RPC flaked briefly, retry succeeded. Disconnect lasts ≥1 submitter poll interval (6s) with zero L1/wall-clock advance, then reconnects; asserts POST /tx keeps working and no batch gets invalidated. Complement to §11.2.1 (load-under-outage); this covers the "pure retry loop" path with no wall-clock pressure. |

---

## 12. Storage Layer

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 12.1.1 | Schema CHECK constraints enforced: `safe_inputs.sender` length 20, `frames.fee >= 0`, XOR on `sequenced_l2_txs`, etc. | `[x]` | `storage/recovery.rs::tests::schema_invariants::schema_rejects_*` — six new tests exercise CHECK-level refusals: `safe_input_with_wrong_sender_length`, `user_op_with_wrong_sender_length`, `user_op_with_wrong_signature_length`, `sequenced_l2_tx_with_neither_xor_branch`, `l1_bootstrap_cache_with_zero_chain_id`, `safe_input_with_negative_block_number`. Each asserts `CHECK constraint failed` specifically (not a trigger/FK/NOT NULL error). |
| 12.1.2 | FK cascade: deleting a `batches` row (should be impossible via PK) doesn't orphan children | `[-]` | Structural; writes are append-only |
| 12.2.1 | `valid_batches` correctly filters by `invalidated_at_ms IS NULL` | `[x]` | Implicit in recovery tests |
| 12.2.2 | `valid_closed_batches` correctly filters (sealed + valid) | `[x]` | Submitter pending-batch load covers it |
| 12.2.3 | `valid_sequenced_l2_txs` correctly filters | `[x]` | |
| 12.2.4 | `valid_open_batch` has at most one row (partial unique index `ux_single_valid_tip`) | `[x]` | `schema_rejects_second_valid_tip` |
| 12.2.5 | Schema triggers reject: bad nonce, re-seal, re-invalidate, writes to non-Tip, parent mutation | `[x]` | `schema_rejects_*` test group |
| 12.3.1 | Multi-statement writers wrap in `Immediate` transaction; partial failure leaves DB unchanged | `[?]` | |
| 12.3.2 | `trg_sequence_user_op` does not fire if outer user_ops INSERT rolls back | `[?]` | |
| 12.4.1 | Rowid pagination correctly skips invalidated rows via `valid_sequenced_l2_txs` view | `[x]` | Implicit in WS catch-up after recovery |

### 12.5 Parent-pointer tree invariants (NEW)

| # | Scenario | Status | Notes |
|---|----------|--------|-------|
| 12.5.1 | **Tree integrity property test**: for a mixed workload (opens, closes, partial/torn cascades), every valid batch satisfies `nonce = parent.nonce + 1`, `parent_batch_index` is NULL (genesis) or references an existing batch, and parent-walk terminates within `batch_index` hops. | `[x]` | `tree_invariants_hold_across_mixed_workload` in `storage/recovery.rs` tests. |
| 12.5.2 | **Subtree equivalence**: among *valid* batches, `{batch_index >= N}` equals the subtree rooted at N via recursive `parent_batch_index` walk. Documents the equivalence the cascade query relies on. | `[x]` | `subtree_by_batch_index_equals_subtree_by_parent_walk`. If this ever diverges, cascade must switch to recursive CTE. |
| 12.5.3 | **Post-e2e schema invariants**: after each passing e2e test, harness-side DB inspection asserts at most one `valid_open_batch` row, `nonce = parent.nonce + 1` across all batches, contiguous valid-path nonces, and no FK orphans. | `[x]` | `ManagedSequencer::assert_schema_invariants` wired into `tests/e2e/src/main.rs` as a post-scenario step. Harness-only; no sequencer changes. |

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
| T2 | Runtime toggle of Anvil's auto-mining + mempool drop | §11.1.4 (done); §7.1.1, §7.1.3, §7.1.4 (pending — live-runtime variants) | `[x]` `ManagedSequencer::set_automine(bool)` (via `anvil_setAutomine`) holds or releases the mempool without respawning Anvil; `drop_all_pending_txs` (via `anvil_dropAllTransactions`) simulates gateway packet loss. Chosen over `--no-mining` spawn flag because it's runtime-toggleable — existing tests stay on auto-mining, only delayed-inclusion tests flip it. |
| T3 | Shorter poll intervals for tests (sub-second `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`) | Reduces raciness in §11, §7.7, §6 | `[ ]` Not built |
| T4 | `wait_for_recovery_complete` helper (poll a health / debug endpoint) | Replaces sleep-based waits throughout §11, §7 | `[ ]` Not built |
| T5 | Injectable failpoints (SQLite error, sub-transaction crash) | §7.2.2, §7.6.2 done; §7.6.3, §2.10.1 (H1) need more | `[?]` Partial — inline tests already induce some |
| T6 | Smaller `MAX_WAIT_BLOCKS` for test builds (optional optimization) | Shortens mine-1200-blocks tests | `[-]` Probably not needed — 1200 empty blocks mines in <1s |
| T7 | libfaketime via `FAKETIME_TIMESTAMP_FILE` (dynamic) for the sequencer subprocess | §7.8.1 (done), §7.8.3 (clock skew, done), §11.2.2 (done, live danger-zone detection), §7.3.5 (aging-Tip, pending), §7.8.2 (first-boot-L1-down, pending) | `[x]` `ManagedSequencer::set_faketime_offset(Option<String>)` writes to the rc file; `ManagedSequencer::advance_wall_and_mine(Duration)` is the coupled (cumulative) helper. Harness sets `FAKETIME_TIMESTAMP_FILE` + `FAKETIME_NO_CACHE=1` + `DYLD_INSERT_LIBRARIES`/`LD_PRELOAD` on the child. Dynamic: the running sequencer re-reads the file on every time call, so tests can shift time mid-run without a respawn. Added to `flake.nix` + CI (`apt install faketime` on Ubuntu). |
| T8 | Orchestrator-restart primitive (`respawn_until_stable`) | §11.1.5 (done), §11.2.2-follow-up (done), §11.3.3 (done) | `[x]` `ManagedSequencer::respawn_and_watch(Duration) -> RespawnAttemptOutcome` classifies a single attempt into `Stable` / `RespawnFailed(String)` / `ExitedPostRespawn(ExitStatus)`. `respawn_until_stable(RespawnPolicy)` wraps it in a retry loop with optional `advance_per_retry` — required for the danger-zone-to-cascade convergence path (aged closed batch only cascades once it ages past `MAX_WAIT_BLOCKS`, so each retry needs to advance L1 + wall clock). Returns the full attempt sequence so tests can assert *both* convergence and that the loop actually exercised the flush/shutdown path (not a cheap first-attempt success). |

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
