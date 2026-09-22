# Watchdog v2

**Status:** active plan (2026-09-22). Items marked *proposed* await
confirmation; everything else is decided. When the work lands, move durable
content to its owners (named in [Documentation](#documentation)) and reduce this
file to any remaining work.

## Purpose

The watchdog independently replays L1 inputs in the canonical Cartesi Machine
and checks that the sequencer's accepted checkpoint holds the same application
state. v2 changes three things:

1. **Where the state comes from.** Requiring every application to answer an
   inspect `state` query is not a reasonable contract, and inspect cannot carry
   a state larger than one 2 MiB report. The only real client keeps its
   canonical state `M` in a machine memory range (flash drive or NVRAM).
2. **How inputs execute.** The Lua host loop re-implemented rollup semantics
   and got them wrong: no revert on reject, inspect run on the machine that is
   then stored, and halt/exception treated as transient. CM v0.21 refuses to
   feed an input after a reject without a revert, so the old loop cannot run on
   the pinned emulator at all.
3. **What happens when it fires.** Detection alone is not enough. A divergence
   needs a latch, preserved evidence, a documented playbook, tooling for each
   step, and drills practiced before an incident.

The [Cartesi Machine facts](../cartesi-machine.md) page records the emulator
behavior this design depends on.

## Decisions

### 1. A generic executable with two state sources

The watchdog stays one generic executable. `init` persists the state source:

- `inspect`: the query `state` must end at `RX_ACCEPTED` with exactly one
  report. The toy wallet keeps using it.
- `range:<label>`: the whole flash drive or NVRAM carrying user label `<label>`,
  all `length` bytes including the zero tail.

A library with application-supplied extraction hooks waits for a second client
whose needs these two sources cannot meet.

### 2. CM v0.21

v0.21 stores user labels in the machine config (so the state source is a label,
not an address), adds NVRAM, and enforces the revert root hash. The bump landed
first in this branch: pins, kernel `ctsi-2`, guest tools 0.18, rebuilt canonical
images, and the vendored `cartesi-tools/` crates (`libcmt-sys`, `trolley`,
`testsi`, `types`).

### 3. In-process execution with the reference snapshot semantics

The watchdog drives the machine in-process through the `cartesi` Lua module,
with no JSON-RPC server and no subprocess. It mirrors the CLI's
`--revert-mode=stored` loop:

```
working = clone_stored(head)                   -- once per tick
m = load(working, SHARING_ALL)                 -- runs in place on disk
for each input:
    revert_root = m:get_root_hash()
    close m; clone_stored(working, snapshot); reopen m
    send_cmio_response(ADVANCE_STATE, input, revert_root); run to a stop
    RX_ACCEPTED            -> remove snapshot
    RX_REJECTED            -> close m; replace working with snapshot; reopen;
                              assert root == revert_root
    any other stop         -> canonical fixed point: fatal divergence
```

The measured cost on APFS is ~50 ms per input with a 1 GiB NVRAM and a nearly
free per-tick clone. On filesystems without reflinks, every clone becomes a full
sparse copy; the [deployment guide](../watchdog/operator-deployment.md) will
require a copy-on-write filesystem (XFS with reflink, btrfs, APFS) for
multi-GiB states. The reference CLI serves as a test oracle: conformance tests
run the same inputs through both and compare root hashes.

### 4. Compare SHA-256 digests

The comparison is an equality check between two parties we operate, over an
internal channel, for states expected to reach several GiB. Transferring the
bytes on every accepted checkpoint is waste; a digest is the transport.

| | Flat SHA-256 (chosen) | Flat Keccak-256 | Machine Merkle root |
|---|---|---|---|
| Sequencer cost | ~0.3–0.5 s/GiB, streamed | ~1 s/GiB | reimplement the CM tree; ~8× flat keccak |
| Watchdog cost | `cartesi.sha256(read_memory(range))`, ~2× range in memory | same | ~free (`get_node_hash`) |
| Coupling | none; checkable with `sha256sum` | none | CM tree layout and hash function |
| Composes with | nothing on chain | nothing (no one hashes GiBs on chain) | machine root hash via `get_proof` |

The Merkle root is the right answer to a different question: checking the
sequencer's state against a machine commitment (for example a settled Dave
claim) without re-executing. Adopt it when such a verifier exists. If the
watchdog's 2× peak memory becomes a problem, a two-level digest (SHA-256 over
SHA-256 of fixed 64 MiB chunks) keeps memory bounded.

Sequencer API addition:

