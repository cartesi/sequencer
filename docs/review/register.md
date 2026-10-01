# Unresolved review work

Current follow-ups, maintained under the [review lifecycle](README.md).
Contracts and settled design reasons belong to their owners; completed review
history is in Git. Unless separately stamped, entries were checked against code
and test sources on 2026-09-17 at `85f033b0768de527312ff876a29bca67ee2f9316`.
That was a source audit, not a new run of every cited test. Recheck an entry
before acting on it.

## QA follow-ups

Selected next work from the QA report assessment, checked on 2026-09-24 against
`7e471454f4b9f55a245d10b922f81d3daa40e443`. Evidence includes current source and
selected archived reproducers/logs; the archived devnet/OOM campaigns were not
rerun. `cargo check --locked` and all 23 focused scheduler tests passed with
Rust 1.95. Each entry describes current behavior; its direction and next steps
are proposed work.

### Bound the canonical direct-input fridge

The shared [scheduler](../../sequencer-core/src/scheduler/mod.rs) retains raw
direct payloads without a byte budget. The age backstop does not bound memory,
and HTTP controls cannot limit canonical L1 traffic. QA's guest/node OOM
evidence supports investigating this independently of the toy wallet's state
growth; its workload-specific thresholds are not deployment capacity bounds.

Direction: use application-defined, deterministic format/sender rules to avoid
retaining irrelevant bytes, and a generous fixed capacity pinned per deployment.
Filtering or compacting must preserve valid deposits and agree across canonical
execution, live prediction, application-history progress, and recovery. Define
capacity in protocol bytes/slots, not allocator-dependent memory usage.

Size against the cheapest accepted L1 inputs, including transaction batching,
actual batch-inclusion delays and the backstop window. The five-block local
frame tick is not a canonical drain interval. Include temporary allocations,
application execution and accumulated outputs in the guest budget.

Overflow must preserve valid deposits and have deterministic, non-catastrophic
behavior; uninterrupted service and soft confirmations need not survive it.
Early FIFO execution is a candidate, not a settled algorithm: it can reorder
still-fresh delayed batches. Decide whether incompatible batches are rejected
or overflow is detected as an exceptional ordering event that stops optimistic
service and requires cockroach recovery. Matching batch bytes/nonce alone does
not reveal this divergence. Silent loss of valid deposits is not a recovery
strategy.

