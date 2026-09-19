# Cockroach recovery: rebuild from L1

Cockroach recovery (`setup --recovery`) creates a fresh sequencer starting state
from a trusted application checkpoint and historical L1 inputs. Use it when the
local database is lost or cannot be trusted, including after a sequencer bug has
corrupted its state or produced malformed batches. **Find and fix the bug before
rebuilding.** The operator initiates recovery; the command automates the rebuild.

The procedure is **flush → fold → fill**:

1. **Flush** outstanding submitter transactions and choose a fixed safe L1
   stopping block.
2. **Fold** the input history through the canonical scheduler, starting from the
   trusted checkpoint. Every input receives its normal scheduler treatment:
   accepted batches execute, malformed or rejected batches are skipped, and
   direct inputs are queued and drained. Finally, drain every remaining direct
   input through the stopping block.
3. **Fill** a fresh database with the recovered application state and next batch
   nonce, ready for `run`.

The result has accounted for the whole input prefix and carries no inherited
speculative user-operation suffix. It does not need to reach the moving tip:
normal operation handles inputs after the stopping block.

This is a **resume baseline**. The final drain can execute young direct inputs
that the canonical scheduler still has queued at that block. The resumed frame
covers them before new user operations; the baseline itself is not a canonical
comparison checkpoint at that block.

[Standard recovery](README.md) instead uses the existing database to repair an
optimistic suffix automatically. It assumes the local bookkeeping is trustworthy.

## Recovery readiness before deployment

