# Security Review TODO

Open findings from the staged security review. The threat model being applied is documented in [`docs/threat-model/README.md`](docs/threat-model/README.md).

Findings accumulate here section by section as review parts complete. Fixes are batched after all passes finish to avoid interleaving changes with ongoing review.

## Severity legend

- **Critical** — protocol break or directly exploitable; must be fixed before any public deployment
- **High** — exploitable under realistic conditions
- **Medium** — real issue, conditional impact
- **Low** — defense-in-depth / hardening

---

## Part 1 — Scheduler

### [Critical] EIP-712 domain mismatch between scheduler and sequencer

**Locations:**
- Scheduler uses `name: None, version: None` — [`examples/canonical-app/src/scheduler/core.rs:328`](examples/canonical-app/src/scheduler/core.rs)
- Sequencer uses `name: Some("CartesiAppSequencer"), version: Some("1")` — [`sequencer/src/runtime/config.rs:8`](sequencer/src/runtime/config.rs) and [`sequencer/src/runtime/config.rs:116`](sequencer/src/runtime/config.rs)

**What it is.** The two sides disagree on which optional fields are present in the EIP-712 domain struct. Presence vs absence of `name` and `version` changes the `typeHash` used in `hashStruct(EIP712Domain)`, which changes the domain separator, which changes the final signing hash. The same signature recovers a different address (or fails) under each domain.

`UserOp` has no `from` field ([`sequencer-core/src/user_op.rs:10`](sequencer-core/src/user_op.rs)), so the address returned by `recover_address_from_prehash` is authoritative. The scheduler passes it directly to `validate_and_execute_user_op(sender, ...)` without cross-check ([`examples/canonical-app/src/scheduler/core.rs:251`](examples/canonical-app/src/scheduler/core.rs)).

**Impact.** Every honest user transaction that the sequencer admits is undeliverable on the scheduler. The sequencer's WS feed and HTTP responses promise soft confirmations for transactions the rollup cannot execute. Off-chain state diverges from canonical state on every tx.

**Why no existing test catches it.** `examples/canonical-test/src/main.rs:233` constructs the domain with the same `None, None` form used by the scheduler, so scheduler-local tests agree with themselves while failing to cross-check against real sequencer-produced signatures.

**Action items:**
- [ ] Promote `DOMAIN_NAME` and `DOMAIN_VERSION` into `sequencer-core` and expose a shared `build_input_domain(chain_id, app_address) -> Eip712Domain` constructor.
- [ ] Replace the local constructors in `sequencer/src/runtime/config.rs::build_domain` and `examples/canonical-app/src/scheduler/core.rs::input_domain` with the shared one.
- [ ] Add an integration test that signs a `UserOp` through the sequencer's signing path and asserts a scheduler-side recovery yields the same address.
- [ ] Update `examples/canonical-test/src/main.rs` to use the shared constructor so the harness cannot mask future drift.

**Threat model note.** This is a correctness bug, not an attacker-triggered exploit. Under the rollup's security model, a correctness bug that causes scheduler/sequencer state divergence is as severe as direct theft — the sequencer's soft-confirmation guarantee is structurally broken.

---

## Part 2 — `sequencer-core` (excluding `fee.rs`)

### [Low] `INPUT_TAG_DIRECT_INPUT` is a dead-code constant with self-contradicting documentation

**Locations:**
- [`sequencer-core/src/batch.rs:6-9`](sequencer-core/src/batch.rs) — constant and its stale docstring
- [`sequencer-core/src/batch.rs:40-41`](sequencer-core/src/batch.rs) — authoritative (and correct) contract documentation in the same file
- [`AGENTS.md:103`](AGENTS.md) — reinforces the stale claim

**What it is.** The constant is documented as if it were part of the wire contract (`0x00 || body`), but zero code in the workspace reads it. Input classification is actually by `msg_sender`, with the payload treated as opaque bytes — which the adjacent paragraph correctly states. The two paragraphs in the same file contradict each other.

**Impact.** No runtime exploit today; both sides agree on "ignore any tag byte." The forward-looking risk is that a future change acting on the misleading doc could add tag checking on one side but not the other, silently causing scheduler/sequencer divergence.