- `GET /finalized_state/digest` returns `{ inclusion_block,
  executed_input_count, sha256 }`. It hashes the accepted checkpoint's
  comparison file off the lane, while holding the same finalized lease as
  `GET /finalized_state`.
- `GET /finalized_state/inclusion_block` stays: it is the cheap database-only
  poll that decides idle ticks.
- `GET /finalized_state` stays: the watchdog downloads it only as divergence
  evidence.

### 5. The tick

1. If the divergence marker exists, exit 2 without work.
2. Load config; the head is the newest directory under `checkpoints/`. Remove a
   leftover `working/`.
3. Poll `inclusion_block`. If equal to the head, exit 0 idle. If lower, the tick
   is a divergence (`inclusion_block_regressed`).
4. `GET /finalized_state/digest`. Its `inclusion_block` B is the replay target,
   so the target cannot move under a long replay.
5. Stream InputBox inputs for `(head, B]` with completeness witnesses
   (Decision 8) and feed each one through the executor.
6. Close the working machine, `sync_stored` it, and open it read-only
   (`SHARING_NONE`): confirm it sits at `RX_ACCEPTED`, then extract the digest
   (read the range, or run the inspect query on this discarded private copy).
7. Match: `rename_stored` to `checkpoints/<B>`, then remove the previous head.
   Mismatch: latch (Decision 6).
8. Write `status.prom`, including a last-completed-tick timestamp.

There are no in-tick retries. A transient failure exits 1; the scheduler's next
tick is the retry.

### 6. Divergence latch and evidence

Exit 2 writes `divergence.json` atomically: kind, last agreed block P, target
block B, both digests, and the detection time. Kinds:

- `state_mismatch`: digests differ at B.
- `canonical_machine_dead`: the canonical machine reached a fixed point
  (exception, halt, unexpected yield, or cycle overflow), with the input index
  and the guest's message or exit code.
- `inclusion_block_regressed`: the accepted checkpoint went backwards.

Evidence goes under `evidence/<B>/`:

- the canonical machine at B, which is canonical state independent of the
  sequencer;
- the sequencer's comparison file, if the sequencer is still at B;
- for ranges, the first differing offset and the number of differing pages.

While the marker exists, every tick exits 2 without work. Clearing is always
safe: if the cause persists, the next tick latches again.

### 7. Incident tooling, runbook, and drills

*Proposed* subcommands:

- **`status`**: read-only JSON on stdout: head block, state source, latch
  contents, evidence paths, and last tick outcome. Safe during an incident.
- **`clear --block <B> --reason <text>`**: requires an existing marker for
  block B, so it cannot clear a different incident. Moves the marker and
  evidence to `incidents/<detected-at>-<kind>-<B>/` with the reason, and never
  deletes evidence.
- **`replay --from <checkpoint> --from-block <A> --to-block <B> --out <dir>`**:
  re-derives canonical state into a fresh directory with the same executor and
  L1 reader, and prints the digest. It never touches the state directory. It
  answers "is the canonical side reproducible?" and produces a canonical
  checkpoint at a chosen block for triage or cockroach export.

A new [incident runbook](../watchdog/incident-runbook.md) replaces
`staging-drills.md`. Playbook:

1. **Stop the sequencer.** The sequencer cannot take funds, but users act on
   soft confirmations; a real divergence means new confirmations may not hold.
   The cost of a false alarm is downtime. Deviate only with evidence of a
   watchdog-side fault.
2. **Triage by kind.**
   - **`state_mismatch`:** decide which side is wrong. Run `replay` from an
     earlier trusted checkpoint to B. A matching canonical digest means the
     canonical side is reproducible. Then use the evidence diff and the inputs
     in `(P, B]` to locate the divergence.
   - **`canonical_machine_dead`:** the application is dead on chain; no later
     input will ever be processed. Rule out a wrong image first. This is an
     application-level incident for its owners.
   - **`inclusion_block_regressed`:** a sequencer misconfiguration or restored
     data directory.
3. **Resolve.**
   - **Sequencer wrong:** fix it, then run cockroach recovery from the
     canonical checkpoint at B (or at P). Resume and `clear`. The watchdog
     keeps its head at P: the sequencer's recovery does not change canonical
     history.
   - **Watchdog wrong:** fix it, wipe and re-`init` its state directory, and
     restart the sequencer.
   - **Canonical dead:** the sequencer stays stopped and the marker stays.

Drills run in CI as e2e scenarios, each walking the runbook with the real
commands:

