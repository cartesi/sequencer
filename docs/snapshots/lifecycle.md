# Snapshot lifecycle

Snapshots preserve application state at a declared execution boundary. Their
contents and boundary are immutable. L1 acceptance is a separate durable fact:
there is no promotion operation and no mutable finalized pointer.

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

SQLite separates artifact ownership from snapshot boundaries:

| Fact | Meaning |
|---|---|
| `dumps` | Artifact path and active reader lease count |
| `snapshots` | Artifact's immutable application count and local batch identity |
| Baseline snapshot (`batch_index IS NULL`) | State from which this era starts |
| `safe_accepted_batches` | Scheduler-accepted, content-matched local batch landings |

The application count is the next canonical input position, never a physical
SQLite cursor. The batch identity disambiguates empty batches, which can share
an application count, and replacement branches, which can reuse a nonce.

## Creation and restart

Initially every batch close creates a snapshot. The lane makes the artifact
and metadata durable **before** one transaction seals the batch and registers
its snapshot. A failed database commit leaves an orphan directory; startup
sweeps it. A committed close therefore has its required snapshot.

Setup similarly makes the baseline artifact durable before publishing the
complete era baseline, recovery root when applicable, and setup-completion
facts atomically.

Restart requires the newest valid closed batch's snapshot, or the baseline
when no valid closed batch exists. A missing required snapshot fails loud.
The selected artifact and stored application count come from one row.
Catch-up checks the restored engine's count and replays
application inputs from that count. Invalidated branches are excluded by the
same valid-batch relation used elsewhere.

## Acceptance and comparison

The newest accepted batch determines the comparison checkpoint. Its snapshot
must exist; storage refuses a missing required row instead of falling back to an
older snapshot. Acceptance already includes scheduler validation and local
content identity, so merely observing an own-sender L1 input is insufficient.

A persisted canonical-divergence marker refuses accepted-checkpoint selection,
including when a matching batch precedes a divergent acceptance in the same
block. Selection checks the marker in the same SQLite transaction as its read
and any download lease. Finalized endpoints return HTTP 503 before consulting
conditional cache headers; they do not fall back to an older artifact.

This selection is independent of the lane's L1 reconciliation cursor. A crash
between reader ingestion and lane reconciliation cannot miss a promotion or
repeat one: acceptance is already durable, and the query derives the result.

The genesis baseline is known canonical at block zero. A rebuilt baseline is
**only a restore artifact** until a new batch is accepted. The recovery fold can
pre-execute queued direct inputs through its stop block; that state need not
equal the canonical machine's state at that block. It is never advertised as an
accepted comparison checkpoint solely because recovery produced it.

The current watchdog compares at the selected accepted batch's **L1 block
boundary**. Every batch has a snapshot, and reader ingestion accounts for a
complete safe block before publishing its accepted prefix. Selecting the latest
accepted batch therefore includes later accepted batches in the same block.
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
  executed-input count for the watchdog.
- `/finalized_snapshot` streams a complete recovery tar archive containing
  `info.toml`, `state`, and a generated `checkpoint.toml` acceptance receipt.
  The receipt supplies the accepted inclusion block and next batch nonce.

Snapshot bodies carry `X-History-Era`, `X-Recovery-Generation`, and
`X-Executed-Input-Count`. Acceptance endpoints also carry `X-Inclusion-Block`.
The metadata, selected artifact, and lease are captured in one transaction.
Restoring `/latest_snapshot` and subscribing with its history claim gives the
consumer a coherent snapshot-plus-suffix starting point.

**Operator backup workflow:** download `/finalized_snapshot` and extract the
archive. Supply that extracted directory to `setup --recovery`, with the
checkpoint block recorded in its receipt. The loader checks the receipt against
the immutable dump metadata and configured block. Copying a bare local
`dumps/<id>/` directory is insufficient: it is a restore artifact and has no
acceptance receipt. Export creates the receipt without modifying local files;
startup no longer stamps or repairs acceptance metadata in `info.toml`.

## Retention, leases, and crash safety

Garbage collection retains:

1. The newest accepted checkpoint, or the baseline before first acceptance.
2. Every valid snapshot beyond the accepted frontier. An intermediate optimistic
   batch may become the next accepted head before its successors do.
3. Every artifact with an active reader lease.

Older accepted artifacts and invalidated branch artifacts are collectible.
The baseline artifact can be retired after an accepted checkpoint replaces its
rollback role; the era's immutable baseline metadata remains in SQLite.

Selection, lease acquisition, and GC serialize through SQLite write
transactions. A lease release guard is armed only after its increment commits.
An HTTP body retains the lease through completion or disconnect. Archive
production also retains ownership until it stops reading the source, so dropping
the network stream cannot race producer reads against filesystem deletion.

GC selects and deletes eligible database rows in one transaction, then removes
the enclosing dump directories recursively. This filesystem operation disposes
of all checkpoint resources. Filesystem deletion failure leaves a harmless
orphan for the startup sweep. The reverse ordering would leave a durable row pointing at missing
state and is forbidden. Startup clears leases left by the dead process,
checks the rollback checkpoint's `info.toml` and format version, collects obsolete
rows, and sweeps orphan directories before workers start. The sweep resolves
every retained artifact path before deleting orphans and compares resolved paths
so alternate spellings and symlinks preserve the same artifact. Application
restoration runs afterward in the launched inclusion lane, before processing new
user ops; the metadata check does not validate the application bytes. Missing or
corrupt referenced artifacts fail loud when read or restored; operational
filesystem errors retain their normal error classification.