**Action items:**
- [ ] Remove the `INPUT_TAG_DIRECT_INPUT` constant and its docstring from `sequencer-core/src/batch.rs`.
- [x] Remove the corresponding paragraph in `AGENTS.md`. *(Done in the 2026-04-15 AGENTS.md rewrite — classification is now documented as by `msg_sender`, payload opaque.)*
- [ ] Keep the correct paragraph at `batch.rs:40-41` as the authoritative wire contract.

---

### [Low] Protocol invariant `max_fee >= current_fee` lives per-impl instead of in the shared trait default

**Locations:**
- [`sequencer-core/src/application/mod.rs:99-116`](sequencer-core/src/application/mod.rs) — default `validate_and_execute_user_op`, no pre-check
- [`examples/canonical-app/src/scheduler/core.rs:247-250`](examples/canonical-app/src/scheduler/core.rs) — scheduler's explicit protocol-level pre-check
- [`examples/app-core/src/application/wallet.rs:150-155`](examples/app-core/src/application/wallet.rs) — wallet impl correctly enforces the same rule

**What it is.** The scheduler treats `max_fee >= fee_price` as a protocol-level invariant, checked *before* dispatch into the `Application` trait. The sequencer's side relies on each `Application` impl to enforce the rule via its own `validate_user_op`. The shared `sequencer-core` trait default does not encode the invariant. An app impl that omits the check would cause the sequencer to admit ops the scheduler silently drops — structural soft-confirmation break.

**Impact.** Latent. The shipping `WalletApp` enforces the check correctly. The concern is that a protocol invariant lives in two places (scheduler source + each app impl) rather than in the shared crate.

**Action items:**
- [ ] Move the `max_fee < current_fee` check into the default `validate_and_execute_user_op` in `sequencer-core/src/application/mod.rs` (return `ExecutionOutcome::Invalid(InvalidMaxFee { .. })` before dispatching to `validate_user_op`).
- [ ] Optional: remove the now-redundant pre-check at `scheduler/core.rs:247-250`, or leave it as defense-in-depth.
- [ ] Optional: remove the now-redundant check from `WalletApp::validate_user_op`.

---

## Part 5 — L1 Interaction

No vulnerability findings. See Hardening section below for two defense-in-depth items surfaced by the Part 5 review.

---

## Part 6 — Recovery

### [Low] `open_recovery_batch_in_tx` masks `l1_safe_head` corruption with silent zero

**Location:** [`sequencer/src/storage/recovery.rs:388`](sequencer/src/storage/recovery.rs)

**What it is.** During recovery, the safe block is read via `query_current_safe_block(tx).unwrap_or(0)`. If the `l1_safe_head` singleton row is missing (DB corruption, manual tampering, forgotten migration), the recovery batch is opened with `safe_block = 0`.

**Impact.** A recovery batch with `safe_block = 0` is immediately stale on any chain older than `MAX_WAIT_BLOCKS` blocks (i.e., effectively always). The scheduler skips it. The sequencer's danger-detection fires again on the next tick → new recovery → new batch with `safe_block = 0` → stale again. Infinite recovery loop, bounded only by the batch submitter's gas budget.

Every other `query_current_safe_block` caller in the codebase propagates the error. This is an unprincipled silent-failure path in the one subsystem where silent failure is worst.

**Why not higher severity.** The triggering condition is not adversary-reachable — it requires DB corruption. Under self-trust, operator-caused DB state is not a threat we runtime-defend. The finding is filed because the Part 6 threat-model calibration calls for extra rigor in recovery-internal correctness, and this is a silent-fail regression vs the rest of the codebase.

**Action items:**
- [ ] Replace `.unwrap_or(0)` with `?` propagation: `let safe_block = query_current_safe_block(tx)?;`
- [ ] Add a test asserting `open_recovery_batch_in_tx` returns an error (not silent zero) when `l1_safe_head` has no row.

---

*Vulnerability findings from subsequent review parts will be appended here, above the Hardening section.*

---

## Hardening / Defense-in-Depth

Not vulnerabilities under the project's threat model — filed here to track opportunistic hardening that reduces surface area or information disclosure without addressing concrete exploits. Apply when convenient; no urgency.

### [Hardening] rusqlite error text echoed to 500 response body

