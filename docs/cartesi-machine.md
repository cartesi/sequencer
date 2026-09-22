# Cartesi Machine Facts We Rely On

The canonical application runs inside the Cartesi Machine (CM). The watchdog
drives it, the canonical guest is built for it, and cockroach recovery exports
from it. This page records the CM behavior those components depend on,
especially behavior that is easy to assume wrongly. Everything here holds for
the pinned version (`CARTESI_MACHINE_VERSION` in
[`toolchain-pins.env`](../toolchain-pins.env), currently v0.21.0) and was
checked against the emulator source at that tag. Re-check it on every bump.

## Versions are all-or-nothing

A stored machine records a config archive version and the emulator's machine
identifiers; loading rejects any mismatch. A CM bump therefore invalidates every
stored image, release tarball, and watchdog checkpoint: rebuild the canonical
images and re-`init` watchdog state directories. The kernel
(`machine-linux-image`) and guest tools (`machine-guest-tools`) are paired with
the emulator release and move with it. The development shell pins the same
emulator through the parent flake.

## Rollup host semantics

The reference host loop is the `cartesi-machine` CLI (`run_advance_state_epoch`
in `src/cartesi-machine.lua`); Dave's PRT client implements the same rules.
Any other host, including the watchdog, must match it exactly:

| Guest outcome after an input | Host action |
|---|---|
| `RX_ACCEPTED` manual yield | commit the input's state |
| `RX_REJECTED` manual yield | restore the pre-input snapshot |
| `TX_EXCEPTION`, halt, any other manual yield, or mcycle overflow | fixed point: the machine stays there forever, with no revert |

- The host reads the root hash, snapshots, then sends the input together with
  that root as the *revert root hash*. The emulator refuses an advance unless
  the machine sits at an `RX_ACCEPTED` yield and the revert root hash equals the
  current root, so a host cannot silently continue from a rejected state.
- Each input gets an mcycle budget of 2^48 (`imcyclemax`). Exhausting it is a
  fixed point, not a reject. At realistic speeds this takes days, so a stuck
  guest shows up as a hung host long before the budget ends.
- An inspect query is legal only at an `RX_ACCEPTED` yield and runs on a
  snapshot the host discards. Inspect execution is never part of canonical
  history, and its single report is bounded by the 2 MiB CMIO TX buffer.

A fixed point is permanent: once the canonical machine reaches one, no later
input is ever processed for that application.

## Storage, sharing, and snapshots

- **Loading** defaults to `SHARING_NONE`: every backing file is mapped
  privately and never modified, under a shared `flock`. `SHARING_ALL` maps the
  files shared and runs the machine in place on disk, under an exclusive
  `flock`, so one directory cannot be open both ways at once.
- **`store`** writes every range in full as sparse files, refuses an existing
  directory, and does **not** fsync. Durable publication uses `sync_stored`
  (fsyncs files, the directory, and its parent), then `rename_stored` (atomic,
  no-replace, fsyncs both parents). `remove_stored` also syncs the parent.
- **`clone_stored`** reflinks writable files (`clonefile` on macOS APFS,
  `FICLONE` on Linux btrfs/XFS), hardlinks read-only ones, and falls back to a
  sparse copy when the filesystem cannot link (`ENOTSUP`, `EXDEV`, `EPERM`). It
  always works; it is cheap only on copy-on-write filesystems.
- **Snapshots come in two families.** An in-place (`SHARING_ALL`) machine
  snapshots by closing, cloning its directory, and reopening; this is the CLI's
  `--revert-mode=stored` and Dave's sling node. The CLI's default
  `--revert-mode=fork` instead forks a JSON-RPC machine server process, which
  refuses to fork a machine with shared ranges (both processes would see the
  writes). We run the machine in-process and use the clone family only.

Measured on APFS (Apple M5 Max) with a machine holding a 1 GiB NVRAM, 768 MiB
of it non-zero:

| Operation | Cost |
|---|---|
| Write 768 MiB in place and compute the root hash | 0.68 s |
| Clone the whole stored machine | ~0.1 s, 64 KiB of new disk |
| Per input: root hash, close, clone, reopen, dirty 16 pages, commit or revert | ~50 ms; 100 inputs grew disk by ~40 MB |

Every revert in that run restored exactly the recorded pre-input root hash. On
a filesystem without reflinks, each clone is a full sparse copy of the
machine.

## Memory ranges for application state

An application can keep canonical state in a labeled memory range instead of
answering an inspect query:

- **NVRAM** (`--nvram=label:<l>,...`) appears in the guest as `/dev/uioN`. It
  has no filesystem layer and no page cache: guest writes through `mmap` are
  immediately in the machine state. It requires the paired kernel with UIO
  (ctsi-2 and later).
- **Flash drives** appear as `/dev/pmemN`. Guest writes go through the page
  cache, so the application must flush (`fsync`, `msync(MS_SYNC)`, or
  `O_DIRECT`) before finishing each input; otherwise the drive holds a stale
  mix of pages.
- **A comparison range must be raw.** The CLI formats a flash drive that has no
  data file (`mke2fs`) and mounts one that has; either puts filesystem metadata
  the native engine cannot reproduce into the range. Declare state drives with
  `mke2fs:false,mount:false`, or use an NVRAM.
- **Labels.** User labels are stored in the machine config (`flash_drive[i].label`,
  `nvram[i].label`). The automatic names `flashdriveN` and `nvramN` exist only as
  device-tree aliases; `cartesi.util.find_drive` matches user labels only. The
  CLI's NVRAM and drive init lines call the guest-tools `nvram`/`flashdrive`
  helpers, which are glibc binaries and do not run on a musl (Alpine) rootfs.
- Read a range with `machine:read_memory(start, length)`. The stored file layout
  (`<start>-<length>.bin`) is an internal format; do not depend on it.

## Hashing

- `cartesi.sha256(data)` and `cartesi.keccak256(data)` hash a single Lua string
  in one shot; there is no streaming hasher. Hashing a range in-process needs
  the whole range as one string (about twice the range in peak memory).
- The machine's own hash tree uses keccak256 by default (sha256 is
  configurable), with 32-byte leaves and 4 KiB pages. `get_node_hash(start,
  ceil_log2(length))` returns a range's Merkle root at the cost of rehashing
  dirty pages only, and `get_proof` ties it to the machine root hash. Any party
  comparing against that root must reproduce the same tree.

## The CLI as a test oracle

The CLI is a script, not a module: it parses the global `arg` and ends in
`os.exit`, so it cannot be `require`d. Its building blocks can (`cartesi`,
`cartesi.util`, `cartesi.hash-tree`, `cartesi.jsonrpc`). When tests run the CLI
as a reference:

- `--cmio-advance-state` checks the outputs Merkle root from genesis by default;
  resuming from a mid-history checkpoint needs `check_outputs_merkle_root:false`.
- `--max-mcycle` reached mid-input exits 0 and stores the mid-input state.
- Exit codes do not distinguish outcomes (a halt with payload 0 exits 0); load
  the stored machine and read its state instead, cross-checked with
  `--final-hash`.
- An inspect query is delivered even to a machine at an exception yield, and
  its own outcome is not reported.
- `--remote-spawn` leaves machine servers running unless `--remote-shutdown` is
  also given. `--no-rollback` no longer exists; use `--no-revert`.
