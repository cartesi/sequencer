# Cockroach recovery (`setup --recovery`)

When the local DB is lost or has diverged, the operator rebuilds from a trusted
application checkpoint and L1 in a fresh data directory. This is a one-shot
setup operation. [Standard recovery](README.md) instead keeps the database and
invalidates an unaccepted suffix.

The [canonical scheduler fold](../../sequencer-core/src/scheduler/fold.rs)
reconstructs the state. Its terminal drain also prepares the next local frame:
the result is a **resume baseline**, which can be ahead of canonical application
execution at the stopping block. It is not exposed as a finalized comparison
checkpoint merely because the L1 inputs used to construct it are safe.

## Data dictionary

| Symbol | Meaning | Source |
|---|---|---|
| `S` | Trusted application state at checkpoint block `B`. | Restored application dump. |
| `A` | Last executed application safe block in `S`; pending directs are seeded from `(A, B]`. | Application progress in the dump. |
| `B` | Checkpoint inclusion block. | Exported `checkpoint.toml`; must equal the configured checkpoint block. |
| `N` | Scheduler's next batch nonce at `B`. | Exported receipt, checked against immutable `info.toml`. |
| `C` | Post-flush safe stopping block. | Flusher result. |
| `N'` | Next batch nonce after folding through `C`; the new batch-tree anchor. | Fold result. |
| `K` | Application count after the terminal drain; first local history entry is `K`. | Recovered application progress. |
| `E`, `g` | New UUIDv4 era and generation zero. | Atomic completed-baseline registration. |

The checkpoint contract requires `A < B`, checked at load. The sole exception
is the known empty genesis checkpoint (`B = 0`, next nonce and app count zero).
At a non-genesis `A = B`, a direct arriving after the accepted batch in block
`B` can still be pending; the empty `(A, B]` seed would silently omit it.
A recovery export carries a canonical application dump and a separate receipt;
baseline downloads and ordinary optimistic snapshots have no such receipt.

### Trusted checkpoint boundary

The application state, resume nonce, and relationship between the checkpoint and
L1 are operator-trusted. The receipt catches accidentally mixing an artifact,
nonce, or configured inclusion block; it does not independently verify state
against L1. A wrong checkpoint nonce, whether low or high, is outside the
supported model. The content-identity check verifies newly observed acceptance
after the baseline, not the opaque prefix or checkpoint correctness.

An independent verification would need a trusted canonical-machine checkpoint
or replay from an independently trusted origin. The infrastructure subscriber's
application dump is not a substitute for that watchdog trust boundary.

## The procedure: flush → fold → fill

1. **Load the checkpoint.** Restore `S`, read both metadata files, verify their
   nonce agreement and the configured `B`, then derive `A` and require `A < B`
   or the known empty genesis checkpoint.
2. **Flush stranded transactions.** Consume unresolved wallet nonce slots and
   wait for safe finality, obtaining `C`. The lost database cannot supply its
   previous watermark, so the flush uses the provider's pool view. A dropped
   transaction alive elsewhere can evade that view; a later accepted foreign or
   mismatched landing after `C` freezes the new instance and requires another
   rebuild. The trusted provider is fail-stop, not Byzantine.
3. **Re-sync raw L1 inputs.** The safe head `H1` must cover `C`; it can be later.
   Acceptance projection is deferred while the new local tree is absent.
4. **Source disjoint fold ranges.** Seed external directs in `(A, B]`, then
   replay all raw inputs in `(B, C]`. Sender classification excludes own batch
   envelopes from the direct-input seed queue.
5. **Fold and drain.** The scheduler processes the stream with expected nonce
   `N`, then drains every remaining direct through `C`, producing `(S', N')`.
   A young direct still waiting in the canonical scheduler may therefore already
   be present in `S'`. The resumed frame covers it before executing new user ops.
6. **Write the baseline artifact, then publish it.** First create and durably
   sync the immutable dump. One SQLite transaction then creates history
   `(E, 0, K, C)`, sets anchor `N'`, opens its parentless root frame at `C`,
   registers the baseline artifact, and records `setup_complete`. It creates no
   application-input rows for the collapsed prefix.

On the first `run` sync, acceptance starts at `N'` and scans only raw inputs
whose block is **strictly greater than `C`**. Nonce filtering alone is unsound:
a previously rejected future-nonce batch inside the old prefix could match the
new expected nonce. The opaque prefix is never classified again.

Inputs in `(C, H1]` remain available to the inclusion lane. Its next complete
reconciliation executes them once and records application entries beginning at
`K`. Raw L1 input indices and application offsets remain separate coordinates.

## Recovery and retention

`C` is the immutable fallback reconciliation boundary. While valid frames
survive, their latest `safe_block` gives the already-reconciled boundary. If
standard recovery invalidates the original root, it falls back to `C`, so
inputs represented by the baseline are never executed again. Canonical
application rows belonging to invalidated batches are deleted atomically with
the generation change and suffix invalidation.

Startup loads the latest surviving batch-close snapshot, falling back to the
baseline. Admission requires a **rollback-safe checkpoint**: either that
baseline or a retained accepted batch snapshot. An optimistic snapshot alone
cannot satisfy this requirement because a cascade may discard its whole suffix.

Once an accepted post-baseline batch snapshot exists, standard recovery cannot
invalidate it or return to the original baseline. GC can retire the baseline
artifact, subject to download leases. Immutable baseline metadata remains.
Snapshots with equal application counts remain distinct artifacts associated
with distinct batches; acceptance and retention never infer identity from count.

## Crash-safety & idempotency

A completed rebuild refuses another `setup --recovery`. Before completion,
there is no partially registered history or recovery root to resume:

- A failure during artifact creation leaves setup incomplete.
- A failed registration transaction leaves neither baseline history, root,
  anchor update, snapshot row, nor completion marker; any durable file is an
  orphan for cleanup.
- A successful transaction establishes all those facts together. There are no
  nullable baseline coordinates and no physical replay padding.

Early identity pinning and raw L1 ingestion can survive an incomplete attempt.
They do not establish an application-history era. The process lock and setup
admission exclude runtime serving before the complete baseline exists.

## Code map

| Responsibility | Code |
|---|---|
| Load, flush, sync, fold | [`commands/setup/mod.rs`](../../sequencer/src/commands/setup/mod.rs) |
| Durable baseline artifact | [`commands/setup/fill.rs`](../../sequencer/src/commands/setup/fill.rs) |
| Atomic baseline completion | [`storage/lifecycle.rs`](../../sequencer/src/storage/lifecycle.rs) |
| Scheduler fold | [`scheduler/fold.rs`](../../sequencer-core/src/scheduler/fold.rs) |
| Accepted-prefix boundary | [`storage/safe_accepted_batches.rs`](../../sequencer/src/storage/safe_accepted_batches.rs) |
| Snapshot selection and GC | [`storage/snapshot_dumps.rs`](../../sequencer/src/storage/snapshot_dumps.rs) |
