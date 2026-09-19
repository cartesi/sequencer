# Feed and Replay Protocol (Track 3)

**Status: implemented in the current application-history redesign.**
The former physical-rowid feed and sparse execution mapping are superseded.
The [README](../../README.md) owns the wire contract; the
[application-history design](application-history.md) owns storage and recovery
boundaries. This document records the consumer workflow and remaining gates.

## 1. Consumer workflow

1. Download `GET /latest_snapshot` using the SDK. Its tar body contains the
   complete immutable restore artifact (`info.toml` and the opaque `state`
   file or directory).
2. Restore the application and verify its executed-input count against
   `X-Executed-Input-Count`. Keep the matching `X-History-Era` and
   `X-Recovery-Generation` headers with those bytes and the restored state.
3. Subscribe with that `HistoryClaim`: mandatory `era_id`,
   `recovery_generation`, and `next_input` query fields.
4. Apply each entry whose `offset` equals the application's current count.
   Successful execution advances the count by one. Persist identity with the
   replicated state before using it for a later resume.
5. After an ordinary disconnect, reconnect with the saved identity and actual
   next-input count. On an era or generation refusal, discard the incompatible
   replica and bootstrap from a current snapshot.

A fresh identity lookup cannot authorize old state. The SDK requires an explicit
claim on every subscription; it does not silently change identity on reconnect.
There is no separate history-version endpoint.

Snapshot selection, its count, history version, and GC lease share one storage
transaction. The lease lasts through response completion or disconnect. A
recovery between snapshot acquisition and subscription is handled by refusing
its old claim, without blocking history advancement during transfer.

## 2. Coordinates and storage

- `safe_inputs.safe_input_index` names a source L1 InputBox event. It includes
  scheduler batch envelopes and direct application inputs.
- `application_inputs.offset` is an `ExecutedInputCount`: an application at
  count `N` consumes entry `N` next. Every row has an offset, owner frame, and
  exactly one user-op or source-L1 reference. Batch envelopes never appear.
- The immutable era baseline supplies the unavailable application prefix
  `K` and accounted L1 block. Current entries occupy `[K, H)`, where `H` is
  the next application count.
- Standard recovery deletes an invalidated projection suffix and advances
  its generation atomically. Replacement inputs reuse those canonical
  offsets. Raw L1 inputs, batches, frames, and user ops retain their source
  evidence; the invalidated flattened sequence is not separately retained.
- Rebuild creates a new UUIDv4 era and a complete baseline. It does not insert
  padding inputs or preserve a physical replay cursor.

Catch-up and egress use the same named entry and coherent canonical-page
reader. Bounds, identity, and rows are read in one SQLite transaction. Missing
interior rows and invalid payload context fail loudly. Empty requests at the
head do not convert the exclusive boundary back into a SQL row coordinate.

The latest valid frame's `safe_block`, bounded below by the era's L1 baseline,
accounts for the complete L1 prefix. No separate mutable processed-input cursor
is needed. Snapshot application count and L1 accounting are different facts;
see the application-history design for recovery's terminal drain and sparse
checkpoint availability.

## 3. Subscription admission

Validate history identity before position, using one coherent `(era,
generation, K, H)` read:

| Condition | HTTP 409 policy code | Consumer action |
|---|---|---|
| Era differs | `ERA_CHANGED` | Bootstrap from a current snapshot. |
| Generation differs | `STALE_GENERATION` | Bootstrap from a current snapshot. |
| `N < K` | `HISTORY_UNAVAILABLE`, with `available_from` | Bootstrap from an available snapshot. |
| `N > H` | `AHEAD_OF_HEAD`, with `head` | Correct the invalid claim. |
| `K <= N <= H` | Upgrade to WebSocket | Replay inclusively from `N`, then follow the tip. |

Refusals precede the upgrade and all input delivery. The JSON body is also
carried in `X-History-Error`: WebSocket libraries may stop reading a refused
handshake at its headers before the body arrives. Missing or malformed required
query fields receive HTTP 400.

A successful stream carries the existing tagged user-op/direct-input messages
with canonical offsets and persisted context. The mandatory admission claim
binds the session identity; there is no hello frame or per-event generation.
Recovery changes history only across a process boundary, after existing
subscriptions have ended. No generation bus or farewell guarantee is needed.

`N == H` waits normally. Every valid available backlog is replayable: there is
no total 50,000-event cap. Page size, send queue, subscriber count, and inbound
message limits remain bounded independently of backlog depth. The same durable
query handles backlog and live delivery, avoiding a separate handoff cursor.

## 4. Snapshot and watchdog boundaries

`/latest_snapshot` is a replica restore archive. `/finalized_state` remains the
watchdog's application comparison bytes, with its inclusion-block metadata
route. `/finalized_snapshot` exports an accepted recovery artifact and a derived
`checkpoint.toml` receipt. These are operator-infrastructure routes.

The watchdog starts from trusted state and independently consumes L1; snapshot
bootstrap for a tip replica does not replace that trust boundary. Finalized
comparison/export is available only at a supported accepted checkpoint, not at
an invented intra-frame or arbitrary execution position.

## 5. Acceptance evidence and remaining work

The implementation tests cover inclusive pages and source context, exclusion
of envelopes, nonzero rebuild bases, actual suffix invalidation and replacement,
coherent SQLite snapshots during a second writer's recovery, counts beyond the
largest SQL row, and loud interior-gap detection. Feed/API/SDK tests cover
mandatory claims, typed refusals, exact-head waiting and live delivery,
50,001-entry history with bounded pages, ordinary resume, subscriber limits,
terminal storage faults, and cancelled preparation retaining process ownership.
Snapshot integration tests own artifact/header association and restore proof.
The Anvil recovery/WS gate also exercises process restart, generation refusal,
and re-drained direct replay at a reused offset.

The cold-replica E2E restores a nonempty HTTP archive, checks it against a
genesis-fed replica, and holds catch-up behind a barrier while new writes commit.
It checks whole-state, count, and clock agreement through live direct inputs
and user operations, then exercises real stale recovery, claim refusal, and
fresh bootstrap. Canonical-machine gates cover genesis, ordinary execution,
stale recovery, and database reconstruction from an exported checkpoint.
The [validation record](../review/2026-09-16-track3-validation.md) records the
pinned environment and local latency measurements.

Remaining integration gates are concrete consumers and environments:

- Validate the native reference adapter and, when available, the private DEX
  bridge against the application contract and this bootstrap workflow.
- Remeasure submit-to-matching-WS-event latency on the representative deployment.

Revisit resumable snapshot transfer only when artifact size requires it;
retained client checkpoints only when full rebootstrap cost matters; archival
HTTP replay only for an identified consumer. Revisit session fencing if history
can mutate within an admitted process or multiple writers become supported.
