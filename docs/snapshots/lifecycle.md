# Snapshot Lifecycle

How the inclusion lane takes, promotes, serves, and garbage-collects application
snapshots — and **why** it is built the way it is. This is the companion to
[`format.md`](format.md): that doc defines the on-disk *format* (the
`Application` dump trait and the wallet's wire encoding) and explicitly scopes
the lifecycle out; this doc owns the lifecycle.

Audience: anyone changing the snapshot path, the inclusion lane's
safe-frontier processing, or recovery. Read [`AGENTS.md`](../../AGENTS.md) first
for the sequencer/scheduler duality and the optimistic-confirmation model.

> **Code is the source of truth.** This doc explains *why*; the symbol names
> below (functions, tables, columns) can drift. Verify them against the current
> code before relying on them.

## Invariants & landmines

Read these first — the load-bearing rules, and the things that look wrong but
aren't. Section references point to the full reasoning below.

**Invariants** (hold at all times):

- **Always-load.** A finalized snapshot exists before the lane starts (genesis
  at cold start). Absence is a bug, surfaced fail-loud as
  `CatchUpError::NoSnapshot` — never a branch the happy path handles. (§2)
- **Tip exists before the lane.** A valid open Tip exists when the lane starts:
  `ensure_open_tip` opens the genesis Tip on a fresh DB (after recovery's safe-head
  sync, before the lane); recovery reopens it atomically across cascades. The lane
  loads the resulting head from storage (fail-loud if absent), so it only ever
  *loads* — it never branches on tip existence or initializes one. (§7)
- **A committed promotion implies an advanced drain.** Promotion is folded into
  the drain's transaction (`close_frame_only_promoting`), so the two commit
  together — this is what makes a crash safe. (§5, §6)
- **No dangling row.** No `dumps` row references a missing directory: create the
  file before the row; delete the row before the file. (§7)
- **One resume checkpoint.** The same row supplies both the `from_dump` prefix
  and the replay offset, so loaded state and replay cursor can't drift. (§4)
- **Snapshot `l2_tx_index` is the global valid replay head**, not the batch's
  own last offset — so an empty batch doesn't reset catch-up to genesis. (§3)
- **Storage is SQLite-only; the lane owns FS cleanup.** That boundary *is* the
  GC crash-ordering guarantee — don't push filesystem work into
  `storage/snapshot_dumps.rs`. (§7)

**Landmines** (deliberate, or foot-guns):

- **Per-range promotion intentionally skips intermediate blocks** — only the
  range's max nonce is promoted. Sound because nonces land monotonically and the
  skipped checkpoints were never observable. Not a bug. (§5)
- **`Storage::promote_finalized` (standalone) is `pub`, but production must not
  call it.** Promoting outside the drain transaction re-opens the wedge (§6); it
  exists only for test setup. Production promotes via `close_frame_only_promoting`.
- **Several snapshot `Storage` methods are `#[cfg(test)]`** — non-atomic
  siblings of the atomic production methods (`gc_dump_rows` vs
  `gc_unreferenced_dumps`; `acquire_dump_lease` vs `acquire_*_lease`;
  `clear_pending_dumps` vs `clear_pending_dumps_in`). Use the atomic ones in
  production.
- **GC runs after a promotion, not on a timer.** Don't move it back to the idle
  path — it starves under load. (§7)
- **A *missed* promotion on crash is fine; a *re-promotion* is the wedge.** That
  asymmetry is the whole point of §6 — don't "optimize" by promoting eagerly or
  separately from the drain.
- **The HTTP lease is held by a drop-guard inside the response body, not
  released after the stream.** A linear acquire → stream → release would *leak*
  the lease on client disconnect — code after the `.await` doesn't run when the
  body future is cancelled. (§7)

## 1. Purpose & model

A snapshot is a durable copy of the application's canonical state at a known
point in the L2-tx stream. It exists for three consumers:

- **Catch-up** (lane startup): instead of replaying the entire L2-tx history,
  the lane loads the freshest snapshot and replays only the tail after it — a
  single *load-then-replay* path.
- **The watchdog** (operator): polls the **finalized** snapshot to verify the
  sequencer's state against an independent canonical machine advanced through
  L1.
- **Indexers** (operator): fetch the **latest** snapshot, then subscribe to the
  L2-tx feed from that snapshot's offset.

Three SQLite tables back it (`storage/migrations/0001_schema.sql`):

| Table                | Holds                                              |
|----------------------|----------------------------------------------------|
| `dumps`              | `(id, prefix, lease_count)` — one row per on-disk dump directory |
| `pending_snapshots`  | `(nonce, dump_id, l2_tx_index)` — snapshots of closed-but-not-yet-L1-confirmed batches |
| `finalized_snapshot` | single row `(dump_id, inclusion_block, l2_tx_index)` — the latest L1-confirmed state |

`prefix` is an opaque directory the `Application` owns (see [`format.md`](format.md));
the lifecycle code never looks inside it except through the trait. The split
between **pending** and **finalized** mirrors the sequencer's optimism: a batch
closes off-chain (soft) → its snapshot is *pending*; the batch lands safe on L1
→ its snapshot is *promoted* to finalized.

The storage half lives in `storage/snapshot_dumps.rs` (SQLite only — no
filesystem); the lane half in `ingress/inclusion_lane/snapshot.rs` (drives the
trait and FS cleanup). That split is load-bearing for the GC crash-ordering
(§7).

## 2. The always-load invariant

**A finalized snapshot always exists by the time the lane starts.** The runtime
guarantees it in `Workers::spawn` (`runtime/workers.rs`): on cold start
`ensure_finalized_snapshot` consumes the genesis `Application`, writes it as a
dump, and registers it directly as finalized (bypassing pending); on warm start
it is a no-op. This gives catch-up a single unconditional path — there is always
*something* to load — and turns "no snapshot" into a violated invariant surfaced
fail-loud as `CatchUpError::NoSnapshot`, never a branch the happy path handles.

## 3. Taking a snapshot at batch close

When the lane closes a batch (`close_batch_with_snapshot`), ordering is chosen
for crash/error safety:

1. **`create_dump` first, outside any transaction.** The `Application` writes
   and `fsync`s the dump. On failure nothing is sealed — the batch stays the
   open Tip and the lane retries next pass.
2. **One transaction seals the batch, opens the next, and inserts the
   `pending_snapshots` row** (`close_frame_and_batch_with_pending_dump`). A
   committed close therefore *always* has a promotable pending row; a tx failure
   rolls the seal back, leaving only an orphan directory (reaped by the startup
   sweep, §7).

This atomicity closes a "seal succeeds, snapshot insert fails, promotion later
wedges forever on `QueryReturnedNoRows`" gap (commit `0a98cf9`).

The pending row records `l2_tx_index` = the **global valid replay head**
(`valid_ordered_l2_tx_head`, `MAX(offset)` over `valid_sequenced_l2_txs`), *not*
the batch's own last offset. An empty batch (no sequenced txs of its own) thus
inherits the prior head rather than recording genesis — otherwise catch-up from
its promoted snapshot would replay the whole stream and double-apply it.

## 4. The resume checkpoint

On startup the lane selects **one** checkpoint (`catch_up_snapshot`, in
`catch_up.rs`): the latest pending snapshot if any, else finalized. The *same*
row supplies both `A::from_dump(&prefix)` and the catch-up replay offset
(`l2_tx_index`), so the loaded state and the replay cursor can never drift apart
(they come from one row). Loading from a *pending* (not-yet-L1-confirmed)
snapshot is safe because danger-zone recovery clears any cascade-doomed pending
**before** the lane starts (§8) — a surviving pending is either gold or
legitimately in-flight under the optimistic model.

## 5. Promotion

As the lane advances the safe frontier (`maybe_advance_safe_frontier`), it walks
the newly-safe inputs. For each input that is one of *our* batches landing on
L1, `accepted_batch_nonce_at` (reading `safe_accepted_batches`, the
scheduler-acceptance view) yields its nonce. A `BlockObservation` accumulates
the **highest accepted nonce seen in the range and the L1 block it landed in**.
At range close the lane promotes that one `(nonce, block)` target.

`promote_finalized` points the singleton `finalized_snapshot` at the pending
dump for `max_nonce`, carries over its `l2_tx_index`, and **deletes every
pending row with `nonce <= max_nonce`** — the promoted one plus any stale rows
behind it.

### Per-range, not per-block

Promotion happens **once per safe-frontier advance**, even when the range spans
several L1 blocks with several of our batches. This is sound, and loses nothing,
because of two facts:

- **Monotonic landing order.** L1 wallet nonces guarantee a higher nonce lands
  in a later-or-equal block, so the range's max nonce sits in its *latest*
  block-with-our-batch, and `promote_finalized`'s `delete <= max` supersedes
  every lower pending. Per-block promotion would compute the *same* `(nonce,
  block)` pairs and end at the same final one — it would just expose the
  intermediate ones transiently.
- **The intermediate checkpoints were never observable.** `finalized` is a
  single row the watchdog polls *asynchronously* — even with per-block
  promotion it can miss intermediates between polls. So "visits every block" was
  never a guarantee; per-range removes a cadence nicety, not a contract. In
  steady state a range is ~1 block anyway; the difference only appears during
  multi-block catch-up, where a finalized that jumps to the latest block is
  exactly what you want.

`BlockObservation` (`snapshot.rs`) is therefore a **constant-memory
accumulator**: one `Option<(nonce, block)>`, `observe()` infallible and
storage-free, `promotion()` returning the target. It does no I/O on the hot
loop.

### Atomic with the drain

The promotion is **folded into the same transaction that advances the drain**:
`maybe_advance_safe_frontier` calls `close_frame_only_promoting`, which sequences
the drained safe inputs, rotates the frame, *and* runs `promote_finalized_in` —
all in one `write`. A crash therefore leaves promote + delete-pending +
drain-sequence either all committed or all rolled back. This is the fix for the
wedge in §6; see there for why a *separate* promotion is dangerous.

The standalone `Storage::promote_finalized` is retained only for test setup
(it's the only way to *supersede* an existing finalized row, which
`insert_finalized_dump` — the genesis-only path — cannot).

## 6. Case study: the promote/drain wedge

The earlier design promoted **per block, each in its own transaction**, before a
separate `close_frame_only` advanced the drain. That window was a latent
fail-loud crash-loop. The mechanism, with every escape that *fails* to save it:

A crash after a promotion commits but before the drain advances leaves a
**promoted-but-undrained** batch. On restart the lane re-processes the same safe
input and re-promotes — on a pending row the first promotion already deleted.
Why nothing prevents that:

1. **`promote_finalized` hard-fails on a missing pending row** — `SELECT dump_id
   … WHERE nonce = ?` → `QueryReturnedNoRows`, no idempotency guard.
2. **`safe_accepted_batches` survives the crash** — the lane *read* it to decide
   to promote, so it was committed earlier (by the safe-head sync), independent
   of the drain.
3. **`accepted_batch_nonce_at` has no pending-row gate** — bare `SELECT nonce
   FROM safe_accepted_batches WHERE safe_input_index = ?`. It still returns the
   nonce after the pending row is gone.
4. **The drain cursor didn't move** — `next_undrained_safe_input_index` is
   `MAX(safe_input_index)+1` over *sequenced* rows, and sequencing is
   `close_frame_only`'s job, which never committed. So the range re-processes.
5. **Recovery doesn't reconcile it** — both recovery paths clear pending *only*
   `if !invalidated.is_empty()`, i.e. only on a real cascade. A plain crash
   invalidates nothing.

Result: restart → re-process → `QueryReturnedNoRows` → `InclusionLaneError`,
uncaught → lane exits → next restart hits the identical row → **crash-loop**, no
automatic way out (the promoted batches are L1-confirmed/gold, so even aging
won't trip a cascade to clean them). The window is *wide* during catch-up — it
spans from the first promotion until `close_frame_only` commits.

**The fix** (§5): fold the single per-range promotion into `close_frame_only`'s
transaction. The "committed promotion, uncommitted drain" state becomes
unrepresentable.

### Why "missing a promotion" is fine but the wedge is not

These are opposite crash outcomes, and only one is benign:

- **Lag** (a promotion that simply didn't happen) is fine: `finalized` stays at
  a valid *older* block-complete checkpoint; the lane's own catch-up loads from
  the **latest pending** (not finalized), so app state doesn't even lag; and
  re-processing re-promotes forward. Converges.
- **Stuck** (re-promoting a deleted row) is the wedge: it makes *no* forward
  progress.

The atomic fold converts every crash into the first kind: either fully
committed, or fully redone cleanly on restart. The invariant it establishes — **a
committed promotion implies an advanced drain past that batch** — is exactly what
the regression tests pin.

### Regression tests

- `promotion_advances_drain_atomically_so_restart_cannot_re_promote` — red
  against the per-block design (literal `QueryReturnedNoRows`), green after the
  fold (promotion + drain advance commit together).
- `close_frame_only_promoting_rolls_back_the_drain_when_promotion_fails` — the
  atomicity complement: a mid-tx promotion failure rolls the drain back too.

## 7. Garbage collection

A dump becomes collectable when `lease_count = 0` AND it is referenced by
neither `pending_snapshots` nor `finalized_snapshot`. Promotion is what *creates*
such garbage (the superseded finalized, lower-nonce pendings).

### When GC runs

**After a promoting safe-frontier advance, on the lane's own thread**
(`maybe_advance_safe_frontier`, right after `close_frame_only_promoting`
commits — `run_gc::<A>` when a promotion occurred). One full
`gc_unreferenced_dumps` pass per advance that promoted; it reclaims the
just-superseded finalized plus any earlier lease-released garbage.

Why this, and not the alternatives:

- **Not the idle path.** GC used to run on the lane's idle branch, gated to 60s
  *checked only when idle*. Under sustained load the lane never idles, so GC
  starved exactly when batches and their superseded dumps piled up. Tying GC to
  *promotion* couples it to garbage *creation*: promotion is ≤ batch-close
  frequency, so the cadence self-scales and can't be starved.
- **Not a dedicated worker.** Keeping GC on the lane preserves "every
  snapshot-table write happens on one thread." A separate GC worker would be a
  second writer contending for the SQLite write lock with the lane's
  promote/insert — buying decoupling we don't need at the cost of contention.
- **Not on promotion's "critical path" in any harmful sense.** Promotion is on
  the finalized-tracking (safe-frontier) path, not the soft-confirmation hot
  path, so a small extra tx there is invisible to user-facing latency.

`snapshot_gc_at_startup` remains the once-per-boot backstop. If GC ever becomes
expensive, a dedicated background task is still the upgrade path.

### Crash ordering: no SQLite row pointing at a missing file

The invariant is **no `dumps` row referencing a non-existent directory**. It
gives the create/delete orderings:

- **File create → SQLite insert** (file-first): `create_dump` `fsync`s before
  the reference row is written.
- **SQLite delete → file delete** (SQLite-first): `gc_unreferenced_dumps`
  deletes the rows inside one `write` tx and *returns* the prefixes; the lane's
  `run_gc` then `A::delete_dump`s them after commit. If a directory delete
  fails, an orphan file is acceptable — the next startup's `sweep_orphan_dumps`
  catches it. The reverse ordering would leave a dangling row.

This is why `storage/snapshot_dumps.rs` is SQLite-only and the FS half lives in
the lane: the boundary *is* the ordering guarantee.

### Single-transaction GC closes a lease race

`gc_unreferenced_dumps` does the eligibility query and the deletes in **one
Immediate-mode tx**. A naive read-then-delete split would race a concurrent
`acquire_*_lease` from an HTTP handler (§ leases below); doing both in one write
serializes against any concurrent writer.

### Leases (HTTP serving)

The streaming endpoints (`/finalized_state`, `/latest_snapshot`) must not have
their dump GC'd mid-response. The lease read and the row read are **one atomic
tx** (`acquire_finalized_lease` / `acquire_latest_snapshot_lease`), and the
handler holds the lease for the response lifetime via a **drop-guard** inside the
streaming body — so it releases on completion, error, *and* client disconnect.
Release is offloaded to `spawn_blocking` so the write-lock-contended release
never stalls an async worker. `reset_dump_leases` at startup is the crash
backstop. (Endpoint shapes: [`AGENTS.md`](../../AGENTS.md) and the root
[`README.md`](../../README.md).)

### Startup sequence

`Workers::spawn` runs five steps, order-critical: (1) `reset_dump_leases`
(clear stale leases from a crashed run), (2) `ensure_finalized_snapshot`
(genesis snapshot if cold), (3) `ensure_open_tip` (genesis Tip if cold — the
tip-existence invariant below; the lane loads the head itself after catch-up),
(4) `snapshot_gc_at_startup`, (5) `sweep_orphan_dumps` (remove on-disk dirs not
in `dumps`; runs *after* (2) so the genesis prefix is registered and not swept).

## 8. Recovery interaction

Danger-zone recovery (`storage/recovery.rs`, see
[`../recovery/README.md`](../recovery/README.md)) cascade-invalidates batches
that the canonical stream will never reach. When it does (`!invalidated
.is_empty()`), it clears `pending_snapshots` **in the same transaction** as the
cascade — those pendings represent states catch-up must never load. `finalized`
is untouched (its bytes are for an L1-confirmed batch, which survives any
cascade). A **no-op** recovery (closed batches gold, Tip fresh) deliberately
preserves in-flight pendings the lane is still working with. This conditional
clear is why catch-up can safely resume from a surviving pending (§4).

## 9. Where the code lives

| Concern                         | Location |
|---------------------------------|----------|
| Dump trait + wire format        | [`format.md`](format.md); `sequencer-core/src/application/`, `examples/app-core/` |
| Storage (SQLite only)           | `sequencer/src/storage/snapshot_dumps.rs`; atomic close + promote in `storage/ingress.rs` |
| Lane integration (take/observe/GC) | `sequencer/src/ingress/inclusion_lane/snapshot.rs`, `mod.rs`, `catch_up.rs` |
| Runtime startup sequence        | `sequencer/src/runtime/workers.rs` |
| HTTP serving + leases           | `sequencer/src/egress/api/snapshot.rs` |
| Recovery clear                  | `sequencer/src/storage/recovery.rs` |

## 10. Deferred / future work

- **Watchdog (separate project).** `/finalized_state` and
  `/finalized_state/inclusion_block` are consumed by an operator watchdog that
  advances its own canonical machine through L1 to the served `inclusion_block`
  and compares its `inspect_state` output to the served bytes. Its prerequisite
  is a real `inspect_state` on the canonical-machine app — the symmetric side of
  `create_dump` (see [`format.md`](format.md)) — currently a stub in
  `examples/canonical-app/`. The watchdog itself lives outside this repo.
- **Pending-pool upper bound.** The pending pool grows per batch-close until
  promotion; pathologically (L1 stops accepting batches) it grows unboundedly.
  Harmless for the toy wallet's tiny dumps; cap + reject + alert once a real
  app's state is hundreds of MB.
- **Directory-style dumps.** `create_dump` already supports multi-file prefixes,
  but the HTTP layer streams a single `state_file_in_dump`. Serving a directory
  means a tar-stream or a minimal archive format. (Lease design "X" already
  holds for the whole stream, so no rework there.)
- **`Range:` requests / compression** for `/finalized_state` — defer until
  measured; state bytes are low-entropy but range-resume of large dumps may
  matter.
- **Cross-implementation test vectors** — land when a second `Application` (the
  canonical machine's) exists to validate byte-for-byte against the wallet's
  format.
