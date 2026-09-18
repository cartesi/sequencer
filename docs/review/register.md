# Unresolved review work

Current follow-ups, maintained under the [review lifecycle](README.md).
Contracts and settled design reasons belong to their owners; completed review
history is in Git. Entries below were checked against code and test sources on
2026-09-17 at `85f033b0768de527312ff876a29bca67ee2f9316`. This was a source audit,
not a new run of every cited test. Recheck an entry before acting on it.

## Confirmed discrepancies

### Batch-size accounting omits SSZ overhead

The lane estimates each operation as `71 + max_method_payload_bytes()`.
The SSZ layout takes `83 + actual_payload_bytes` per operation, including its
list offset, plus 12 bytes per batch and 18 per frame. Thus the estimate
understates a maximum-size operation; smaller actual payloads can mask it.
This is a batch-target accounting discrepancy, not a demonstrated protocol-size
overflow. The discrepancy is not a universal percentage.

Evidence: [`SignedUserOp`](../../sequencer-core/src/user_op.rs),
[`Batch` / `Frame` / `WireUserOp`](../../sequencer-core/src/batch.rs), and
`user_op_count_to_bytes` in the [lane](../../sequencer/src/ingress/inclusion_lane/mod.rs).
Next: compare the intended bound with serialized batches across payload/frame
counts, then correct the estimate and check the separately configured
`batch_policy.log_user_op_bytes` used for fee accounting.

### Setup misclassifies some deterministic L1 configuration failures

Setup wraps reader bootstrap failures as live-worker failures, yielding exit 1;
normal-run startup classifies deterministic reader bootstrap failures as
terminal, exit 30. Malformed RPC URLs exercise this distinction. Discovery
failures such as a wrong InputBox also occur in setup, but run does not repeat
that discovery. The impact is a misleading operator hint in a one-shot command.

Evidence: [`setup`](../../sequencer/src/commands/setup/mod.rs),
[`InputReaderError::is_terminal_invariant`](../../sequencer/src/l1/reader.rs),
and `classify_input_reader` in [startup recovery](../../sequencer/src/recovery/mod.rs).
Next: classify failures by the setup phase and pin the external exit code;
preserve genuinely transient provider failures.

### Startup logs the full RPC URL

The `sequencer startup` event includes `eth_rpc_url` verbatim. Operator URLs can
carry credentials in userinfo, paths, or query parameters; private-key
redaction does not cover this field.

Evidence: [`commands/run/mod.rs`](../../sequencer/src/commands/run/mod.rs).
Next: omit the field or define a safe endpoint representation, and check
diagnostic/help paths with a synthetic credential-bearing URL. No credential
exposure in an actual deployment was established by this review.

## Bounded investigations and cleanup

- **Intermittent process-lock test failure.** The macOS workspace suite can
  report `Locked` at the final reacquisition in
  `dropped_runtime_scope_keeps_lock_until_detached_worker_stops` in
  [`workers.rs`](../../sequencer/src/commands/run/workers.rs); an isolated rerun
  passes. The worker drops its scope before signalling completion, so a simple
  worker-completion race does not explain the failure. Identify any remaining
  descriptor/process ownership before changing the assertion or lock behavior.
  Reproduced during the 2026-09-18 stack closeout; isolated and full serial
  runs passed. See the [current validation record](2026-09-18-stack-review-validation.md).
- **Transient SQLite contention stops the submitter.** Read handles use a
  50 ms busy timeout; a storage/open failure escapes the submitter loop.
  BUSY/LOCKED are nonterminal but project to unclassified exit 1, causing
  respawn/recovery under a restarting supervisor. Frequency and benefit of
  local retry are unmeasured. Check contention before choosing a bounded
  retry or timeout change; preserve other errors. Evidence:
  [`storage/open.rs`](../../sequencer/src/storage/open.rs),
  [`submitter/worker.rs`](../../sequencer/src/l1/submitter/worker.rs),
  [`commands/error.rs`](../../sequencer/src/commands/error.rs).
- **Canonical direct-input queue capacity.** The shared
  [`Scheduler`](../../sequencer-core/src/scheduler/mod.rs) retains queued
  payloads without a byte budget. Force-drain bounds age in the observed L1
  timeline, not bytes. Determine the supported L1-window volume and guest
  memory cost before claiming an OOM vulnerability or proposing a limit.
  Dropping or capping canonical inputs would change protocol semantics.
