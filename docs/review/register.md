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

The scheduler bounds its fridge by a count of entries, and a direct whose
payload is over a cap never joins it. Both are template constants the
application supplies through `Application::scheduler_limits`; the wallet's
values, 16384 entries and 1024 bytes
([`wallet.rs`](../../examples/app-core/src/application/wallet.rs)), are
provisional. The [scheduler contract](../protocol/scheduler-semantics.md)
owns the rules: an overflow drains whole blocks of the oldest entries, which
live prediction cannot mirror, so an overflow is a deliberate divergence that
the watchdog's divergence latch reports and cockroach recovery answers.

To settle with Bart:

- The payload cap's value. A count bounds memory only if each entry's size
  is bounded too, and portals accept zero-value deposits with arbitrary extra
  data, up to the 64 KiB input cap; the cap is the longest payload the
  application accepts.
- The count. Above what L1 can include within the backstop window, roughly
  2 million zero-value deposits at mainnet's 60M gas limit today (30-40k gas
  each when a contract batches them; measure it on the pinned contracts),
  overflow cannot happen at all; the image fixes the bound for the
  deployment's life, so leave headroom for higher gas limits. A smaller count
  still makes overflow costly, and its consequence is a manual rebuild, not a
  dead application. One block can carry about 1,500 to 2,000 directs at
  today's gas limit; a block that fills the bound alone joins over it.
- The largest drain. The first frame after an outage and the root frame after
  a rebuild both drain a full fridge in one input, so the bound must fit that
  drain in the guest's budget, temporary allocations, execution and
  accumulated outputs included. The five-block local frame tick is not a
  canonical drain interval.