**Location:** [`sequencer/src/ingress/inclusion_lane/mod.rs:244-247`](sequencer/src/ingress/inclusion_lane/mod.rs)

**What it is.** `append_user_ops_chunk` failures are mapped into the client-facing 500 JSON body via `SequencerError::internal(format!("db error: {err}"))`. `rusqlite::Error::Display` can include SQL fragments, table / column / constraint names, and SQLite detail messages. These then appear verbatim in the `message` field of the JSON response.

**Why not a vulnerability.** Not adversary-reachable — no user-submitted field hits a UNIQUE constraint or FK, and the schema is visible in the open migration file anyway. The path only fires on operational incidents (disk full, WAL contention, migration drift). Surfaced in Part 4 review.

**Action item:**
- [ ] Replace the interpolated `{err}` with a constant client-facing string (e.g. `"internal storage error"`). Keep the detailed `rusqlite::Error` on the lane-crash / structured-log path only. Mirrors the existing `ApiError::internal_error("inclusion lane dropped response")` pattern.

### [Hardening] axum `JsonRejection` Display text echoed to 400 response body

**Location:** [`sequencer/src/ingress/api.rs:94-100`](sequencer/src/ingress/api.rs)

**What it is.** `map_json_rejection` wraps axum's raw `JsonRejection::Display` into `ApiError::bad_request(format!("invalid JSON: {err}"))`. For malformed bodies the Display text includes serde's line/column and an excerpt of the offending token, exposing parser-version fingerprinting and reflecting attacker-submitted bytes.

**Why not a vulnerability.** Response content-type is `application/json`, so no XSS. The attacker is reflecting their own bytes back to themselves — no credential or third-party data exposure. Fingerprinting axum/serde versions is low-impact (dep versions are recoverable from `Cargo.lock`). Surfaced in Part 4 review.

**Action item:**
- [ ] Replace `{err}` interpolation with a fixed taxonomy driven by the `JsonRejection` variant: `"invalid JSON"`, `"missing content type"`, `"unsupported content type"`, `"request body too large"`. Log the full `err` for operators.
- [ ] Audit any other handler that maps extractor rejections into user-visible error bodies and apply the same pattern.

### [Hardening] Private-key parse error may echo key bytes into the error string

**Location:** [`sequencer/src/l1/provider.rs:52-54`](sequencer/src/l1/provider.rs)

**What it is.** `create_signer_provider` formats the underlying parse error as `format!("invalid private key: {e}")`. alloy's `LocalSignerError` wraps `hex::FromHexError::InvalidHexCharacter { c, index }`, which echoes a character from the input and its index. For a key that *almost* parsed (typo, stray whitespace, extra characters), the error string includes one character of the intended secret plus its position — enough to substantially narrow the secret for an observer with access to the startup log.

**Why not a vulnerability.** Operator-trusted surface; not adversary-triggered. Surfaced in Part 5 review.

**Action item:**
- [ ] Replace the interpolated `{e}` with a fixed string, e.g. `.map_err(|_| "invalid private key".to_string())`. Mirror in `runtime/mod.rs` and any other callsite that maps `PrivateKeySigner::from_str` errors.

### [Hardening] Provider accepts `http://` URLs with no scheme enforcement

**Location:** [`sequencer/src/l1/provider.rs:20-47`](sequencer/src/l1/provider.rs)

**What it is.** `create_client` accepts any URL parseable by `reqwest::Url`. No guard against `http://` for non-loopback hosts. Our node and Infura/Alchemy fallback are both trusted fail-stop under the threat model (MITM is byzantine, out of scope), but a scheme typo in a remote RPC URL makes MITM newly possible — a concrete operational foot-gun.

**Why not a vulnerability.** The threat being prevented is out-of-scope byzantine RPC. This guard just reduces the blast radius of operator misconfig. Surfaced in Part 5 review.

**Action item:**
- [ ] In `create_client`, reject non-`https` schemes unless the host is a loopback address (`127.0.0.1`, `::1`, `localhost`). Three-line guard.

### [Hardening] Flusher bumps `max_priority_fee_per_gas` but not `max_fee_per_gas`

**Location:** [`sequencer/src/recovery/flusher.rs:147-155`](sequencer/src/recovery/flusher.rs)

