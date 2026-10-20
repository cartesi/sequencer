# Cartesi Machine Facts We Rely On

The canonical application runs inside the Cartesi Machine (CM). The watchdog
drives it, the canonical guest is built for it, and cockroach recovery exports
from it. This page records the CM behavior those components depend on,
especially behavior that is easy to assume wrongly. Everything here holds for
the pinned version (`CARTESI_MACHINE_VERSION` in
[`toolchain-pins.env`](../toolchain-pins.env), currently v0.22.0) and was
checked against the emulator source at that tag. Re-check it on every bump.

## Versions are all-or-nothing

A stored machine records a config archive version (8) and the emulator's
machine identifiers (`marchid` 22); loading rejects any mismatch, and nothing
converts a stored machine to another version (`skip_version_check` skips only
the identifiers). A CM bump therefore invalidates every stored image, release
tarball, and watchdog checkpoint: rebuild the canonical images and the watchdog
test guest (the recipes reuse an image that exists), and start each new
deployment's watchdog from a new state directory
([upgrades](watchdog/operator-deployment.md#upgrades)). The kernel and guest
tools are the ones the emulator release's `dependencies.lock` names, and move
with it: kernels are
[`cartesi/linux`](https://github.com/cartesi/linux/releases) releases named by
kernel version, guest tools are `machine-guest-tools` releases;
`scripts/verify-toolchain-pins.sh --upstream` checks the pairing. The
development shell pins the same emulator through the parent flake.

**A deployed application is pinned to its emulator version.** Its on-chain
template hash is the root hash of a machine built with one emulator release,
and the machine identifiers are part of that hash. A machine rebuilt with
another release has a different root hash, and the old release's stored
machines do not load. Every canonical machine for a deployment, the
watchdog's included, must run the release its template was built with; moving
to a new emulator release means a new application deployment.

**What moves the template hash.** The hash covers every byte of the stored
machine, so it moves when any input to the image moves, even when behavior does
not:

- the emulator release, even with every other input fixed (the machine
  identifiers and the microarchitecture's state are part of the root hash, and
  execution fixes change what runs), and its kernel (`toolchain-pins.env`);
- the rootfs: the guest Dockerfile, `machine-guest-tools`, and `xgenext2fs`;
- whether the cross toolchain has `rust-src`: with it, rustc embeds local
  standard-library paths in panic locations instead of the remapped
  `/rustc/<hash>/` ones. cross 0.2.5 adds it only on a run after the one that
  installs the riscv64 target, so the guest recipes install both first
  (`cross-toolchain`) and a cold build is reproducible;
- the dapp binary: the `rust-toolchain.toml` channel, every `Cargo.lock` entry
  in `canonical-app`'s build graph, the cross image digest in
  `examples/canonical-app/Cross.toml`, and `CROSS_VERSION`. The build graph
  includes the proc macros and build-dependencies that only run on the host
  (`syn`, `bindgen` for libcmt, `ruint` for the fee table): a version bump
  anywhere in the graph can move the binary, most likely because each crate's
  metadata hash covers its dependencies and feeds symbol names and code
  layout. The host half of
  the graph is `x86_64-unknown-linux-gnu`, cross's container, whatever the
  machine; `cargo tree -p canonical-app --target all -e normal,build` lists a
  superset from any machine, and an update outside it leaves the binary
  byte-identical;
- any source change in a guest crate (`sequencer-core`, `sdk/guest/*`,
  `examples/app-core`, `examples/canonical-app`), even in code the guest never
  calls: recompiling a crate shifts its constants and code layout.

The checkout path does not enter the hash, and `just canonical-print-build-hashes`
prints all four inputs to compare; compare cold builds, since a warm `target/`
can hold crates compiled before `rust-src` arrived. A move that is meant to preserve behavior is
checked against [`tests/fixtures/consensus_vectors_v1.json`](../tests/fixtures/consensus_vectors_v1.json),
which pins the consensus bytes (EIP-712 hashes, signature recovery, SSZ, fee
table, output payloads) natively and, through `canonical-test`'s golden cases,
in the built image, where each signature variant is checked for running as the
signer or not. The fixture is never regenerated: a mismatch is a behavior
change to explain. A release that moves the hash says so, and deployed
applications keep their old image, which the watchdog checks against the
on-chain hash.

## Rollup host semantics

The reference host loop is the `cartesi-machine` CLI (`run_advance_state_epoch`
in `src/cartesi-machine.lua`); Dave's PRT client implements the same rules.
Any other host, including the watchdog, must match it exactly:

| Guest outcome after an input | Host action |
|---|---|
| `RX_ACCEPTED` manual yield | commit the input's state |
| `RX_REJECTED` manual yield | restore the pre-input snapshot |
| `TX_EXCEPTION`, halt, any other manual yield, or mcycle overflow | fixed point: the machine stays there forever, with no revert |

The outcome is read in this order: halt, then manual yield, then an exhausted
budget. A yield on the budget's last cycle is that yield, and the next input
renews the budget; overflow means neither flag is set.

A host classifies a pending yield as the step does: `iflags.Y`, then tohost's
device, command and reason, never its data field (sling's
`MachineStf::manual_yield_reason` at dave@dbcca7d4). A guest that bypasses
libcmt can declare data past the 2 MiB TX buffer, which `receive_cmio_request`
refuses; the CLI reads every yield through it and aborts there. `iflags.Y`
under another device or command, which only a preset template carries, is a
fixed point.

- The host reads the root hash, snapshots, then sends the input together with
  that root as the *revert root hash*. At an `RX_ACCEPTED` yield the emulator
  refuses an advance without a revert root hash or with one that differs from
  the current root, and at any state it refuses a revert root hash on any
  other response. Every other undeliverable response is a silent no-op that
  leaves the machine unchanged, like the logged and proven transition: no
  pending yield, an advance at any yield other than `RX_ACCEPTED` (an
  unreverted rejection included), or data larger than the 2 MiB RX buffer.
  Only delivery clears `iflags.Y`, so a host checks the yield before sending
  and `iflags.Y` after.
- Each delivered advance sets the budget `imcyclemax` to the current mcycle
  plus 2^48 − 1. Exhausting it with no halt or yield pending is a fixed point,
  not a reject. At realistic speeds this takes days, so a stuck guest shows up
  as a hung host long before the budget ends.
- An inspect query is legal only at an `RX_ACCEPTED` yield and runs on a
  snapshot the host discards. Inspect execution is never part of canonical
  history, and its single report is bounded by the 2 MiB CMIO TX buffer. The
  emulator itself delivers an inspect at any pending manual yield; the host
  enforces `RX_ACCEPTED` (the CLI asserts it).

A fixed point is permanent: once the canonical machine reaches one, no later
input is ever processed for that application.

## Storage, sharing, and snapshots

- **Loading** defaults to `SHARING_NONE`: every backing file is mapped
  privately and never modified, under a shared `flock`. `SHARING_ALL` maps the
  files shared and runs the machine in place on disk, under an exclusive
  `flock` on each writable file (read-only ranges keep a shared one), so one
  directory cannot be open both ways at once. `SHARING_ALL` refuses a writable
  range whose backing file has no write permission or more than one hard link.
- **`store`** writes every range in full as sparse files, refuses an existing
  directory, and does **not** fsync. Durable publication uses `sync_stored`
  (fsyncs files, the directory, and its parent), then `rename_stored` (atomic,
  no-replace, fsyncs both parents). `remove_stored` also syncs the parent.
- **`clone_stored`** reflinks writable files (`clonefile` on macOS APFS,
  `FICLONE` on Linux btrfs/XFS), hardlinks read-only ones, and falls back to a
  sparse copy when the filesystem cannot link (`ENOTSUP`, `EXDEV`, `EPERM`). It
  always works; it is cheap only on copy-on-write filesystems. It hard-links
  only files without write permission, so a clone still loads with
  `SHARING_ALL`; a copy hard-linked by hand (`cp -l`) does not.
- **Snapshots come in two families.** An in-place (`SHARING_ALL`) machine
  snapshots by closing, cloning its directory, and reopening; this is the CLI's
  `--revert-mode=stored` and Dave's sling node. The CLI's default
  `--revert-mode=fork` instead forks a JSON-RPC machine server process, which
  refuses to fork a machine with shared backing stores, its hash-tree files
  included (both processes would see the writes). We run the machine
  in-process and use the clone family only.

Measured with v0.21.0 on APFS (Apple M5 Max) with a machine holding a 1 GiB
NVRAM, 768 MiB of it non-zero:

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
  immediately in the machine state. It requires a kernel with UIO
  (`CONFIG_UIO`, `CONFIG_UIO_PDRV_GENIRQ`), which the pinned kernel has.
- **Flash drives** appear as `/dev/pmemN`. Guest writes go through the page
  cache, so the application must flush (`fsync`, `msync(MS_SYNC)`, or
  `O_DIRECT`) before finishing each input; otherwise the drive holds a stale
  mix of pages.
- **A comparison range must be raw.** The CLI formats a flash drive that has no
  data file (`mke2fs`) and mounts one that has; either puts filesystem metadata
  the native engine cannot reproduce into the range. Declare state drives with
  `mke2fs:false,mount:false`, or use an NVRAM.
- **Labels.** User labels are stored in the machine config (`flash_drive[i].label`,
  `nvram[i].label`). The guest's device tree aliases both the user label and the
  automatic name (`flashdriveN`, `nvramN`), but the config holds only the user
  label, and `cartesi.util.find_drive` matches user labels only.
- Read a range with `machine:read_memory(start, length)`. The stored file layout
  (`<start>-<length>.bin`) is an internal format; do not depend on it.

## The guest side

- **The guest–host interface is the same from v0.20 through v0.22.** Yield
  reasons, CMIO buffer addresses, and ioctl numbers match; only the names moved
  to `HTIF_*`. The revert root hash and the per-input cycle budget are
  host-side. libcmt 0.18 bundles its ioctl header, so the guest build needs no
  kernel headers.
- **Some guest tools are glibc builds.** The `nvram`/`flashdrive` label helpers,
  which the CLI's generated init script calls for every NVRAM and for flash
  drives it formats, mounts, or chowns, and `xhalt`, which `cartesi-init` calls
  to halt with the entrypoint's exit status, do not run on a musl (Alpine)
  rootfs. Without `xhalt`, a failing entrypoint halts with payload 0. The
  watchdog test guest ships musl stand-ins for both; the canonical wallet image
  does not, so its panics halt with exit code 0.
- **trolley has no exception call.** A guest that must raise a CMIO exception
  does it through `libcmt-sys` directly (see `watchdog/test-guest`).
- **The last input stays in the machine.** The CMIO receive buffer keeps it,
  and it is part of the machine state: root hashes agree only for byte-identical
  inputs, metadata included.

## Hashing

- `cartesi.sha256(data)` and `cartesi.keccak256(data)` hash a single Lua string
  in one shot, in place; neither the Lua module nor the C API exports a
  streaming hasher.
- `machine:read_memory(start, length)` fills a native buffer and copies it into
  a new Lua string, so reading a whole range holds it twice for a moment. It
  takes any start and length, so a range can be read in chunks.
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

- `--cmio-advance-state` reads input files `input_file_index_begin` to
  `input_file_index_end`−1 (the end defaults to the begin, so name both) and
  rejects unknown keys. It checks the outputs Merkle root, starting from the
  empty tree unless `last_output_proof` (or, with `resumable:true` and
  `begin_epoch:false`, an `outputs_frontier` file) seeds it; a run resumed
  from a mid-history checkpoint with neither needs
  `check_outputs_merkle_root:false`.
- `--max-mcycle` reached mid-input exits 0 and stores the mid-input state
  (without `resumable`).
- Exit codes do not distinguish outcomes (a halt with payload 0 exits 0); load
  the stored machine and read its state instead, cross-checked with
  `--final-hash`.
- `--assert-rolling-template` runs after `--store`: it requires an
  `RX_ACCEPTED` yield whose TX buffer holds the expected outputs Merkle root
  (by default the empty tree's, which libcmt writes on its first accept) and
  exits 2 otherwise, leaving the stored image in place.
- `--cmio-inspect-state` asserts an `RX_ACCEPTED` yield before the query and
  exits nonzero otherwise; the query's own accept or reject does not set the
  exit code.
- `--remote-spawn` leaves machine servers running unless `--remote-shutdown` is
  also given. `--no-revert` aborts the run at the first rejected input.
