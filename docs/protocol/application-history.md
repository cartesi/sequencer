# Application history and replay

Application history is the sequencer's current execution order: executed user
operations and external direct inputs. It contains an optimistic suffix that
recovery may replace. A stable offset identifies an input only together with
its history version; receiving it does not establish L1 acceptance.

This document owns history coordinates, recovery boundaries, and replica
bootstrap. The [Application contract](application-contract.md) owns execution
and the count; the [snapshot lifecycle](../snapshots/lifecycle.md) owns artifact
publication, selection, and retention; the [README API](../../README.md#api)
owns routes, wire fields, and refusal codes.

## Progress, ordering, and acceptance

These facts answer different questions:

| Fact | Meaning |
|---|---|
| `L2TxCount` | Number of L2 transactions already executed; at count `N`, entry `N` executes next. |
| Latest surviving frame's `safe_block` | Complete L1 interval accounted for by local ordering, bounded below by the era baseline. |
| `safe_accepted_batches` | Safe L1 landings accepted by the scheduler rules and matched to local sealed bytes. |

A frame can account for an empty interval or only batch envelopes, advancing
L1 accounting without executing an application input. An accepted batch can
confirm existing execution without adding an input. Empty batches can share
an application count. The count substitutes for neither the L1 accounting
boundary nor the acceptance facts.

`safe_inputs` retains all InputBox observations, including batch envelopes.
An era's history lives in two tables, joined at its
[rebuilt history end](#era-baseline). `rebuilt_history` holds what setup
rebuilt from L1, from the base count up to that end; `application_inputs`
holds the live rows after it. Each live row has a mandatory pre-execution
offset, an owning batch/frame, and exactly one reference to a user op or
external direct input. Payloads remain in those source tables. Included
business failures, underfunded ops and
malformed-direct no-ops advance the count; refusals and envelopes do not. A
direct whose payload is over the application's cap never executes, as in the
[canonical fridge](scheduler-semantics.md#the-fridge), so it gets no row and no
offset: one storage predicate filters it from every query that enumerates
directs to execute.

Restart and egress read this same sequence. Replay runs each stored user op
through the shared executor with its recorded fee and frame safe block, without
admission; the executor's nonce check still runs, and a stored op it skips is
an impossible state that stops the lane. A direct replays with its inclusion
block, input index and timestamp, which are application inputs. Transaction
hashes are provenance, not transition inputs. The
[execution contract](application-contract.md#the-execution-methods) defines the
shared executor.

## Identity and available history

A `HistoryClaim` combines:

- **Era**: a UUIDv4 created by setup, identifying one local history.
- **Recovery generation**: a revision within that era, advanced atomically
  whenever automatic recovery invalidates at least one valid batch.
- **Next input**: the application's actual L2 tx count.

If the era's base count is `K` and the current head is `H`, available entries
occupy `[K, H)`. A claim at `H` waits for future entries; a claim below `K` or
above `H` is refused. Identity is checked before position. Equal counts do not
by themselves authorize resuming a different era or generation. Two lookups
can rebind a saved state to the current version: the compatibility query
below, within an era, and the [adopt rule](#adopting-a-new-era), across eras.
WS itself requires exact identity.

Automatic recovery invalidates a batch suffix, removes its live application
rows, advances the generation, and opens the replacement Tip in one transaction.
It never reaches the rebuilt history, which comes from safe blocks.
Replacement inputs reuse suffix offsets under the new generation. Original L1,
batch, frame, and user-op source records remain; the invalidated flattened
sequence is not separately retained. A repair that invalidates nothing leaves
the generation unchanged. The [recovery guide](../recovery/README.md) owns
repair selection and guards.

### Checkpoint compatibility after standard recovery

Each generation transition records the surviving application count after the
invalidated rows are removed and before the replacement Tip adds any directs.
This cut commits atomically with invalidation and reopening, and it is the
generation advance: the current generation is the greatest cut's (0 before
any). An invalidated empty batch still creates a transition with the old
head as its cut; a repair that invalidates nothing creates neither. The cuts are
immutable and retained for the era's lifetime.

For a checkpoint saved in generation `g`, `/history` returns the current version
at generation `G`, head `H`, and preserved count:

```text
P = min(H, cut[g+1], ..., cut[G])
```

For `g = G`, `P = H`. Every cut is at or past the rebuilt history end, since
recovery removes only live rows. A checkpoint at count `X` is eligible to
resume under the returned version exactly when `K <= X <= P`. Check each saved checkpoint using
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
inspect client state or establish trust after a software bug. A new era is
crossed by the [adopt rule](#adopting-a-new-era) instead.

### Era baseline

Every setup starts from a base, genesis or a stored canonical machine, and
rebuilds the history from L1 through a stop block it pins after flushing the
submitter's wallet
([what setup does](../recovery/cockroach.md#what-setup-does)).
Once the baseline's artifact is durable, setup publishes the era in one
transaction: one immutable row in the `era` table, whose existence is what
makes the database set up, and the baseline snapshot. It publishes:

| Field | Value |
|---|---|
| Base count `K` | The base's L2 tx count, where the era's history starts; `/history` reports it as `available_from` |
| Stop block `C` | The safe block setup pinned after the flush; `/history` reports it as `l1_stop_block` |
| Rebuilt history end `E` | The L2 tx count at the end of block `C`, where the live rows start; `/history` reports it as `rebuilt_history_end` |
| Anchor `N'` | The scheduler's next batch nonce at the end of block `C`, which the era's first batch carries; `/history` reports it as `next_batch_nonce` |
| First queued input | The InputBox index of the oldest direct still queued at the end of block `C`, or the input count through `C` when none is; not on the wire |
| Baseline snapshot | The application's state at count `E` |

A genesis base has `K = 0` and still runs the rebuild, so a deployment whose
L1 history executed transactions before its stop block starts with `E > 0`. The baseline is
the application's state at the end of block `C`, which the canonical machine
reaches too, so until a batch of the era is accepted the sequencer serves it
as the comparison checkpoint: inclusion block `C`, count `E`, next nonce `N'`.

Setup opens no batch. `run`'s first frame drains the directs still queued at
`C`, from the first queued input on, as the canonical scheduler's next frame
does; a cascade that tears every batch of the era drains from there again.
Accepted-batch scanning begins after `C`. The era row survives root
invalidation and baseline artifact GC.

### The rebuilt history

`rebuilt_history` holds one row per transaction that executed while setup fed
the scheduler the inputs from the base's position through the last input at
or below `C`, keyed by its count
([what setup does](../recovery/cockroach.md#what-setup-does)).
A direct row keeps its input index, and the feed takes its sender, payload,
block, timestamp and transaction hash from `safe_inputs`. A user op row keeps
its sender, nonce, fee, data and safe block. Each row keeps the scheduler's
next batch nonce when it handed the row out: the batch whose frame holds or
drained it, or the next batch to come. A user op the executor skipped has no
row.

Setup is the table's only writer, and only before it publishes the era: the
schema refuses an insert after that, any update, any deletion, a count that
does not follow the last, and a direct out of input order. The era's `K` and
`E` are exactly the span of the rows.

### Adopting a new era

A cockroach recovery replaces the era, so a client's claim from the old one
is refused with `ERA_CHANGED`. A client holding state at count `X` adopts the
new era at that count when

```text
K <= X <= E
```

and then subscribes with the current version and `next_input = X`.
[`HistoryInfo::adopt_claim`](../../sequencer-core/src/history_api.rs) is the
check, and the [Rust SDK](../../sdk/rust-client/src/history.rs)'s
`adopt_current_era` reads `/history` and applies it. The server adds no check
of its own: the adopted claim carries the current version, which the feed
admits like any other.

| Refusal | Meaning | Remedy |
|---|---|---|
| `BelowBase` (`X < K`) | The era's base is newer than the client's state | The operator recovers again from an older base, or the client starts over from a current snapshot |
| `PastRebuiltEnd` (`X > E`) | The client's state holds rows the old instance served that never landed by `C` | The client restores an older state |

Rows past `E` are the new era's live rows, served after the stop block, so an
old-era state there is not canonical even when the head has passed `X`.

### What the count checks miss

The adopt rule compares counts, not rows. Two faults pass it:

- **A base machine that is not canonical.** Setup trusts its base. The
  watchdog's first comparison of the era's baseline exposes it.
- **A client state that is wrong below its count**, such as one built on rows
  the old instance served below `E` that the rebuild replaced. Only the
  client's own check exposes it: subscribe from `K` and compare each row up to
  `X` with the one it stored
  ([the client's playbook](../recovery/cockroach.md#the-clients-playbook)).

## Consumers of checkpoints

| Consumer | Starting point and continuation |
|---|---|
| Native restart | Load the newest surviving batch snapshot, or the era baseline at `E`, check the engine's count against its row, then replay the live rows after it. |
| Sequencer replica | Download `/latest_snapshot`, restore its application state, and subscribe using the matching history claim. This follows optimistic execution. |
| Watchdog | Start from independently trusted canonical machine state and replay L1. Compare at the sequencer's accepted checkpoint; the replica feed does not establish independent trust. |
| Application projection | Restore its own checkpoint at a count the era serves, then follow the feed ([the client's playbook](../recovery/cockroach.md#the-clients-playbook)). The feed starts at the base count; the rows below it exist only on L1. |

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
   download can still invalidate the claim before subscription. If the server
   refuses that old era, [adopt](#adopting-a-new-era) the new one at the
   restored count, or rebootstrap; within the same era, a compatible saved
   checkpoint can instead be selected through `/history`.
4. Require each entry's offset to equal the application's current count, then
   execute it through the shared execution boundary. Successful application
   advances the count by one. Persist the history identity with the replica's
   state so that a later resume cannot combine different histories.
5. After an ordinary disconnect, reconnect with that saved identity and the
   actual count. A generation mismatch permits the compatibility procedure
   above, and an era mismatch the adopt rule. An unavailable prefix or lack of
   a compatible checkpoint requires a current snapshot. A count ahead of the
   server's head is an invalid claim to correct.

The [Rust SDK](../../sdk/rust-client/src/lib.rs) returns a `HistoryClaim` with
its snapshot response and requires an explicit claim for subscriptions. The
consumer owns restore, persistence, and reconnect. Fetching fresh identity
metadata alone cannot authorize old application state; an explicit compatibility
result can authorize a saved prefix within the same era, and `adopt_current_era`
one from an older era.

One durable page reader handles both backlog and live delivery, so there is no
separate cursor to switch at the live boundary. It serves the rebuilt rows in
count order up to `E`, then the live rows, and a page may span that
seam. Each page reads identity, bounds, and rows in one SQLite transaction.
Interior gaps, a missing rebuilt row below `E`, and invalid source context
fail loudly. Page size and send queues bound memory; available backlog
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
| Coherent application pages | [`storage/history/canonical.rs`](../../sequencer/src/storage/history/canonical.rs) and its tests — source context, the rebuilt/live seam, nonzero baselines, replacement offsets, concurrent recovery, gaps and SQL limits. |
| Rebuilt history | [`storage/rebuilt_history.rs`](../../sequencer/src/storage/rebuilt_history.rs) and its tests — contiguous counts, directs in input order, immutability, sealing at publication; [`commands/setup/rebuild.rs`](../../sequencer/src/commands/setup/rebuild.rs) — the export check, and the rebuild executing as it goes. |
| Era adoption | [`history_api.rs`](../../sequencer-core/src/history_api.rs) — `adopt_claim` and its range; the [SDK](../../sdk/rust-client/src/history.rs)'s `adopt_current_era`. |
| Replay followed by live delivery | [`l2_tx_feed`](../../sequencer/src/egress/l2_tx_feed/) — bounded deep replay, identity refusals, shutdown and persistent faults; [`catch_up.rs`](../../sequencer/src/ingress/inclusion_lane/catch_up.rs) for native replay. |
| Artifact and claim association | [`snapshot_endpoints.rs`](../../sequencer/src/integration_tests/snapshot_endpoints.rs) — headers, restore, archive contents, and lease lifetime. |
| Checkpoint compatibility | [`storage/history.rs`](../../sequencer/src/storage/history.rs) and recovery tests — immutable cuts, complete lineage, and transaction rollback; [`recovery_compatibility.rs`](../../sequencer/src/integration_tests/recovery_compatibility.rs) — saved projections across missed recoveries and HTTP lookup/WS admission races. |

The [integration validation record](../review/2026-09-16-track3-validation.md)
records wallet replica and canonical-machine evidence. Remaining consumer and
deployment gates are in the [register](../review/register.md#integration-work-owned-elsewhere).