**What it is.** The flusher submits no-op txs with `max_priority_fee_per_gas` doubled vs the current fee estimate, but `max_fee_per_gas` unchanged. Ethereum's local-node replacement rule requires **both** fields to bump by ≥10% to evict an existing tx at the same `(sender, nonce)`. If a previously-submitted batch tx is still in our node's mempool when the flusher runs, the no-op replacement will be rejected by our own node.

**Why not a vulnerability.** The outer `flush_and_wait` loop is unbounded (runs until `pending ≤ safe`), so eventual inclusion of either the original batch tx or the no-op resolves the slot. Safety holds regardless of which lands; only operational efficiency suffers. Surfaced in Part 6 review.

**Action items:**
- [ ] Bump `max_fee_per_gas` by ≥10% in the flusher too, mirroring the priority-fee bump.
- [ ] Add a sentence to `docs/recovery/README.md` clarifying that flush safety does not depend on eviction — it depends on the unbounded outer loop.

### [Hardening] Hardcoded 12s block time in flusher's confirmation timeout

**Location:** [`sequencer/src/recovery/flusher.rs:22, 25`](sequencer/src/recovery/flusher.rs)

**What it is.** `MempoolFlusher::CONFIRMATION_TIMEOUT = 120 seconds` hardcodes 10 × 12s = Ethereum cadence. On slower chains the per-tx watch fires spuriously; on faster chains, it's needlessly conservative. The related `SEQ_SECONDS_PER_BLOCK` is already operator-configurable for the wall-clock danger estimate but not wired into the flusher.

**Why not a vulnerability.** Inner `watch_txs` timeout only affects retry cadence; the outer loop retries. No correctness impact. Surfaced in Part 6 review.

**Action item:**
- [ ] Derive `confirmation_timeout` from `SEQ_SECONDS_PER_BLOCK * N` (e.g., N = 10), mirroring the batch poster's existing formula.

### [Hardening] Chain-id mismatch check runs late in bootstrap, after recovery writes to DB

**Location:** [`sequencer/src/runtime/mod.rs:211-257`](sequencer/src/runtime/mod.rs) and [`sequencer/src/runtime/mod.rs:132`](sequencer/src/runtime/mod.rs) (cache write)

**What it is.** `assert_eq!(rpc_chain_id, config.chain_id)` runs at line 257 — **after** `run_preemptive_recovery` (line 211), `input_reader.start()` (line 232), and the L1-cache write at line 132. The cache stores `config.chain_id` (operator-supplied), not the live RPC value. On a misconfigured chain_id, recovery pulls safe inputs from the wrong chain's InputBox before the mismatch panic fires. On crash-loop (systemd/k8s restart), each boot accumulates more wrong-chain `safe_inputs` rows.

**Why not a vulnerability.** Operator-config triggered; not adversary-reachable. Per the threat model, operator config is trusted. Filed as hardening because the fix is a genuine bootstrap-correctness improvement. Surfaced in Part 8 review.

**Action items:**
- [ ] Move the chain_id check to immediately after `provider` construction, before any `sync_to_current_safe_head` or `input_reader.start`.
- [ ] Return a typed `RunError`, not `assert_eq!` panic.
- [ ] Store the live-queried chain_id in the L1 cache (not `config.chain_id`), so the cache-fallback path at line 160 has independent evidence.

### [Hardening] `SEQ_SECONDS_PER_BLOCK=0` causes divide-by-zero panic during wall-clock fallback

**Location:** [`sequencer/src/runtime/config.rs:111`](sequencer/src/runtime/config.rs) (config), [`sequencer/src/recovery/mod.rs:210`](sequencer/src/recovery/mod.rs) (use site)

**What it is.** `SEQ_SECONDS_PER_BLOCK` is parsed as unbounded `u64` with no min validation. Used directly as divisor: `elapsed_secs / seconds_per_block`. An operator typo `=0` panics the process during the L1-outage fallback path — the worst time for the sequencer to crash.

**Why not a vulnerability.** Operator-config triggered. Surfaced in Part 8 review.

**Action items:**
- [ ] Add a clap `value_parser` on `seconds_per_block` requiring `>= 1`.
- [ ] Optionally mirror a guard at the use site in `wall_clock_danger_estimate` for defense in depth.