- **Overlapping admission policies.** The generic
  [`lifecycle` preflight](../../sequencer/src/storage/lifecycle.rs) contains
  setup/rebuild branches, but production calls it only for run/flush.
  [`setup`](../../sequencer/src/commands/setup/mod.rs) has its own admission
  path with an intentionally tested already-complete no-op. Consolidate or
  narrow the unused branches when next changing admission; there is no
  demonstrated conflicting live route.
- **Fee-observation visibility.**
  [`log_gas_price_updated_at_ms`](../../sequencer/src/storage/fee_oracle.rs)
  is persisted, with no production reader. Decide whether operator SQL
  inspection suffices or a real consumer needs exposure before adding an
  endpoint or removing the stamp. The accepted
  [oracle outage policy](../threat-model/README.md#actors-and-trust) is an
  economic tradeoff, independent of whether a health field exposes age.
- **Recovery across external effects.** Model/storage tests do not replace
  process-level zombie re-injection or a restart between flush completion and
  cascade commit. Before adding harness machinery, identify the missing
  observation: safe nonce consumption must exclude a later original landing,
  and a restarted recovery must rederive facts without reusing the previous
  attempt's flush witness. Use the [recovery model](../recovery/README.md) to
  bound a scenario and assess whether existing component tests suffice.

## Verification gaps

These are specific behaviors whose coverage remains incomplete, not a mandate
to build a general fault-injection framework. Add a discriminating assertion
at the smallest useful boundary when working on that behavior.

| Boundary | Existing evidence and remaining check |
|---|---|
| Elapsed-time danger | Storage/procedure coverage exists. Add a process scenario isolating `EstimatedBatchInDanger` from stale-view refusal: retry at exit 20 without a speculative cascade. Start in [recovery](../../sequencer/src/recovery/mod.rs) and the [E2E scenarios](../../tests/e2e/src/test_cases.rs). |
| Canonical divergence | Storage freeze and startup refusal are covered separately. Compose accepted divergent input, runtime stop, and refusal after respawn in a process test; frontier must remain frozen. See [I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen). |
| Rebuilt anchor | Anchor unit mechanics and rebuild round-trip are covered. Exercise a later full-tear cascade after a nonzero-anchor rebuild and verify submission resumes at that anchor. See [I16](../invariants.md#i16-the-batch-tree-has-exactly-one-valid-parentless-root-carrying-the-deployments-anchor-nonce). |
| Same-block directs | `multi_deposit_reconciliation_test` accumulates directs in separate blocks. Queue portal sends, mine once, assert equal receipt blocks, and verify canonical/WS order and attribution in the [E2E scenarios](../../tests/e2e/src/test_cases.rs). |
| Wallet business failure | Mixed replay is covered. Pin insufficient transfer/withdrawal amounts after successful fee validation: fee/nonce/progress advance, the transfer/withdrawal has no further effect, and replay agrees. See the [wallet implementation](../../examples/app-core/src/application/wallet.rs) and [Application contract](../protocol/application-contract.md#2-replay-safety--rejection-inclusion-and-failure). |
| Boot failure exits | Exit 20 now has a process assertion; exits 40/1 still need exact assertions in applicable setup/failure scenarios, so the supervisor receives the intended recovery/retry hint. Start in the [E2E scenarios](../../tests/e2e/src/test_cases.rs) and [exit contract](../../sequencer/src/commands/error.rs). |
| Uniswap-mode boot | Source-boundary tests pin setup validation, lazy runtime refresh, and transient quote retention. Existing sequencer E2Es use fixed mode. Decide whether a mock-pool boot scenario warrants its harness cost when changing oracle integration. |

Warm restore already has state/cursor agreement tests and snapshot downloads
have lease/GC coverage. If restart cost becomes a requirement, add an assertion
that distinguishes restoring a dump from a correct but expensive genesis
replay; no new snapshot lifecycle is implied.

## Integration work owned elsewhere

- [Track 3 follow-ups](../plans/2026-07-track3-feed-replay-design.md):
  private-engine snapshot-to-live replication and representative deployment latency,
  including checkpoint creation and complete L1-reconciliation turns.
- [Track 6](../plans/2026-07-coordination-tracks.md#track-6--dump--application-api-redesign):
  external engine/scheduler agreement, application-specific recovery export, independent
  fee-conversion vectors, and consumer-driven ABI/checkpoint decisions.

The [2026-09-16 validation record](2026-09-16-track3-validation.md) supports
those integration decisions within its stated scope. It is not proof of
private-engine conformance or deployment capacity.