- **Mismatch:** latch, then `status`, `replay` (canonical reproduces), and
  `clear`. The watchdog re-latches while the fault remains, then ticks clean
  after the fix.
- **Canonical dead:** a test guest halts on a marker input.
- **Regression.**

### 8. Correctness and robustness fixes carried by the rewrite

- **L1 completeness.** Each input's InputBox index must be contiguous from the
  head's next index; the count at B is pinned with `getNumberOfInputs` at block
  B; the chain id in each payload must equal the pinned chain id; and the
  coverage check uses the `safe` tag.
- **Classification.**
  - Exit 2 for latched divergence.
  - Exit 1 for transient (RPC, HTTP, I/O) and operator (config, state, image)
    errors; operator errors are named as such in the log and in `status.prom`.
  - No string matching.
- **Heartbeat.** `cartesi_watchdog_last_tick_timestamp_seconds`, because
  textfile-collector samples carry the scrape time rather than the tick time, so
  a hung or blocked watchdog otherwise keeps showing its last `ok`.
- **Config.** `config.json` becomes version 2. `init` persists identity and
  state source; only the RPC and sequencer URLs are overridable at tick. Dead
  knobs go: the `INPUT_ADDED_TOPIC` override, the unverified
  `CM_IMAGE_HASH`, and the `config.load` alias. A missing range-error-code list
  falls back to the defaults.
- ***Proposed* genesis check.** When the bootstrap block is the application's
  deployment block, `init` checks the bootstrap machine's root hash against the
  application's on-chain template hash.
- **Deletions.**
  - `machine_runner.lua` and the Lua host loop.
  - Test-only seams in the runner.
  - Parsing of `executed_input_count`, ETag, and 304.
  - Dead `l1_reader` exports.
  - Hand-rolled JSON and `head.json`.
  - `retry.lua`.
  - The fake-only divergence drill script.
  - Test assets in the runtime image.
  - Legacy skips in the CM e2e.

## Documentation

| Content | Owner after landing |
|---|---|
| CM behavior and measurements | [`docs/cartesi-machine.md`](../cartesi-machine.md) (written) |
| Runtime contract, state sources, config table, metrics, module map | `docs/watchdog/README.md` |
| Incident playbook and drills | `docs/watchdog/incident-runbook.md` (new; replaces `staging-drills.md`) |
| Production checklist, CoW filesystem requirement, re-`init` on CM bumps | `docs/watchdog/operator-deployment.md` (links, not copies) |
| Comparison bytes for `range` sources: full length, raw, flush or NVRAM | [Application contract §6](../protocol/application-contract.md#6-checkpoint-lifecycle) and the C binding header |
| Canonical evidence checkpoint as a recovery source | [`docs/recovery/cockroach.md`](../recovery/cockroach.md) |
| `/finalized_state/digest` wire contract | [README API](../../README.md#api) |

The consolidation removes the duplicated metrics, exit-code, flock, and
quick-start text across the watchdog docs, fixes the stale recipe and binary
names, and deletes the `sepolia.md` redirect stub.

## Work order

1. CM v0.21 bump and `cartesi-tools/` import. Done in the working tree.
2. `docs/cartesi-machine.md`. Written.
3. Sequencer `GET /finalized_state/digest`, with an integration test.
4. Watchdog core: executor, state sources, checkpoints, tick, config v2,
   classification, L1 witnesses, heartbeat. Lua unit tests are rebuilt around
   one fake-dependency builder.
5. A CM test guest (trolley) that accepts, rejects, raises an exception, halts
   on marker inputs, and writes an NVRAM range. It is used for executor
   conformance against the CLI oracle and for range-source tests.
6. Latch, evidence, `status`/`clear`/`replay`, runbook, and CI drills.
7. Deletions and docs consolidation.
8. Full validation: workspace tests, `just test-watchdog`, CM e2e,
   `just test-rollups-e2e`, and the Docker smoke.

## Deferred

- Merkle-root digests, until a verifier against machine commitments exists.
- A long-lived watchdog process that keeps the machine open between ticks,
  until measured per-tick costs call for it.
- Automatic sequencer stop on divergence. The watchdog stays notify-only.
- The watchdog as a library with application extraction hooks.

## Open questions for the DEX integration

- Is `M` exactly the whole NVRAM or drive, zero tail included, with the same
  length as the native comparison file?
- Which CM version does the DEX image pin?
- Does the guest ever reject an input? The watchdog assumes it may.
- How large is `M` expected to grow? This bears on the 2× digest memory and on
  the CoW filesystem requirement.
