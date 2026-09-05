# Dump / `Application` API — Design Draft (Track 6)

**Status: draft for review** (us, Bart). Companion to the Track 3 feed
design — review together; libdex's on-disk layout depends on both. On
acceptance, the trait contract graduates into
`docs/protocol/application-contract.md` and the lifecycle changes into
`docs/snapshots/lifecycle.md`.

## 1. Motivation

Three forces, one API:

- **Cost.** `create_dump(&self)` is a full O(state) serialize + 3-fsync
  ladder, run synchronously on the single lane thread at every batch close —
  soft-confirmation acks stall for the duration. Tolerable for the toy
  wallet; not for a multi-GB flat buffer.
- **Bart's app shape.** libdex runs natively against an mmap'd flat state
  buffer — the Cartesi Machine's own storage approach (Bart implemented that
  feature), whose ecosystem exploits CoW (`clone_stored`: reflink, plus
  hardlinks — safe there only because stored images are immutable, see §10 —
  with plain-copy fallback; ~2.6 ms vs ~350 ms full store at 533 MB in
  Dave's measurements).
- **Verb review.** `create_dump / from_dump / delete_dump /
  state_file_in_dump` is a leaky projection of what the lane needs; the
  2026-07 investigation mapped it against the CM/Dave verb set.

Key investigation results this design builds on: the CM emulator has **no
commit/revert** — those are node-level orchestration over `clone_stored`,
and the sequencer already has both (DB row as commit point; older-dump +
replay as revert) correctly *off* the trait. The one missing primitive is
**cheap clone**. And the CM has since moved our way on durability:
machine-emulator PR #398 adds `rename_stored` and makes it and
`remove_stored` **durable (auto-synced)** — the commit-point idiom our dump
lifecycle hand-rolls.

## 2. Requirements

From the lane trace (R) and the wishlist (W):

- **R1** Checkpoint the current state at batch close — atomic,
  crash-durable *before* the DB row lands (invariant I13).
- **R2** Reconstruct at startup: latest checkpoint + replay.
- **R3** Dispose checkpoints (GC + orphan sweep).
- **R4** Serve canonical bytes over HTTP without instantiating the app;
  bytes must equal the canonical machine's `inspect_state` output (the
  watchdog byte-compares).
- **R5** Genesis construction (off-trait today, stays off-trait).
- **W1** Checkpoints cheap enough to not shape batch policy (CoW).
- **W2** Checkpointing off the ack path.
- **W3** Natural fit for an mmap'd-working-image app without penalizing
  pure-RAM apps (WalletApp).

## 3. The fork, decided: working-image model

