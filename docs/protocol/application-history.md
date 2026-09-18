# Application history and replay

Application history is the sequencer's current execution order: included user
operations and external direct inputs. It contains an optimistic suffix that
recovery may replace. A stable offset identifies an input only together with
its history version; receiving it does not establish L1 acceptance.

This document owns history coordinates, recovery boundaries, and replica
bootstrap. The [Application contract](application-contract.md) owns execution
and progress; the [snapshot lifecycle](../snapshots/lifecycle.md) owns artifact
publication, selection, and retention; the [README API](../../README.md#api)
owns routes, wire fields, and refusal codes.

## Progress, ordering, and acceptance

These facts answer different questions:

| Fact | Meaning |
|---|---|
| `ExecutedInputCount` | Number of application inputs already executed. At count `N`, entry `N` executes next. |
| `ApplicationProgress.last_executed_safe_block` | Maximum clock carried by an executed input: frame safe block for user ops, inclusion block for directs. |
| Latest surviving frame's `safe_block` | Complete L1 interval accounted for by local ordering, bounded below by the era baseline. |
| `safe_accepted_batches` | Safe L1 landings accepted by the scheduler rules and matched to local sealed bytes. |

A frame can account for an empty interval or only batch envelopes, advancing
L1 accounting without executing an application input. An accepted batch can
confirm existing execution without adding an input. Empty batches can share
an application count. Neither the count nor the app clock substitutes for the
L1 accounting boundary or acceptance facts.

`safe_inputs` retains all InputBox observations, including batch envelopes.
`application_inputs` contains only the current ordered application sequence;
each row has a mandatory pre-execution offset, an owning batch/frame, and
exactly one reference to a user op or external direct input. Payloads remain
in those source tables. Included business failures and malformed-direct no-ops
advance the count; validation rejections and envelopes do not.

Restart and egress read this same sequence. Replay executes stored valid
user ops with their recorded fee and frame clock, without revalidation;
directs use their original inclusion block. Timestamps and transaction hashes
are provenance, not extra application-transition inputs. The
[execution contract](application-contract.md#the-execution-methods) defines the
shared execution boundary.

## Identity and available history

A `HistoryClaim` combines:

- **Era**: a UUIDv4 created by setup/rebuild, identifying one local history.
- **Recovery generation**: a revision within that era, advanced atomically
  whenever automatic recovery invalidates at least one valid batch.
- **Next input**: the application's actual executed-input count.

If the era baseline count is `K` and the current head is `H`, available entries
occupy `[K, H)`. A claim at `H` waits for future entries; a claim below `K` or
above `H` is refused. Identity is checked before position. Equal counts cannot
authorize resuming a different era or generation, even if the consumer believes
its state precedes the replaced suffix. The compatibility query below can
authorize rebinding a surviving checkpoint to the current version; WS itself
continues to require exact identity.

Automatic recovery invalidates a batch suffix, removes its current application
rows, advances the generation, and opens the replacement Tip in one transaction.
Replacement inputs reuse suffix offsets under the new generation. Original L1,
batch, frame, and user-op source records remain; the invalidated flattened
sequence is not separately retained. A repair that invalidates nothing leaves
the generation unchanged. The [recovery guide](../recovery/README.md) owns
repair selection and guards.

### Checkpoint compatibility after standard recovery

Each generation transition records the surviving application count after the
invalidated rows are removed and before the replacement Tip adds any directs.
This cut commits atomically with invalidation, the generation advance, and
reopening. An invalidated empty batch still creates a transition with the old
head as its cut; a repair that invalidates nothing creates neither. The cuts are
immutable and retained for the era's lifetime.

For a checkpoint saved in generation `g`, `/history` returns the current version
at generation `G`, head `H`, and preserved count:

```text
P = min(H, cut[g+1], ..., cut[G])
```

For `g = G`, `P = H`. A checkpoint at count `X` is eligible to resume under the
returned version exactly when `K <= X <= P`. Check each saved checkpoint using
its own era and generation, then choose the newest eligible one. For cuts
`0 -> 1: 3` and `1 -> 2: 5`, a generation-0 checkpoint at 4 is invalid, while a
generation-1 checkpoint at 4 is eligible. Looking only at the latest cut would
incorrectly reuse the former. Cuts and current history are read together; a
missing intervening transition is an invariant failure, not permission to take
the minimum over an incomplete ledger.

The client owns checkpoint consistency: application state, projection, and claim
must describe the same executed prefix. Once compatibility is established,
persist the new version with the restored checkpoint before continuing. If
another recovery wins the race with subscription, query again using that saved
version. A stale response cannot weaken WS admission. The query proves prefix
preservation under standard recovery's trusted local bookkeeping; it does not
inspect client state or establish trust after a software bug. A new era requires
the [manual projection recovery procedure](projection-replay.md#client-checkpoints).

### Era baseline

Setup publishes a complete baseline only after its artifact is durable:
application count `K`, accounted L1 stop block `C`, starting batch nonce, and
history identity. Ordinary direct ordering and accepted-batch scanning begin
after `C`. The baseline's metadata survives root invalidation and artifact GC;
no padding inputs or separate mutable processing cursor represent its prefix.

Cockroach recovery drains queued directs through `C`, including young directs
that the canonical scheduler has not executed yet. Its output is a stable
restart baseline; its bytes need not equal canonical state at block `C`.
A later accepted batch snapshot supplies the first comparison checkpoint in that era.
Genesis supplies the trusted block-zero comparison state. The
[rebuild guide](../recovery/cockroach.md) owns checkpoint requirements and the
fixed stopping boundary.

## Consumers of checkpoints

| Consumer | Starting point and continuation |
|---|---|
| Native restart | Load the newest surviving batch snapshot, or baseline, check the engine's count against its row, then replay current application inputs. |
| Sequencer replica | Download `/latest_snapshot`, restore its application state, and subscribe using the matching history claim. This follows optimistic execution. |
| Watchdog | Start from independently trusted canonical machine state and replay L1. Compare at the sequencer's accepted checkpoint; the replica feed does not establish independent trust. |
| Application projection | Reconstruct additional transfer/order history using the era's historical L1 prefix, then join the application feed at the immutable baseline. The [projection replay contract](projection-replay.md) owns its checkpoint preparation and handoff. |

A batch-close snapshot is identified by its local batch identity, not just its
count or nonce. Recovery can reuse a nonce and empty batches can repeat a count.
Acceptance derives the comparison point without modifying the artifact.
Selection must first choose the required accepted batch and then require its
snapshot; a missing one cannot justify falling back to an older comparison.
The [snapshot lifecycle](../snapshots/lifecycle.md#acceptance-and-comparison)
explains block-boundary comparison and rollback retention. The
[watchdog guide](../watchdog/README.md) explains independent verification.

## Replica bootstrap and resume

1. Download `GET /latest_snapshot`. The tar archive contains `info.toml` and
   the complete opaque application `state` file or directory.
2. Retain its `X-History-Era`, `X-Recovery-Generation`, and
   `X-Executed-Input-Count` headers with those bytes. Restore the application and
   verify its count against that header. The archive alone does not carry the
   complete subscription claim.
3. Subscribe with the matching era, generation, and next-input count. Snapshot
   selection, headers, and lease share one transaction; recovery during the
   download can still invalidate the claim before subscription. Rebootstrap
   if the server refuses that old era; within the same era, a compatible saved
   checkpoint can instead be selected through `/history`.
4. Require each entry's offset to equal the application's current count, then
   execute it through the shared execution boundary. Successful application
   advances the count by one. Persist the history identity with the replica's
   state so that a later resume cannot combine different histories.
5. After an ordinary disconnect, reconnect with that saved identity and the
   actual count. A generation mismatch permits the compatibility procedure
   above. An unavailable prefix or lack of a compatible checkpoint requires a
   current snapshot. A count ahead of the server's head is an invalid claim to correct.

The [Rust SDK](../../sdk/rust-client/src/lib.rs) returns a `HistoryClaim` with
its snapshot response and requires an explicit claim for subscriptions. The
consumer owns restore, persistence, and reconnect. Fetching fresh identity
metadata alone cannot authorize old application state; an explicit compatibility
result can authorize a saved prefix within the same era.

One durable page reader handles both backlog and live delivery, so there is no
separate cursor to switch at the live boundary. Each page reads identity,
bounds, and rows in one SQLite transaction. Interior gaps and invalid source
context fail loudly. Page size and send queues bound memory; available backlog
has no total replay cap. The [API contract](../../README.md#api) owns exact
resource limits and handshake errors.

History replacement happens across a process boundary, after existing
subscriptions end. A session's mandatory claim therefore binds all its events;
there is no per-event generation or guaranteed farewell message. Revisit this
assumption if history can change within an admitted process or multiple local
writers become supported.

## Code and validation map

| Boundary | Code and tests |
|---|---|
| Coordinates and claim ordering | [`sequencer-core/src/history.rs`](../../sequencer-core/src/history.rs) — identity before position, inclusive head, checked counts. |
| Coherent application pages | [`storage/egress/canonical.rs`](../../sequencer/src/storage/egress/canonical.rs) and its tests — source context, nonzero baselines, replacement offsets, concurrent recovery, gaps and SQL limits. |
| Replay followed by live delivery | [`l2_tx_feed`](../../sequencer/src/egress/l2_tx_feed/) — bounded deep replay, identity refusals, shutdown and persistent faults; [`catch_up.rs`](../../sequencer/src/ingress/inclusion_lane/catch_up.rs) for native replay. |
| Artifact and claim association | [`snapshot_endpoints.rs`](../../sequencer/src/integration_tests/snapshot_endpoints.rs) — headers, restore, archive contents, and lease lifetime. |
| Checkpoint compatibility | [`storage/history.rs`](../../sequencer/src/storage/history.rs) and recovery tests — immutable cuts, complete lineage, and transaction rollback; [`recovery_compatibility.rs`](../../sequencer/src/integration_tests/historical_bootstrap/recovery_compatibility.rs) — saved projections across missed recoveries and HTTP lookup/WS admission races. |

The [integration validation record](../review/2026-09-16-track3-validation.md)
records wallet replica and canonical-machine evidence. Remaining consumer and
deployment gates belong to the [Track 3 plan](../plans/2026-07-track3-feed-replay-design.md).
