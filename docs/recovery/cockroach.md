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

## Run a rebuild

Stop the old sequencer and resolve the cause of the failure. Choose a trusted
canonical application checkpoint; a recent one reduces replay work. Its state,
inclusion block, and next batch nonce must agree. After a bug, establish that
trust independently of the faulty local state, using a trusted canonical-machine
checkpoint or replay from a trusted origin.

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