Next: settle that transition and measure headroom, then exercise delayed fresh
batches, same-block overflow, outages, and replay/rebuild agreement. Preserve
the [checkpoint eligibility rule](../recovery/cockroach.md#checkpoint-eligibility):
forcing one direct while leaving another from the same block queued can require
an earlier checkpoint or genesis. The [scheduler contract](../protocol/scheduler-semantics.md)
and [application contract](../protocol/application-contract.md) own the boundaries
the implementation must update together.

### Validate batch-open and danger timing together

[`RunConfig`](../../sequencer/src/commands/config.rs) accepts any positive
`max_batch_open_seconds`, while `protocol_timing()` validates only the separate
timing fields. Incompatible settings can repeatedly recover a low-volume open
batch before its seal deadline. Next: validate their relationship at the runtime
configuration boundary, with checked units and an explicit submission margin;
test refusal and a valid low-volume closure. This guard cannot guarantee timely
L1 inclusion.

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

### Support HTTPS and WSS in the Rust SDK

The [client constructor](../../sdk/rust-client/src/lib.rs) rejects every HTTPS
endpoint, leaving its HTTPS-to-WSS URL branch unreachable. Next: support both
HTTP/HTTPS and their WS/WSS counterparts, verify the dependency TLS features,
and exercise certificate-verified requests and subscriptions. Preserve local
HTTP use; URL acceptance alone is not evidence that TLS works.

### Separate public ingress and internal egress listeners

[`http.rs`](../../sequencer/src/http.rs) serves both routers on one listener.
Publishing that listener wholesale also exposes unauthenticated internal state
and snapshot routes. Next: finish the planned per-side bind configuration and
verify that every public route in the [API contract](../../README.md#api) (today
`POST /tx`, `GET /fee`, `GET /nonce`, and `GET /domain`) is on the public
listener and no internal route is. Keep network access controls as the
deployment boundary; no new authentication subsystem is implied.

### Fix the pinned test framework's cycle target

The pinned `testsi::run_machine_increment` passes constant `1 << 28` to an
absolute-cycle-target API. Long runs can stop advancing and report a timeout
caused by the driver. Evidence: [the pinned source](https://github.com/GCdePaula/cartesi-tools-rs/blob/ed14b98ecfe9796dc3ca7c9b96bfdbf0ef9baf22/host/testsi/src/machine.rs#L159)
and the [workspace dependency](../../Cargo.toml). Next: fix the target progression
in that tooling, update the pin, and verify guest progress past the old target
before relying on long-run capacity measurements. This is an upstream tooling
task needed by sequencer validation.

## Confirmed discrepancies

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
- **Intermittent C-host recovery WebSocket reset.** At `dc4dd78` on
  2026-09-19, `c_host_recovery_after_stale_batches_test` failed an expected
  message receive with `Connection reset without closing handshake` in the
  [push run](https://github.com/cartesi/sequencer/actions/runs/35456024614/job/105931662252).
  The [PR run](https://github.com/cartesi/sequencer/actions/runs/35456514750/job/105934071941)
  passed all 49 scenarios on the same source tree. The failed run retained no
  child-process log artifact, so the reset's cause is unclassified. CI now
  uploads `tests/e2e/results/*.log` on failure. On recurrence, use those logs
  to identify the server's last events and the failing receive before changing
  timeouts, teardown, or retry behavior. Evidence: the
  [recovery scenario](../../tests/e2e/src/test_cases.rs) and
  [WS receive helper](../../tests/harness/src/ws.rs).
- **Transient SQLite contention stops the submitter.** Read handles use a
  50 ms busy timeout; a storage/open failure escapes the submitter loop.
  BUSY/LOCKED are nonterminal but project to unclassified exit 1, causing
  respawn/recovery under a restarting supervisor. Frequency and benefit of
  local retry are unmeasured. Check contention before choosing a bounded
  retry or timeout change; preserve other errors. Evidence:
  [`storage/open.rs`](../../sequencer/src/storage/open.rs),
  [`submitter/worker.rs`](../../sequencer/src/l1/submitter/worker.rs),
  [`commands/error.rs`](../../sequencer/src/commands/error.rs).
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
- **OpenAPI drift.** [`openapi.yaml`](../../openapi.yaml) is hand-maintained
  beside the [API contract](../../README.md#api). CI lints its structure, but
  nothing checks it against the routes or serde wire types; only the
  change-together rule in [AGENTS.md](../../AGENTS.md#http-endpoints) keeps them
  aligned. Checked 2026-10-04 against the code at `27e55ce`. Next: if a
  generated client or documentation site comes to depend on the file, add a
  test that compares its paths with the routers and validates serialized wire
  types against its schemas.

## Known optimizations

Performance headroom with a known mechanism, not defects. Each entry names its
measurement, or the lack of one, and the trigger for acting. Checked 2026-09-25
at `6b2a233`.

- **Sender index on the chunk commit.** `idx_user_ops_sender_nonce` lets
  `GET /nonce` seek a sender instead of scanning history, but it is the chunk
  commit's costliest index. In the
  [2026-09-25 measurement](2026-09-25-sender-index-commit-cost.md) it raised
  the mean 64-op commit from 0.27 to 1.0 ms and p99 from 1.1 to 9 ms. It stays:
  any sender-keyed durable structure pays about the same once the sender
  population is large, and a derived index needs no recovery maintenance. If
  lane persistence becomes the throughput limit, the alternatives are a
  lane-written sender-to-next-nonce table, cheaper only while the sender
  population fits in a few pages and needing a rewind on invalidation, and
  moving checkpoints off the lane (next entry). Such a table is also where
  rebuilt-baseline nonces could be seeded; see
  [Track 6](../plans/2026-07-coordination-tracks.md#track-6--dump--application-api-redesign).
  On the read side, the lookup walks the sender's invalidated entries above its
  current nonce: about 0.11 µs each, 16 ms at 100,000, bounded by that sender's
  own rolled-back volume. A covering `(sender, nonce, batch_index)` index would
  skip the table fetch per entry. Revisit with a measurement on production-like
  Linux storage, or when lane throughput is the limit. Evidence:
  [`0001_schema.sql`](../../sequencer/src/storage/migrations/0001_schema.sql),
  [`storage/ingress.rs`](../../sequencer/src/storage/ingress.rs).
- **Checkpoints run inside the lane's commit.** No connection sets
  `wal_autocheckpoint`, so SQLite's default 1,000-page checkpoint runs inline in
  the `COMMIT` of whichever writer crosses it, usually the lane. It is the p99
  tail in the measurement above, with or without the sender index. The candidate
  is to disable autocheckpoint on the lane's connection and run `PASSIVE`
  checkpoints from a background connection; the risk to bound is WAL growth
  while readers hold snapshots. Unmeasured beyond that record. Evidence:
  [`storage/open.rs`](../../sequencer/src/storage/open.rs).
- **Per-request read connections.** `/fee`, `/nonce`, `/history`, and the
  finalized-state routes open a fresh read-only connection per request, so each
  pays a file open, a schema parse on its first statement, and cold page and
  statement caches: about 0.2 ms per request against about 2 µs for the nonce
  query itself ([measurement](2026-09-25-sender-index-commit-cost.md#read-cost)).
  A small pool of long-lived read connections would remove that cost. Separately,
  `current_fee_quote` builds a full `WriteHead`, including two `COUNT(*)` scans
  of the Tip's user ops, to return two numbers; that cost is bounded by batch
  size and unmeasured. Evidence:
  [`ingress/api.rs`](../../sequencer/src/ingress/api.rs),
  [`storage/queries.rs`](../../sequencer/src/storage/queries.rs).

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