Canonical-to-native recovery is a required
[application integration capability](../protocol/application-contract.md#7-canonical-recovery-integration).
Each production deployment must have a release-matched export command, a
completed application-specific runbook, and a successful recovery rehearsal.
An incident is an execution of that prepared procedure, not the first attempt
to determine a state layout or assemble recovery metadata.

The application runbook must name:

- The canonical image, native engine, state layout, exporter, and tool versions.
- How to select and preserve a trusted CM checkpoint and its exact L1 boundary.
- The command extracting the application state, count/clock, and next scheduler
  nonce into the bundle accepted by `setup --recovery`, including supported
  checkpoint boundaries and the loader's `A < B` requirement below.
- Artifact locations, access/backup procedures, validation commands, and the
  commands to rebuild, restart, compare, and resume affected readers.
- A rehearsed fallback to an earlier trusted checkpoint or genesis if the
  preferred artifact is unavailable, with measured replay time and disk needs.

The qualifying drill starts with a **non-genesis canonical machine checkpoint**,
pinned deployment data, and L1 access, while the old native database and dumps
are unavailable. Run the actual exporter and restore its output; check the
native application bytes/progress and scheduler nonce against the canonical
source. Exercise directs pending at the checkpoint and inputs arriving after it.
Run `setup --recovery` in a fresh directory, resume sequencing, and compare
against independent canonical execution after a new batch is accepted. The
terminal-drained baseline itself need not equal canonical state at `C`.

Automate this integration check where the artifacts are available, and require
passing evidence for the supported release before production deployment. Record
artifact versions, commands, checks, and timings; repeat when the state mapping,
checkpoint/recovery behavior, or relevant release artifacts change. A native
dump round-trip or a test using shared scheduler fixtures alone does not exercise
the canonical-machine export boundary. The
[Track 6 integration plan](../plans/2026-07-coordination-tracks.md#track-6--dump--application-api-redesign)
tracks the remaining tooling and validation work; this requirement is not an
implemented deployment gate.

## Incident playbook

The objective is a usable, independently trusted starting state. A newer sound
checkpoint reduces replay work; finding the exact first bad execution or the
latest possible sound checkpoint is not a prerequisite for recovery.

1. **Stop and preserve.** Stop the sequencer and prevent automatic restarts.
   Preserve its data directory, logs, and available archives before rebuilding.
   Pause watchdog ticks while copying its selected checkpoint, manifest,
   `head.json`, and configuration. Preserve affected client databases and their
   checkpoint metadata separately.
2. **Establish the cause and reference.** Check deployment identity, canonical
   machine image, bootstrap boundary, and the reported comparison boundary.
   A configuration mismatch is different from faulty execution. Fix the cause
   before running the replacement sequencer; retain an independently trusted
   canonical machine or earlier checkpoint as the reference.
3. **Select a sound application checkpoint.** The watchdog's durable head is
   its last successful comparison checkpoint, or its operator-trusted initial
   checkpoint if no comparison succeeded. A failed comparison does not replace
   it; initialization and idle ticks are not successful comparisons. Use that
   canonical state, or independently validate a retained candidate at the same
   exact L1 boundary. Current-state equality can establish a usable application
   state without establishing that all earlier executions were correct.
4. **Prepare a restorable native bundle.** Validate the candidate's restored
   application state, embedded count/clock, and next batch nonce against the
   canonical reference at block `B`. Keep the artifact and its boundary metadata
   together and record how its trust was established. The receipt alone is not
   evidence that a faulty sequencer executed correctly.
5. **Rebuild in a fresh directory.** Use the invocation below. Recovery chooses
   its post-flush stopping block `C`, replays from the trusted checkpoint, and
   publishes a new era. Preserve that baseline artifact and its metadata for
   client alignment before resumed operation can collect it. Resume independent
   watchdog comparison when a new accepted comparison checkpoint is available.

### Obtaining the recovery artifact

The watchdog stores a whole CM, including scheduler state; it does not save a
native `/finalized_snapshot` archive. Use the application's rehearsed canonical
export command to obtain the native bundle, or a retained native archive whose
state and resume metadata can be validated against the canonical reference.
The mapping is required even when it is a direct extraction of a designated
drive. The generic watchdog does not implement that application-specific command.
A comparison file alone need not contain everything an engine requires to restore.

Retain verified native archives outside sequencer GC if they are the intended
recovery source. The watchdog normally prunes its previous CM checkpoint, and
sequencer GC may remove the native artifact from the last passing comparison
after a newer batch is accepted. Downloading `/finalized_snapshot` after an
alarm can return the faulty newer state. The
[backup guide](../watchdog/operator-deployment.md#checkpoint-disk-usage-and-backups)
describes retention; the application runbook supplies the tested conversion.
Use its rehearsed earlier-checkpoint/genesis fallback when the preferred source
is unavailable.

### Application-specific reader state

A reader such as Bart's indexer owns more state than the sequencer application.
Choose its latest checkpoint whose execution provenance and indexing behavior
remain trustworthy after diagnosing the incident. Its boundary may differ from
the sequencer's chosen checkpoint. Compare against an independent reconstruction
of the required projection when needed; matching current balances or positions
does not validate historical transfers, deals, or portfolio records.

Restore that complete client checkpoint and its scheduler continuation metadata,
then replay canonical L1 inputs through the replacement era's `C` and perform
the terminal drain. Establish agreement with the replacement application
baseline, including count `K`, before binding the reader to the new history
claim. If no client checkpoint can be trusted, rebuild its projection from a
trusted origin. A current application snapshot cannot supply omitted history.

The [projection replay contract](../protocol/projection-replay.md) describes the
historical-input API, checkpoint preparation, and handoff metadata. Its reference
test exercises a trusted client checkpoint; the application-specific backup and
incident validation remain integration work. This manual procedure does not
require automated cross-era checkpoint matching.

## Run a rebuild

Select and prepare the trusted checkpoint using the incident playbook above.

The loader requires an application artifact, `info.toml`, and `checkpoint.toml`.
The [recovery export workflow](../snapshots/lifecycle.md#http-and-recovery-exports)
describes this bundle. An export receipt checks metadata agreement; it does not
prove the checkpoint correct. An ordinary optimistic snapshot, a subscriber's
dump, or a bare local `dumps/<id>/` directory is insufficient.

With the deployment's [setup configuration](../../README.md#running) and
batch-submitter signing key configured, use a fresh data directory:

```sh
cargo run -p wallet-sequencer -- setup --recovery \
  --data-dir <fresh-data-dir> \
  --checkpoint-block <block-from-receipt> \
  --checkpoint-dump-dir <extracted-checkpoint>
```

Recovery signs L1 transactions, so the key must match the configured submitter.
After success, start `run` with that same data directory. A completed rebuild
refuses another `setup --recovery`; failures before completion publish no partial
baseline.

## Implementation contract

Read this section when changing checkpoint loading, replay, or baseline
publication. The [scheduler contract](../protocol/scheduler-semantics.md) owns
input interpretation; recovery uses that same scheduler implementation.

### Data dictionary

The replay boundaries are:

| Value | Meaning |
|---|---|
| `S`, `B`, `N` | Trusted checkpoint state, inclusion block, and next batch nonce. |
| `A` | Last executed application safe block reported by `S`. |
| `C` | Fixed post-flush safe stopping block. |
| `S'`, `N'` | Recovered state and next batch nonce. |
| `K` | Application count in `S'`; the first later application input has offset `K`. |

Loading checks the receipt's block against configured `B` and its nonce against
`info.toml`. It requires `A < B`, except for known empty genesis (`B`, nonce, and
application count all zero). At non-genesis `A = B`, a direct arriving after the
accepted batch in block `B` could still be pending but disappear from the seed
range. Checkpoint state and nonce remain operator-trusted; the later
content-identity check does not verify this prefix.

### Flush and stopping block

The lost database cannot supply its previous wallet-nonce watermark. Flushing
therefore depends on the provider's pool view. A transaction dropped there but
alive elsewhere may escape; a later accepted foreign or mismatched landing
freezes the rebuilt instance and requires another rebuild. The provider is
trusted fail-stop, as specified in the [threat model](../threat-model/README.md).

After flushing, raw L1 ingestion must reach at least `C`. It may advance farther,
but the fold stops at `C`. Accepted-batch projection is deferred until the new
baseline and batch tree exist.

### Replay boundaries

Seed the scheduler's pending-direct queue from `(A, B]`, excluding inputs sent by
the batch submitter. Then replay **all raw inputs** in `(B, C]` in L1 order with
expected nonce `N`. Drain the remaining directs through `C` to obtain `(S', N')`.
The disjoint ranges preserve pending directs without executing the checkpoint's
accepted batches again.

On the first `run` sync, acceptance starts at nonce `N'` and scans only blocks
**strictly after `C`**. Nonce filtering alone would let a previously rejected
future-nonce batch in the old prefix be reinterpreted as accepted. Raw inputs
ingested beyond `C` remain for normal reconciliation. Newly executed application
inputs are recorded beginning at `K`; raw L1 indices and application offsets are
separate coordinates.

### Publish the baseline

Write and durably sync the immutable application dump first. One SQLite
transaction then registers a fresh UUIDv4 era at generation zero, count `K`,
boundary `C`, anchor nonce `N'`, a parentless root frame at `C`, the snapshot, and
`setup_complete`. The collapsed prefix creates no application-input rows.

Artifact failure leaves setup incomplete; transaction failure leaves at most an
orphan artifact. Identity pinning and raw L1 ingestion may survive an incomplete
attempt, but the lock and setup admission prevent serving a partial baseline.

`C` remains the fallback reconciliation boundary if standard recovery invalidates
the root. The [history contract](../protocol/application-history.md#era-baseline)
owns these immutable coordinates; [snapshot lifecycle](../snapshots/lifecycle.md)
owns restore selection, rollback-safe retention, and eventual baseline disposal.

## Code map

| Responsibility | Code |
|---|---|
| Load, flush, sync, fold | [`commands/setup/mod.rs`](../../sequencer/src/commands/setup/mod.rs) |
| Durable baseline artifact | [`commands/setup/fill.rs`](../../sequencer/src/commands/setup/fill.rs) |
| Atomic baseline completion | [`storage/lifecycle.rs`](../../sequencer/src/storage/lifecycle.rs) |
| Scheduler fold | [`scheduler/fold.rs`](../../sequencer-core/src/scheduler/fold.rs) |
| Accepted-prefix boundary | [`storage/safe_accepted_batches.rs`](../../sequencer/src/storage/safe_accepted_batches.rs) |