Next: settle these values and measure headroom, then exercise delayed fresh
batches, same-block overflow and outages on devnet. An overflow changes the
application state after a block's last accepted batch, so the watchdog's
block-boundary comparison does not hold for that block
([snapshot lifecycle](../snapshots/lifecycle.md#acceptance-and-comparison)).

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

## Bounded investigations and cleanup

- **Confirmation watches outlive an RPC outage** (checked 2026-10-13). alloy's
  watch timeout reaps only transactions it has not yet seen
  (`alloy-provider` `heart.rs`), so if the endpoint dies after a batch
  transaction was seen, the poster's watch waits out the outage instead of
  failing. The heartbeat logs each failed poll at ERROR, and shutdown still
  cancels the wait. Under the fail-stop model this costs nothing, since nothing
  useful can happen meanwhile. Revisit on a partial failure where block polling
  stalls while the endpoint otherwise serves; the fix is an outer timeout around
  the watch. Evidence: [`submitter/poster.rs`](../../sequencer/src/l1/submitter/poster.rs).
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
- **Transient SQLite contention stops the submitter and the input reader**
  (input-reader half checked 2026-10-13 at `1a9615d`).
  Submitter read handles use a 50 ms busy timeout; the input reader opens
  writer handles (5 s busy timeout) for its cursor read and its append. In
  both, a storage/open failure escapes the worker loop. BUSY/LOCKED are
  nonterminal but project to unclassified exit 1, causing respawn/recovery
  under a restarting supervisor. Frequency and benefit of local retry are
  unmeasured. Check contention for both workers before choosing a bounded
  retry or timeout change; preserve other errors. Evidence:
  [`storage/open.rs`](../../sequencer/src/storage/open.rs),
  [`submitter/worker.rs`](../../sequencer/src/l1/submitter/worker.rs),
  [`l1/reader.rs`](../../sequencer/src/l1/reader.rs),
  [`commands/error.rs`](../../sequencer/src/commands/error.rs).
- **Fee-observation visibility.**
  [`log_gas_price_updated_at_ms`](../../sequencer/src/storage/fee_oracle.rs)
  is persisted, with no production reader. Decide whether operator SQL
  inspection suffices or a real consumer needs exposure before adding an
  endpoint or removing the stamp. The accepted
  [oracle outage policy](../threat-model/README.md#fee-oracle) is an
  economic tradeoff, independent of whether a health field exposes age. The
  oracle rewrites the stamp on every successful poll, unchanged price included,
  so a stale stamp means failing polls. That is one FULL-sync commit per
  successful poll, an estimated (unmeasured) cost under 0.1% of writer time.
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
- **Per-batch snapshots run on the lane.** Every batch close writes its
  snapshot before the sealing commit, so admission waits for
  `Application::create_dump` and the sequencer's fsyncs. Closes fire on the fee
  policy's byte target or the batch-open deadline, not on L1 posting, so under
  load several land per L1 block and the stall grows with throughput over target
  size. Exact per-batch snapshots stay: restart, acceptance comparison, and
  recovery select the exact batch's snapshot
  ([lifecycle](../snapshots/lifecycle.md#creation-and-restart)). For a
  large-state native engine, the mitigation is a `create_dump` that clones files
  on a copy-on-write filesystem (XFS with `reflink=1`, btrfs, APFS); see the
  [clone costs](../cartesi-machine.md). Unmeasured beyond the batch-close
  event's `snapshot_ms`. Revisit, per the
  [capacity assumption](../protocol/application-contract.md#5-operational-capacity-for-l1-reconciliation),
  when its p99 on deployment storage exceeds about 250 ms or retained snapshots
  exceed the deployment's disk budget. Evidence:
  [`inclusion_lane/snapshot.rs`](../../sequencer/src/ingress/inclusion_lane/snapshot.rs)
  (added 2026-10-13 at `462e4d1`).
- **Bounded ingestion for syncs without a lane** (checked 2026-10-13 at
  `1a9615d`). A reader tick fetches `[cursor, safe]` whole, holds every event in
  memory (decoded event plus raw log, roughly twice the input bytes;
  unmeasured) and commits once; one failed single-block `get_logs` restarts the
  span. In `run` the span stays under `l1_read_stale_after_blocks`. Setup and
  startup syncs after downtime have no bound: a fresh
  database scans every input since the app's deployment, our batches included
  (the InputBox caps an encoded input at 64 KiB). Current histories are
  megabytes, and rerunning on a larger host is the fallback. The known
  mechanism commits per block window without archive state. When the received
  indices are contiguous from the stored count and the last one sits at block
  `b`, the stored and received inputs in blocks before `b` are complete,
  because InputBox indices follow L1 order. Commit those at head `b - 1`, with
  that block's header timestamp and without refreshing `synced_at_ms` (I12),
  and hold the inputs at `b` for the next window; an empty window commits
  nothing; the window that reaches the sampled safe head keeps today's count
  check; reject logs outside the requested window; a later failure still
  returns an error. A window end is never committed on contiguity alone: the
  lane would stamp frames past a dropped tail input. Act when `safe_inputs` payload bytes
  approach a quarter of the recovery host's memory, when a recovery sync fails
  on a transient mid-crawl, or when the reader becomes a crate shared with
  another node. Evidence: [`l1/reader.rs`](../../sequencer/src/l1/reader.rs),
  [`l1/partition.rs`](../../sequencer/src/l1/partition.rs),
  [`storage/l1_inputs.rs`](../../sequencer/src/storage/l1_inputs.rs).

## Verification gaps

These are specific behaviors whose coverage remains incomplete, not a mandate
to build a general fault-injection framework. Add a discriminating assertion
at the smallest useful boundary when working on that behavior.

| Boundary | Existing evidence and remaining check |
|---|---|
| Elapsed-time danger | Storage/procedure coverage exists. Add a process scenario isolating `EstimatedBatchInDanger` from stale-view refusal: retry at exit 20 without a speculative cascade. Start in [recovery](../../sequencer/src/recovery/mod.rs) and the [E2E scenarios](../../tests/e2e/src/test_cases.rs). |
| Canonical divergence | Storage freeze and startup refusal are covered separately. Compose accepted divergent input, runtime stop, and refusal after respawn in a process test; frontier must remain frozen. See [I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen). |
| Rebuilt anchor | Setup writes the anchor from its rebuild. Storage tests accept a first batch at a nonzero anchor and tear a nonzero-anchor root, which drains again from the first queued input ([`fill.rs`](../../sequencer/src/commands/setup/fill.rs)). No process scenario submits after a full-tear cascade of a recovered era and checks the nonce it resumes at; the stored-machine round trip below is the natural place. See [I16](../invariants.md#i16-the-batch-tree-has-exactly-one-valid-parentless-root-carrying-the-deployments-anchor-nonce). |
| Same-block directs | `multi_deposit_reconciliation_test` accumulates directs in separate blocks. Queue portal sends, mine once, assert equal receipt blocks, and verify canonical/WS order and attribution in the [E2E scenarios](../../tests/e2e/src/test_cases.rs). |
| Boot failure exits | Exit 20 has a process assertion, and exit 30 for setup on a set-up directory. Exit 40 is setup's escape check, covered only by unit and chain tests (`escape_check_*`, `the_escape_check_refuses_activity_past_the_stop_block`); exits 40 and 1 still need exact assertions in process scenarios, so the supervisor receives the intended hint. Start in the [E2E scenarios](../../tests/e2e/src/test_cases.rs) and [exit contract](../../sequencer/src/commands/error.rs). |
| SDK HTTPS | A test pins that `https://` endpoints open with a TLS ClientHello. Certificate rejection is untested; it relies on reqwest's rustls platform verifier, with no override in the SDK. Add a rejection test if the SDK gains custom roots. See the [client](../../sdk/rust-client/src/lib.rs). |
| Uniswap-mode boot | Source-boundary tests pin setup validation, lazy runtime refresh, and transient quote retention. Existing sequencer E2Es use fixed mode. Decide whether a mock-pool boot scenario warrants its harness cost when changing oracle integration. |

Warm restore already has state/cursor agreement tests and snapshot downloads
have lease/GC coverage. If restart cost becomes a requirement, add an assertion
that distinguishes restoring a dump from a correct but expensive genesis
replay; no new snapshot lifecycle is implied.

## Sequencer recovery follow-ups

What `setup --base` leaves open, checked on 2026-11-10 against the working
tree that adds it. The [recovery plan's todo](../plans/2026-11-sequencer-recovery/todo.md)
holds the remaining build work; these are the accepted limits and the
unverified paths.

- **`/nonce` after a recovery.** `next_user_nonce` reads only the era's live
  ops, so a sender whose last op lies in the rebuilt history or below the base
  count reads 0 until its first live op. It stays because the application is
  the nonce authority: the lane's `422` names the expected nonce, which
  corrects the client in one round trip, and an exact answer would need the
  lane to ask the application per request or a sender table seeded from the
  baseline (the [sender index](#known-optimizations) entry). The
  [README](../../README.md#get-noncesenderaddress) documents it. Revisit if a
  wallet cannot recover from the `422` without user action. Evidence:
  [`storage/ingress.rs`](../../sequencer/src/storage/ingress.rs).
- **A base machine's state through the range source.** Setup reads the
  application's state only through the `state` inspect query
  ([`machine.rs`](../../sequencer/src/machine.rs)), whose report must fit the
  guest's 2 MiB transmit buffer; the watchdog's labeled-range state source has
  no setup counterpart. The reference wallet's `state` report passes that
  size at about 27,000 accounts, where the guest's inspect halts. This is
  acceptable while no deployment's state approaches it; add a range read
  before a deployment's application state can exceed 2 MiB, which libdex's
  likely will ([state sources](../watchdog/README.md#state-sources)).
- **The 2^36-cycle cap on each inspect query** (`QUERY_MCYCLE_BUDGET`) bounds
  a guest that never answers. Its headroom over encoding a 2 MiB report is
  unmeasured; measure it with the range read above.
- **The scheduler header's direction.** [`scheduler.h`](../../bindings/c-scheduler/include/scheduler.h)
  and its reference export exist; whether a C or C++ guest links that export
  or its port implements the header, and so whether the sequencer would link
  a foreign scheduler for the rebuild, is the port author's call
  ([questions](../plans/2026-11-sequencer-recovery/todo.md#questions-for-bart)).
  Until then setup and the sequencer call the Rust `Scheduler` directly, with
  no adapter and no `Scheduler` trait: nothing foreign runs on that path yet,
  so a seam would have no second implementation to test it.
- **Unverified on the devnet.** The rollups-e2e scenarios
  `setup_refuses_a_set_up_data_directory_test`,
  `c_host_setup_refuses_a_set_up_data_directory_test`,
  `recovery_from_genesis_round_trip_test` and
  `c_host_recovery_from_genesis_round_trip_test`, and the harness's setup and
  respawn model, compile and pass clippy but were not run.
  `recovery_from_stored_machine_round_trip_test` is ignored: the current image
  already answers the `scheduler` query
  (`export_of_the_template_image_is_genesis` in canonical-test), but it needs
  the watchdog Lua dependencies and `cartesi-machine`, and it assumes the
  shape of `sequencer-watchdog replay`'s output. Run
  `just test-recovery-round-trip` before removing the ignore.
  Evidence: [`recovery_round_trip.rs`](../../tests/e2e/src/recovery_round_trip.rs),
  [`test_cases.rs`](../../tests/e2e/src/test_cases.rs).
- **Unverified on GitHub Actions:** the release workflow's native arm64
  runner (`ubuntu-24.04-arm`) and the amd64 release link, now that the
  sequencer links the emulator. Evidence:
  [`release.yml`](../../.github/workflows/release.yml).
- **Release notes owe two items.** `HistoryBaseline.rebuilt_history_end` is a
  required field, so an older SDK build fails to decode `/history`; and the
  released binaries link the emulator statically, so they need the Ubuntu
  24.04 or Debian 13 C++ runtime.
- **Rebuild and execution time on a large history** are unmeasured
  ([measure](../plans/2026-11-sequencer-recovery/todo.md#measure)). Setup
  streams both in chunks, so memory stays bounded; time is the open question.

## Canonical rules open for the C++ port

Each of these changes the canonical machine image or its wire format, so it is
agreed with the port's author. The wire-format bump ships first, on its own,
after draining sealed batches and before the first production deploy; the
rest ship in the port's image. The
[scheduler contract](../protocol/scheduler-semantics.md) owns the rules in
force.

- **Signature acceptance set.** The guest and `POST /tx` accept every `v ≥ 35`
  and the high-`s` twin, as alloy does. Proposed: `v ∈ {27, 28}` and low `s`
  only.
- **Empty batches** are valid at any age and consume a nonce, though the
  sequencer never sends one. Reject them only if the port forces a new image
  anyway.
- **Wire format.** A fixed 65-byte signature field, with the
  [SSZ overhead](#batch-size-accounting-omits-ssz-overhead) fix; drain sealed
  batches before the switch.
- **Riding the port's image:** the
  [fee table](../plans/2026-07-coordination-tracks.md#track-5--fee-exponentiation-lut),
  the [fridge bound and cap values](#bound-the-canonical-direct-input-fridge),
  and a setup check of the guest's compiled `sequencer_address`. With them
  come versioned gate and signature vectors, adding the unpinned `s ≥ n`, `v`
  from 29 to 34 and the fee-range reject.

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
