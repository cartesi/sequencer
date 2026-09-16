# Application history and checkpoints

The sequencer keeps L1 observations, application ordering, and batch acceptance
as separate durable facts. L1 inputs include batch envelopes; application
history contains only included user operations and external direct inputs.
Source references provide provenance without defining a mapping between the
two timelines.

## History

`application_inputs` is the current application sequence. Its primary key is
the input's pre-execution `ExecutedInputCount`; each row belongs to a local
batch/frame and references either its user operation or its source L1 input.
Every row executes. Payloads remain in their source tables.

The latest surviving frame's `safe_block` records complete L1 accounting.
Reconciliation covers the whole newly safe interval before committing its new
frame and application inputs. Full-block ingestion and indivisible range
reconciliation make a separate mutable processing cursor unnecessary. Empty
intervals and intervals containing only batch envelopes advance this boundary
without adding application inputs.

Recovery invalidates a batch suffix, removes its current application rows,
advances the history generation, and opens the replacement tip atomically.
Replacement inputs reuse suffix offsets under the new generation. Original
L1, batch, frame, and user-operation records remain available for diagnostics.

## Era baseline

Setup registers a complete baseline after its artifact is durable: application
count `K`, accounted L1 stop block `C`, starting batch nonce, and history identity.
The recovered L1 prefix through `C` is opaque to ordinary operation. Both direct
ordering and accepted-batch scanning begin after it. The baseline metadata
survives root invalidation and artifact garbage collection.

The recovery fold drains queued directs through `C`, including young directs
that the canonical scheduler has not executed yet. Its output is a restart
baseline, without a claim that its bytes equal canonical state at block `C`.
Genesis supplies the trusted block-zero comparison state.

## Snapshots and acceptance

The lane creates a durable snapshot at every batch close. Snapshot registration
and batch sealing commit together. Snapshots reference immutable local batch
identities; a nonce can be reused by recovery. The baseline is a separate
snapshot origin.

Acceptance is derived from complete safe L1 observations, the scheduler's
acceptance rules, and byte identity with the local sealed batch. An accepted
batch confirms existing application history and adds no replay entry.

Checkpoint selection uses these facts directly:

- Restart and replica bootstrap use the newest surviving batch snapshot, or
  the baseline.
- Recovery requires a retained accepted snapshot, or the baseline before the
  first post-baseline acceptance.
- The watchdog compares an accepted checkpoint at the end of its L1 inclusion
  block. Per-batch snapshots make the latest accepted batch's artifact available.

Select the required accepted batch before loading its snapshot: a missing
required artifact is an invariant violation, never permission to choose an
older checkpoint. Divergence blocks publication of a newly derived comparison.

There is no snapshot promotion mutation. Retention keeps the newest accepted
snapshot (or baseline), all valid snapshots beyond the accepted frontier, and
leased artifacts. The baseline bytes can be retired once an accepted artifact
provides the recovery fallback. Artifact creation precedes DB publication;
DB retirement precedes filesystem deletion.

A portable accepted checkpoint includes the application artifact and coherent
sequencer metadata identifying its canonical comparison point and resume nonce.
Acceptance metadata is derived at export; application artifacts stay immutable.
Sparse snapshot creation and intra-block watchdog checkpoints are separate work.

## Replay and egress

Restart and egress share application-only pages beginning at an inclusive input
count. Snapshot bootstrap uses HTTP; one WS stream replays available history
then follows the tip. Subscription claims include era, generation, and next
input count. Wrong identity or unavailable history requires bootstrap; a claim
at the head waits and one beyond it fails. Pages and queues are bounded, while
total replay has no arbitrary catch-up cap.

## Validation boundaries

Exercise prefix exclusion for previously rejected future-nonce batches;
baseline-only restart and repeated root invalidation; envelopes-only frame
advancement; acceptance observed during downtime; empty accepted batches;
snapshot retirement with active leases; atomic suffix replacement; and cold
replica restore followed by canonical replay. The recovery models constrain
admission and batch safety, not the concrete snapshot/GC implementation.