The investigation flagged one load-bearing fork: cheap clones require the
app to run against an on-disk image the lane can flush-then-clone (Dave's
`SHARING_ALL` model); `create_dump(&self)` serializing live RAM can never be
cheap. **This design takes the working-image model** — it is libdex's
natural shape, it is what the CM ecosystem optimizes, and WalletApp adapts
trivially (its "working image" is a file it rewrites on flush; cost
unchanged from today's serialize).

## 4. Proposed trait

```rust
pub trait Application: Send + Sized {
    // --- execution surface unchanged ---

    /// Open the app on a working image directory. The sequencer owns the
    /// directory's lifecycle; the app owns its contents. Called at startup
    /// (from a cloned checkpoint) and after genesis materialization.
    fn open(working: &Path) -> Result<Self, AppError>;

    /// Make the working image consistent and durable on disk: flush
    /// app-level caches, msync mapped pages, fsync files. After `Ok`, the
    /// on-disk image alone reconstructs this exact logical state via
    /// `open`. Called by the lane at batch close, before cloning.
    fn flush(&mut self) -> Result<(), AppError>;

    /// Clone the (flushed, not currently open) image at `from` into `to`
    /// (must not exist). Default: recursive plain copy + fsync ladder.
    /// CoW apps override with reflink/hardlink (FICLONE / clonefile),
    /// keeping the same durability contract: on `Ok`, `to` survives an
    /// immediate kernel crash.
    fn clone_image(from: &Path, to: &Path) -> Result<(), AppError> { … }

    /// Delete an image directory the sequencer no longer references.
    /// Default: remove_dir_all.
    fn delete_image(prefix: &Path) -> Result<(), AppError> { … }

    /// Locate the single canonical state file inside an image without
    /// opening the app. Contract unchanged from state_file_in_dump:
    /// the file's bytes equal canonical `inspect_state` output (R4).
    fn canonical_file_in_image(prefix: &Path) -> PathBuf;
}
```

Verb mapping: `open` ≈ CM `load(SHARING_ALL)`; `flush` ≈ the msync the CM's
dirty-page sidecars make optional; `clone_image` ≈ `cm_clone_stored`;
`delete_image` ≈ `remove_stored`. Commit stays sequencer-side (DB row;
sealing by durable rename per PR #398's precedent). Revert stays
sequencer-side (older checkpoint + replay). `from_dump`/`create_dump`
disappear: restore is `clone_image(checkpoint, working)` + `open(working)`;
checkpoint is `flush()` + `clone_image(working, checkpoint)`.

## 5. Lane lifecycle changes

Batch close becomes: `flush()` → `clone_image(working, staging)` →
sequencer writes `info.toml` + durable-renames staging into the dumps dir →
DB row in one tx (unchanged commit point). Filesystem-first ordering, the
"orphan dir possible, dangling row never" invariant, promotion, GC, leases,
and the whole `snapshot_dumps.rs` layer are **unchanged** — the storage half
is already representation-agnostic (opaque prefix keys).

W2 (off-ack-path) falls out for CoW apps: `flush` + reflink is
milliseconds, and the expensive part (page write-back) is the kernel's
business afterward. A dedicated async stage is *not* designed in; if a
non-CoW app's flush is slow, that is the app's cost to fix by adopting CoW.

Startup (R2): clone the promoted/pending checkpoint into a fresh working
dir, `open`, replay. The working dir is disposable state — never promoted,
never served, deleted on clean start.

## 6. Crash-safety posture (unchanged, now sharper)

I13 stays: nothing may reach the DB before the corresponding image is
durable. The split is now explicit: the **app** guarantees durability of
image *contents* (`flush`, `clone_image`); the **sequencer** guarantees
durability of *directory structure* (rename + dir-fsync — exactly what CM
PR #398 now bakes into `rename_stored`/`remove_stored`, validating the
posture). Tell Bart directly: the CM's historical no-fsync stance does not
apply here — a CoW `clone_image` override must fsync what reflink leaves
unsynced, and #398 shows the CM itself now agrees for the rename/remove
verbs.

## 7. Serving (R4) and the libdex layout constraint

`canonical_file_in_image` keeps the single-canonical-file contract — the
HTTP snapshot routes, lease protocol, and watchdog byte-compare all survive
untouched. The constraint to put in front of Bart *before* libdex's layout
freezes: the served file must byte-match canonical `inspect_state` output,
so either (a) the flat buffer's layout is itself canonical — fully
normalized, no allocator padding, no free-lists, no pointer-valued fields,
no uninitialized gaps — and the buffer file doubles as the canonical file;
or (b) libdex writes a separate canonical projection during `flush`, which
reintroduces O(state) serialize cost and partly defeats CoW. (a) is the
performant answer and a real design constraint on his buffer format.

## 8. Migration & cleanups folded in

- **WalletApp:** `open` mmap-or-reads its file; `flush` = today's
  serialize+fsync ladder; defaults cover the rest. No capability lost.
- **Genesis:** concrete-type constructor materializes the initial working
  image, then the normal flush/clone path checkpoints it (R5 unchanged).
- **`SafeInputRecord` shim** (`storage/l1_inputs.rs`): collapse
  `StoredSafeInput`/`IngestedSafeInput` into one honest row model in a dedicated
  cleanup. The provenance/clock decision below is settled; it no longer blocks
  that cleanup.
- **Direct-input clock semantics (settled with Track 3):** `block_timestamp`
  is persisted and served as feed provenance only; it is not an application
  transition input. Directs execute at their exact L1 inclusion block and user
  ops at their frame's safe block, as owned by the
  [`Application` contract](../protocol/application-contract.md#3-the-safe-block-clock--last_executed_safe_block).
  No timestamp-bearing trait change is needed, and this no longer blocks
  libdex's state design.

## 9. Open questions

1. **Working-image locking:** the CM uses flock to make `SHARING_ALL`
   exclusive. Do we require the app to hold an equivalent lock, or does the
   sequencer's single-lane discipline suffice? (Lean: sequencer discipline
   suffices; a lock is cheap defense — app's choice.)
2. **`flush` durability scope:** must `flush` fsync, or is fsync deferred to
   `clone_image`? (Lean: `clone_image` owns durability of the *clone*;
   `flush` owns consistency of the *source* — msync yes, fsync optional.)
3. **Non-reflink filesystems:** plain-copy fallback makes checkpoint cost
   O(state) again silently. Log loudly at startup when the dumps dir does
   not support reflink? (Lean: yes — one probe at bootstrap.)
4. **Dirty-page tracking for hashing/inspect:** the CM's `.dpt` sidecars
   make post-clone hashing touch only dirty pages. libdex would need its
   own write-barrier to replicate; out of scope for the sequencer API but
   worth flagging to Bart as a cost driver for (a) in §7.

## 10. Review remarks (non-normative, open design)

These remarks record issues raised during review; they do not replace the
proposed trait or lifecycle above. (An earlier revision promoted the first
remark into §4's `clone_image` contract and misattributed that promotion to
a maintainer instruction; the 2026-08-01 review corrected the record and the
remarks-only posture is restored. Whether hardlinks stay in §4's suggestion
is settled in design review with Bart, alongside the rest of the trait.)

- Hardlinks are not a valid implementation of the mutable-working-image to
  immutable-checkpoint clone. Later in-place or mmap writes through either
  path mutate the same inode and therefore the checkpoint. Any accepted
  implementation should require a real CoW clone (`FICLONE`/`clonefile`) or
  an independent copy, with a test that mutating the reopened working image
  cannot change checkpoint bytes.
- Synchronous durability is likely simple and fast enough, and should remain
  the baseline while it is measured. Measure the relevant phases separately:
  app flush/msync, source synchronization if required, clone or copy,
  destination data and metadata synchronization, staging rename, parent
  directory synchronization, and DB commit. Measurements should cover
  representative state sizes and dirty-page ratios and report tail latency,
  not only a best-case reflink time.
- The contract that `clone_image -> Ok` survives an immediate crash is
  stronger than the statement in §5 that expensive page writeback may remain
  the kernel's work afterward. Until filesystem-specific ordering and sync
  requirements demonstrate both claims together, deferred writeback is an
  unresolved durability issue rather than work safely removed from the
  checkpoint path.
- An asynchronous checkpoint stage need not be designed preemptively. If the
  synchronous phase measurements meet the latency budget, keeping the
  durability boundary synchronous is preferable. Async staging should be
  reconsidered only if measurements show that the required flush and sync
  operations materially violate that budget.
