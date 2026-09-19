# Feed and replay integration (Track 3)

The application-history protocol is implemented. Its current contracts live in:

- [Application history and replay](../protocol/application-history.md): coordinates,
  storage/recovery boundaries, snapshot bootstrap, and consumer resume.
- [README API](../../README.md#api): routes, wire messages, refusal codes, and limits.
- [Snapshot lifecycle](../snapshots/lifecycle.md): durable artifacts, accepted
  comparisons, recovery exports, and leases.

## Merge scope and follow-up ownership

The repository delivery includes the egress contracts, storage, SDK, and reference
replay/recovery tests. Its merge criteria are review and the relevant repository
checks. Private-engine integration, operational rehearsal, and representative
capacity measurements are follow-up work; they do not block merging this API.
Merge the implementation, let consumers integrate, and adjust the API from
concrete feedback. There are no live deployments requiring compatibility.

| Follow-up | Owner | When it is needed |
|---|---|---|
| Private DEX scheduler, indexer, and database backups | Bart / application integration | After merge, while adopting the API. Verify complete checkpoint/claim association and scheduler replay; report missing fields or awkward workflow for adjustment. |
| Canonical-to-native export and incident rehearsal | Application integration and operators, under Track 6 | Before relying on that application's recovery procedure in production. |
| Ingress latency, indexing headroom, and recovery capacity | Sequencer/application maintainers and deployment operators | Before claiming support for the target deployment workload; measure historical serving alongside ordinary traffic. |

The [validation record](../review/2026-09-16-track3-validation.md) records the
wallet's nonempty cold bootstrap, concurrent replay/live delivery, recovery and
rebootstrap, canonical-machine gates, and local latency measurements. Those
results do not establish private-engine conformance or target-deployment capacity.

The reference C host has process coverage in the `c_host_` rollups E2E scenarios:
generated genesis, source-independent `EngineApp` snapshot restore, concurrent
backlog/live replay, clean restart, stale recovery, and a fresh-era rebuild.
Ordinary execution and recovery compare against the canonical machine. The
[C binding guide](../../bindings/c-app-engine/README.md#reference-wallet) owns
the commands and scope. This closes the reference-host follow-up; the private
engine and canonical-to-native exporter still require their own evidence.

## Application projections and recovery

Historical bootstrap and standard-recovery checkpoint reuse are implemented.
Bounded internal readers can keep additional
application-specific transfers, orders, deals, and portfolio history outside the
sequencer's application state. The client owns indexing, complete checkpoints,
and replay. The sequencer owns optimistic ordering; the scheduler remains the
canonical authority.

Current contracts live in the [projection replay guide](../protocol/projection-replay.md)
and [README API](../../README.md#history-metadata-and-historical-l1-inputs-internal-only).
`/history` supplies deployment/baseline/current-generation metadata and a coherent
accepted checkpoint receipt. `/historical-l1-inputs` serves the immutable raw
prefix through the era's stop block, with block seek, bounded pages, and typed
era refusal. `/history` also checks a saved generation against every intervening
recovery cut. The SDK exposes both reads. Standard recovery records cuts in its
existing transaction before replacement directs are inserted.

The [reference integration test](../../sequencer/src/integration_tests/historical_bootstrap.rs)
restores a complete wallet/projection checkpoint, replays one-record HTTP pages
through the scheduler, exercises a malformed-batch overdue drain and terminal
drain, and subscribes at nonzero `K`. It checks application state and explicit
projection order, including a same-block pending direct and the first live
input. Storage/API/SDK tests cover limits, oversized single inputs, fixed prefix
boundaries, era/generation behavior, errors, and response deadlines. This is
reference evidence, not private-engine conformance or a deployment benchmark.

The [checkpoint compatibility test](../../sequencer/src/integration_tests/historical_bootstrap/recovery_compatibility.rs)
uses guarded recovery, complete wallet/projection backups, HTTP compatibility
lookups, and WS replay. It distinguishes equal counts from different generations,
restores the nearest eligible checkpoint after missed recoveries, and retries a
recovery between lookup and subscription. Storage tests cover cuts before
replacement directs, empty/no-op invalidation, nonzero baselines, and rollback.

### Goals and acceptance criteria

| Goal | Status / remaining outcome |
|---|---|
| Historical bootstrap | Implemented: replay the complete fixed prefix and join the feed at `K`, including after a nonzero rebuild. |
| Standard recovery | Implemented: choose the nearest retained checkpoint whose prefix survived every intervening generation change, then restore/resubscribe. |
| Cockroach reader recovery | Core replay/metadata workflow and reference checkpoint restore implemented; Bart's actual checkpoint preparation, incident validation, and fallback rehearsal remain. |
| Stable coordinates | Existing era/generation/application count for claims; separate InputBox indices for raw paging. Counts alone do not certify cross-era compatibility. |
| Bounded serving cost | Page/item/response bounds implemented; representative bootstrap must preserve the ingress latency target and demonstrate catch-up headroom. |
| Operational readiness | Each production application supplies its canonical-to-native exporter, runbook, and non-genesis recovery drill under Track 6. |

### Follow-up sequence

1. **Consumer adoption after merge.** Bart integrates accepted-boundary checkpoint
   preparation and scheduler replay using the implemented API. Sequencer
   maintainers address concrete feedback as it arrives; downstream completion
   is not a prerequisite for repository delivery. Before production use, rehearse
   identifying an unsound projection checkpoint, including one below new `K`,
   and restoring an earlier trusted backup or genesis. The API does not certify
   the client's projection.
2. **Deployment readiness.** Validate the private native engine and measure
   latency, historical serving cost, projection throughput,
   catch-up headroom, and recovery time. Track 6 independently owns the versioned
   canonical-machine exporter and native-state-unavailable drill.

### Scope boundaries

Keep the existing recovery terminal drain: the first accepted resumed frame
accounts for old pending directs before its user ops. Replacing that mechanism
would require carrying pending work across the baseline and is not justified by
this consumer requirement.

Keep manual cross-era trust selection. Execution-prefix hash chains can identify
an input trace but cannot prove that an engine or indexer computed correct state;
revisit only if automated matching or measured reconstruction costs justify them.
Server-side scheduler replay producing a flattened execution archive and
submitter/key rotation remain separate future work. No client checkpoint
registration, server-side projection storage, or historical execution archive is
required by this design.

## Revisit only with a consumer need

- Resumable snapshot transfer: when artifact size makes interrupted downloads costly.
- Session fencing: if history can mutate within an admitted process or multiple
  local writers become supported.

These are triggers for design work, not promised APIs. The
[coordination plan](2026-07-coordination-tracks.md) owns cross-track priorities.
