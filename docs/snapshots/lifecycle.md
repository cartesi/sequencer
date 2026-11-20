# Snapshot lifecycle

Snapshots preserve application state at a declared execution boundary. Their
contents and boundary are immutable. L1 acceptance is a separate durable fact:
there is no promotion operation and no mutable finalized pointer.

Keep three boundaries separate: the engine embeds its L2 tx count;
SQLite associates an artifact with a batch or the era baseline; safe L1
acceptance selects a comparison checkpoint. A count alone does not establish
acceptance. The [history guide](../protocol/application-history.md)
explains these coordinates.

## Artifact and boundary

Every artifact is a sequencer-owned directory:

```text
dumps/<id>/
  info.toml   format_version, next_batch_nonce
  state       opaque app-owned file or directory
```

The application owns `state`, the prefix passed to `Application::create_dump`
and `Application::from_dump`, and must support independent restoration after
source deletion. Its canonical comparison file may be only one part of that
artifact. The [Application contract](../protocol/application-contract.md#6-checkpoint-lifecycle)
owns durability and engine behavior; [format.md](format.md) describes the wallet.

SQLite holds one row per artifact and the boundary it holds:

| Fact | Meaning |
|---|---|
| `snapshots` | Artifact path, immutable application count, and the batch's nonce |
| Baseline snapshot (`batch_nonce IS NULL`) | The application at the end of the era's stop block, at its rebuilt history end |
| `batches.accepted_input` | The safe input whose scheduler-accepted, content-matched landing accepted the batch |

The application count is the next canonical input position, never a physical
SQLite cursor. The batch's nonce disambiguates empty batches, which can share
an application count.

## Creation and restart

Every batch close creates a snapshot. The lane makes the artifact
and metadata durable **before** one transaction seals the batch and registers
its snapshot. A failed database commit leaves an orphan directory; startup
sweeps it. A committed close therefore has its required snapshot.
Creation runs on the lane, so user ops queued behind a close wait for it. Each
close logs the creation time as `snapshot_ms`, at DEBUG, or at WARN once it
reaches half the `POST /tx` acknowledgement budget.

Setup similarly makes the baseline artifact durable before one transaction
inserts the era row (which marks setup complete) and registers the baseline
snapshot;
it opens no root frame, which `run`'s startup does
([cockroach recovery](../recovery/cockroach.md#what-setup-does)). The rows of
the era's rebuilt history, from its base count to the baseline, have no
snapshots: they live in `rebuilt_history`, which the feed serves and restart
never replays.

Restart requires the newest closed batch's snapshot, or the baseline when no
batch is closed. A missing required snapshot fails loud.
The selected artifact and stored application count come from one row.
Catch-up checks the restored engine's count and replays
application inputs from that count.

## Acceptance and comparison

The newest accepted batch determines the comparison checkpoint. Its snapshot
must exist; storage refuses a missing required row instead of falling back to an
older snapshot. Before the era's first accepted batch, the baseline is the
comparison checkpoint: the application's state at the end of the stop block is
the canonical machine's state there, whatever the base, so it is served with
the stop block as its inclusion block, the rebuilt history end as its count,
and the anchor as its next batch nonce. After a cockroach recovery, this is the
watchdog's first comparison of the new era. Acceptance already includes
scheduler validation and local content identity, so merely observing an
own-sender L1 input is insufficient.

A persisted canonical-divergence marker refuses accepted-checkpoint selection,
including when a matching batch precedes a divergent acceptance in the same
block. Selection checks the marker in the same read transaction, which a
download runs under the lease registry's lock. Finalized endpoints return HTTP
503 before consulting conditional cache headers; they do not fall back to an
older artifact.

This selection is independent of the lane's L1 reconciliation cursor. A crash
between reader ingestion and lane reconciliation cannot miss a promotion or
repeat one: acceptance is already durable, and the query derives the result.

The current watchdog compares at the selected accepted batch's **L1 block
boundary**. Every batch has a snapshot, and reader ingestion accounts for a
complete safe block before publishing its accepted prefix. Selecting the latest
accepted batch therefore includes later accepted batches in the same block.
On the canonical side, no later input in that block changes the application
state, even after faulty sequencing. The backstop already ran at the batch's
own input and the fridge is ordered by block, so nothing becomes overdue later
in the block; directs only enqueue; and rejected or stale batches change
nothing. A fridge bound that executes directs on arrival breaks this for the
block where it overflows
([register](../review/register.md#bound-the-canonical-direct-input-fridge)).
Sparse snapshots are a future policy change: an older artifact cannot be
labelled as that block's final state when a later accepted batch in the block
has no artifact. That change must settle comparison positioning and replay
retention together.

## HTTP and recovery exports

These endpoints are operator-only and require network isolation:

- `/latest_snapshot` streams a tar archive containing `info.toml` and the complete
  opaque `state` artifact. It may describe optimistic state.
- `/finalized_state` streams only the canonical comparison file for the latest
  accepted checkpoint. `/finalized_state/inclusion_block` provides its block and
  L2 tx count for the watchdog's idle poll, and
  `/finalized_state/digest` adds the file's SHA-256 under the same lease.
- `/finalized_snapshot` streams a complete recovery tar archive containing
  `info.toml`, `state`, and a generated `checkpoint.toml` acceptance receipt.
  The receipt supplies the accepted inclusion block and next batch nonce.

Snapshot bodies carry `X-History-Era` and `X-Executed-Input-Count`.
Acceptance endpoints also carry `X-Inclusion-Block`.
The metadata and selected artifact come from one read, taken under the lease.
Restoring `/latest_snapshot` and subscribing with its history claim gives the
consumer a coherent snapshot-plus-suffix starting point.

**Operator backup:** `/finalized_snapshot` exports an accepted checkpoint with
its receipt, without modifying local files. A bare local `dumps/<id>/`
directory is only a restore artifact and has no acceptance receipt. No command
consumes the archive: a recovery rebuilds from a canonical machine, never from
a sequencer artifact ([cockroach recovery](../recovery/cockroach.md)).

## Retention, leases, and crash safety

Garbage collection retains:

1. The newest accepted checkpoint, or the baseline before first acceptance.
2. Every snapshot beyond the accepted frontier. An intermediate optimistic
   batch may become the next accepted head before its successors do.
3. Every artifact with an active reader lease.

Older accepted artifacts are collectible.
The baseline artifact can be retired after an accepted checkpoint replaces it
as the comparison checkpoint; the era's immutable baseline metadata and its rebuilt history
remain in SQLite.

Leases live in the serving process, in one registry of per-snapshot counters
that `run` shares between the egress handlers, the lane's GC and startup
hygiene. A lease means something only while this process serves a body, and
only `run` serves and collects, under the data directory's exclusive process
lock; nothing durable records a lease, so a restart starts with none. A second
process that served or collected would need a shared lease record: revisit this
if one ever does. The registry follows one protocol:

- **Acquire** runs on a blocking thread over a read-only connection, which it
  opens and makes load the schema first. It locks the registry, then selects in
  one deferred read transaction (the finalized endpoints' divergence check
  included), ends it, increments the snapshot's counter and unlocks. The
  selection's transaction begins under the lock, so what it reads postdates any
  GC commit.
- **GC** begins its IMMEDIATE write transaction first, then locks the registry,
  selects its candidates, drops every candidate whose counter is above zero,
  removes the zero entries from the registry, deletes the remaining rows and
  commits. It unlocks only after the commit returns: before that, an acquire
  would still read the rows being deleted and lease a snapshot whose artifact
  is about to go, and a missing artifact is terminal.
- **Release** is the guard's drop: it decrements the counter, takes no lock
  and does no I/O, so it is safe on an async worker thread. A zero entry stays
  until the next GC removes it.

GC takes SQLite's writer lock, then the registry; acquire takes the registry,
then only reads. WAL readers never take the writer lock, and any wait a reader
does meet is bounded by its 50 ms busy timeout, so the two cannot deadlock.

An HTTP body retains the lease through completion or disconnect. Archive
production also retains ownership until it stops reading the source, so dropping
the network stream cannot race producer reads against filesystem deletion.

GC selects and deletes eligible database rows in one transaction, then removes
the enclosing dump directories recursively. This filesystem operation disposes
of all checkpoint resources. Filesystem deletion failure leaves a harmless
orphan for the startup sweep. The reverse ordering would leave a durable row pointing at missing
state and is forbidden. Startup, before the listeners are bound, checks the
accepted checkpoint's `info.toml` and format version, collects obsolete rows,
and sweeps orphan directories. The sweep matches
artifacts by filesystem identity (device and inode), not by path, because setup
and run can spell the same directory differently; it resolves every retained
artifact before deleting anything and stops if one cannot be resolved
([`startup_hygiene.rs`](../../sequencer/src/commands/run/startup_hygiene.rs)).
Application restoration runs afterward in the launched inclusion lane, before
processing new user ops; the metadata check does not validate the application
bytes. Missing or corrupt referenced artifacts also fail loud when read or
restored.
