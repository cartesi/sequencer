# Watchdog

The watchdog independently re-derives the application's canonical state from
L1 and checks that the sequencer's accepted checkpoint holds the same state. It
runs the canonical Cartesi Machine over the InputBox inputs, extracts the
application state, and compares its SHA-256 with the digest the sequencer
serves for the same L1 block. Its value is independence: the sequencer serves
state from its own native execution, while the watchdog replays the canonical
program with the rollup's own host semantics.

The sequencer's "finalized" routes serve its latest **safe, accepted batch
checkpoint**, not Ethereum's `finalized` tag.
[Snapshot lifecycle](../snapshots/lifecycle.md#acceptance-and-comparison) owns
checkpoint selection and why that state is comparable at a whole L1 block.

| Document | Audience |
|---|---|
| This file | How the watchdog works: contract, commands, configuration, state, metrics |
| [`incident-runbook.md`](incident-runbook.md) | What to do when it latches a divergence, and the drills that practice it |
| [`operator-deployment.md`](operator-deployment.md) | Deploying on a live chain (Sepolia, mainnet) |
| [`getting-started.md`](getting-started.md) | Running it locally against a devnet |
| [Cartesi Machine facts](../cartesi-machine.md) | Emulator behavior the watchdog depends on |

## The tick

`tick` runs one compare cycle and exits; a scheduler (systemd timer,
Kubernetes CronJob) runs it periodically. There is no daemon and no in-tick
retry: a failed tick's retry is the next scheduled one.

1. If a divergence is latched, exit 2 without work.
2. The head is the newest checkpoint under `checkpoints/`.
3. Poll `GET /finalized_state/inclusion_block`. Unchanged: exit 0 (idle).
   Lower than the head: latch `inclusion_block_regressed`, unless the head is
   still the bootstrap machine. A regression is relative to a block the
   sequencer agreed with; a sequencer behind a never-agreed bootstrap block has
   simply not reached it, and the tick idles.
4. `GET /finalized_state/digest`. Its block B is the replay target, fixed
   before any replay, so a long catch-up never chases a moving target.
5. Check that the L1 RPC serves the configured chain and that its `safe` head
   has reached B.
6. Replay the InputBox inputs of blocks `(head, B]` on a clone of the head
   (below), proving the L1 view complete (below).
7. Extract the application state from the resulting machine and hash it.
8. Equal digests: publish the machine as `checkpoints/<B>-<input count>` and
   remove the previous head. Different: latch `state_mismatch`. A canonical
   machine that stopped for good during replay latches
   `canonical_machine_dead`. A divergence is latched and reported before its
   evidence is collected ([below](#divergence-latch)).

### Canonical execution

The watchdog drives the machine in-process through the `cartesi` Lua module,
with the reference rollup host semantics the `cartesi-machine` CLI and Dave
implement ([details](../cartesi-machine.md#rollup-host-semantics)). It clones
the head into a working directory and runs the machine in place on it; before
each input it clones the working directory as a snapshot:

- `RX_ACCEPTED`: the snapshot is discarded.
- `RX_REJECTED`: the snapshot replaces the working directory, and its root
  hash must equal the input's revert root hash.
- exception, halt, any other manual yield, or cycle overflow: a permanent
  fixed point. Nothing after it can ever run; the watchdog latches.

On copy-on-write filesystems (APFS, btrfs, XFS with reflink) a clone costs
metadata only; elsewhere it is a full sparse copy of the machine, which makes
multi-GiB states slow. Conformance tests run the same inputs through the
watchdog and the reference CLI and require equal root hashes.

### L1 completeness

A provider that silently drops logs must fail the tick, not produce a false
comparison. The reader requires InputBox indices to run contiguously from the
head's input count, and checks the count it reached against
`InputBox.getNumberOfInputs(app)` pinned at block B. Every input's payload must
name the configured application and chain. Long log ranges are split on the
provider error codes that mean "range too large", exactly like the Rust reader
(shared vector: `tests/fixtures/l1_partition_vector.json`).

**Scan floor.** The Rust reader starts at the application's deployment block,
which is sound only because it also witnesses that the InputBox rejects inputs
for undeployed applications. The watchdog starts at its bootstrap block and
proves completeness from the InputBox count instead; do not copy the
deployment-block floor without the witness.

The reader holds one provider response at a time: each successful log range
is decoded and fed to the machine before the next is fetched.

## State sources

`init` persists how the application state is extracted
(`CARTESI_WATCHDOG_STATE_SOURCE`):

- **`range:<label>`**: the whole flash drive or NVRAM whose user label is
  `<label>`, all of its bytes including the zero tail, read without running the
  guest. The sequencer's comparison file must be exactly those bytes. The
  [application contract](../protocol/application-contract.md#6-checkpoint-lifecycle)
  owns that obligation, including the flush rule for flash drives.
- **`inspect`**: the inspect query `state`, which must end at `RX_ACCEPTED`
  with exactly one report. It runs on a private copy of the stored machine,
  which is then discarded. One report is at most 2 MiB (the CMIO buffer), so
  this source only suits small states. The toy wallet uses it.

## Commands

| Command | Effect | Exit |
|---|---|---|
| `init` | Store the trusted bootstrap machine as the first checkpoint and write `config.json`. Idempotent on a complete state directory; refuses one initialized for another deployment or state source. | 0, or 1 |
| `tick` | One compare cycle; writes `last_tick.json` and `status.prom`. | 0 ok or idle, 1 warning, 2 divergence |
| `status` | JSON on stdout: config, head, latched divergence, last tick. Read-only; fails on a missing state directory. | 0, or 1 |
| `clear --block B --reason TEXT` | Archive the divergence latched at block B into `incidents/`. Refuses any other block, except for an unreadable marker, which has none. | 0, or 1 |
| `replay --from DIR --from-block A --to-block B --out DIR` | Re-derive canonical state from a stored machine into a new directory and print its digest. The machine must be a trusted start, like init's bootstrap; `--out` must not exist, and both paths are absolute. Reads the state directory's `config.json` and never writes it. | 0, or 1 |

The `sequencer-watchdog` wrapper takes a non-blocking `flock` on
`$CARTESI_WATCHDOG_STATE_DIR/run.lock` for `init`, `tick`, and `clear`;
schedulers must also prevent overlapping ticks (systemd, or Kubernetes
`concurrencyPolicy: Forbid`).

`init` checks the bootstrap machine: it must wait for an input and yield the
configured state source, and it must be a trusted start, which `replay`
requires of its `--from` machine too. Its block must be safe on the RPC; its
input count comes from `getNumberOfInputs` at that block, which needs an RPC
with state there (an archive node for old blocks); and when no input precedes
the block, the machine must be the application's template (its root hash
equals the on-chain `getTemplateHash()`).

Exit 1 covers three failure classes, which the log line and `last_tick.json`
name: `transient` (the L1 RPC or the sequencer's HTTP API; the next tick may
succeed), `operator` (configuration, the state directory, an image), and
`internal` (anything else, including filesystem failures such as a full disk).
Exit 2 means a divergence: found by this tick, or latched earlier. It stays
latched until `clear`. A tick that finds a divergence exits 2 even when it
cannot create `incident/` (a full state directory); it logs the divergence as
`NOT latched`, writes `last_tick.json` and a state-directory `status.prom` only
if the directory still accepts them, and the next tick finds the divergence
again if it persists.

## Configuration

`init` reads the environment and writes `config.json`, which later commands
read: the chain id, the application, the InputBox it derives from the
application, the state source, the bootstrap block, the sequencer URL, and the
range-error codes. The bootstrap block matters after `init` too: it tells a
never-agreed head from an agreed one (see [the tick](#the-tick)). The L1 RPC
endpoint is never persisted.

| Variable | Read by | Meaning |
|---|---|---|
| `CARTESI_WATCHDOG_STATE_DIR` | all | Absolute path of the state directory |
| `CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT` | init, tick, replay | L1 JSON-RPC endpoint; never persisted, so it can rotate |
| `CARTESI_WATCHDOG_SEQUENCER_URL` | init (persisted); tick (optional override) | Sequencer operator API base URL |
| `CARTESI_WATCHDOG_BLOCKCHAIN_ID` | init (optional) | Expected chain id; defaults to the RPC's `eth_chainId` |
| `CARTESI_WATCHDOG_APP_ADDRESS` | init | Application address; init derives the InputBox from its `getDataAvailability()`, as the sequencer does |
| `CARTESI_WATCHDOG_STATE_SOURCE` | init | `inspect`, or `range:<label>` with a label matching `[a-z][a-z0-9-]*` |
| `CARTESI_WATCHDOG_CM_SNAPSHOT_DIR` | init | Absolute path of the trusted bootstrap machine |
| `CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK` | init | L1 block the bootstrap machine has consumed all inputs through |
| `CARTESI_WATCHDOG_LONG_BLOCK_RANGE_ERROR_CODES` | init (optional) | Comma-separated provider codes that split log ranges; default `-32005,-32012,-32600,-32602,-32616` |
| `CARTESI_WATCHDOG_METRICS_FILE` | tick (optional) | Absolute path to write `status.prom` to instead of the state directory |
| `CARTESI_WATCHDOG_LUA_ROOT`, `CARTESI_WATCHDOG_LUA_BIN` | wrapper | Lua sources and interpreter (defaults suit the release image) |
| `CARTESI_WATCHDOG_LUA_DEPS` | all | Directory of the native modules (`lcurl.so`, `lfs.so`) |
| `CARTESI_WATCHDOG_PRINT_RELEASE_INFO` | wrapper | `1` prints the release's `RELEASE.json`, including its Cartesi Machine version, and exits |

Changing a persisted setting means wiping the state directory and running
`init` again.

## State directory

```text
state/
  config.json                      deployment identity and state source (init)
  checkpoints/<block>-<count>/     stored machine the sequencer agreed with; the newest is the head
  work/                            scratch for the running command
  incident/                        the latched divergence: divergence.json, evidence.json, evidence files
  incidents/<id>/                  cleared incidents, with the operator's reason
  last_tick.json, status.prom      the last tick's outcome
  run.lock                         the wrapper's lock
```

A checkpoint's name carries its L1 block and the InputBox input count through
that block. Checkpoints are published with the emulator's `sync_stored` and
`rename_stored`, which fsync and rename atomically, so the head needs no
pointer file and a crash leaves either the old head or the new one. Older
checkpoints are removed after the new head is published; an interrupted
removal is finished by the next agreeing tick. JSON files are atomic but not
fsynced. An `incident/` whose `divergence.json` is missing or torn still
latches (kind `unreadable_marker`); a lost `incident/` latches again on the
next tick if the divergence persists; a lost tick record is rewritten. Of the
evidence, only the canonical machine is fsynced: after a host crash, check
`canonical.bin` against `canonical_sha256`, and `sequencer.bin` against
`sequencer_sha256` (or `canonical_sha256` when the comparison was
`identical`). A `config.json` lost to a crash right after `init` means
running `init` again; a torn one (`config.json is not valid JSON`) means wiping
the state directory first. The watchdog stores whole machines, never the
sequencer's restore archives.

## Divergence latch

A divergence is latched first, and alone: the tick creates `incident/` and
writes the latch record `divergence.json` (kind, target block B, the agreed
head P, and the digests or the stop). The directory is the latch, so a record
lost to a crash or a full disk still latches, as `unreadable_marker`. The tick
then records the divergence (the record as one `watchdog_event <JSON>` line on
stderr, `last_tick.json` with exit code 2, and `status.prom`), and only then
collects evidence; the process exits 2 once collection ends, so alert on the
metrics, not on the exit status. While latched, a tick exits 2 even when its
configuration or endpoints are broken.

Evidence is collected in this order, each item on its own:

| Item | Kept | Kinds |
|---|---|---|
| `sequencer_bytes` | `sequencer.bin`: the sequencer's comparison file, if it still serves B | `state_mismatch` |
| `comparison` | Against the canonical bytes, when `sequencer_bytes` was kept: the 0-based offset of the first differing byte (`cmp -l` prints it plus one) and the number of differing 4 KiB pages; `identical` means the digests, not the states, disagreed | `state_mismatch` |
| `canonical_machine` | `canonical/`: the canonical machine at B, waiting for an input, or at its fixed point | `state_mismatch`, `canonical_machine_dead` |
| `canonical_bytes` | `canonical.bin`: the canonical comparison bytes | `state_mismatch` |

`evidence.json` records each item as it ends: its path or result, or
`<item>_missing` with the failure class and reason (`transient: …` when the
sequencer is unreachable or has moved on, `internal: …` for a full disk). Its
`collection` is `running`, then `finished`; a later tick that finds it still
`running` marks it `interrupted`, and a torn index reads as `unreadable`.
Files under their final names are complete, because each is renamed into
place (until a host crash; see [State directory](#state-directory)); `*.tmp`
files are partial. `status` shows the index as `divergence.evidence`. A
regression has no evidence.

The collecting tick holds the wrapper's lock, so `clear` reports
`already locked` until it ends. The download has no overall deadline; killing
the tick is safe. The [incident runbook](incident-runbook.md) owns what to do
next.

## Metrics

Each tick writes a [Prometheus textfile](https://github.com/prometheus/node_exporter#textfile-collector)
(`status.prom`); every series carries `chain` and `app_address` labels.

| Series | Meaning |
|---|---|
| `cartesi_watchdog_status{state="ok\|warning\|failed"}` | Exactly one is 1: exit 0, 1, or 2 |
| `cartesi_watchdog_divergence_info{kind}` | Present while a divergence is latched |
| `cartesi_watchdog_head_block` | The head's L1 block: the last block the sequencer agreed with, or the bootstrap block before the first agreement |
| `cartesi_watchdog_last_tick_timestamp_seconds` | When the last tick finished |

A textfile-collector sample carries the scrape time, not the tick time, so a
hung, killed, or lock-blocked tick leaves the previous `ok` in place. Alert on
all three:

```promql
cartesi_watchdog_status{state="failed"} == 1
# Three missed ticks at a 5-minute interval:
time() - cartesi_watchdog_last_tick_timestamp_seconds > 900
# With a `for:` of a few tick intervals:
cartesi_watchdog_status{state="warning"} == 1
```

Golden files: [`tests/fixtures/watchdog_status_ok.prom`](../../tests/fixtures/watchdog_status_ok.prom),
[`tests/fixtures/watchdog_status_failed.prom`](../../tests/fixtures/watchdog_status_failed.prom).

## Detection boundary

The watchdog and the sequencer's content-identity check catch different
failures; neither subsumes the other.

The content-identity check runs inside the input reader's atomic safe-input
sync. For every landing after baseline block `C` that the mirrored scheduler
accepts, it requires a byte-identical valid local sealed batch at that nonce; a
foreign or mismatched landing persists `canonical_divergence`, which freezes the
accepted frontier
([I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen)).
The finalized routes then answer 503, so the watchdog cannot compare that
block. The check shares the sequencer's acceptance predicate and does not
replay application execution. The watchdog catches direct-input, user-op,
scheduler, or application-state divergence once a comparable checkpoint is
published.

Accepted limit: a watchdog initialized while the sequencer already serves a
wrong state at the bootstrap block idles until the next accepted block.
Successful `init` or an idle tick is not evidence of a comparison.

## Why it is built this way

- **Two state sources, one executable.** An inspect `state` handler is not a
  reasonable contract for every application, and one report cannot exceed
  2 MiB; an application that keeps its state in a machine memory range is
  compared on that range. A library with application-supplied extraction hooks
  waits for an application these two sources cannot serve.
- **In-process execution, the CLI as oracle.** The optimizations (reflink
  clones, in-place mapping) live in the emulator, and v0.21 enforces the
  dangerous part of the host semantics itself (no input after a reject without
  a revert). Running the CLI per tick would add process management and a full
  machine store per tick for semantics we get anyway, so the watchdog mirrors
  the CLI's `--revert-mode=stored` loop and tests against the CLI.
- **Flat SHA-256 digests.** The comparison is an equality check between two
  parties we operate, for states expected to reach several GiB. SHA-256 is the
  cheapest on the sequencer and couples to nothing. A machine Merkle root
  answers a different question, checking the sequencer against a machine
  commitment without re-executing; it needs the sequencer to reimplement the
  emulator's hash tree, and it waits for such a verifier. Reading the range in
  one `read_memory` call holds it twice for a moment (the emulator's buffer
  and the Lua string). If that binds, read the range in chunks and stream the
  same SHA-256: the protocol, the sequencer (which already streams its file),
  and `sha256sum` on the evidence files stay as they are. The emulator exports
  only one-shot hashes, so streaming needs an incremental SHA-256 module,
  vendored and built like LuaFileSystem.
- **Latch first, evidence after the signal.** Evidence for a multi-GiB state
  can take minutes and fail on a full disk; a divergence must neither wait for
  it nor be lost with it. The sequencer's file comes first because the
  watchdog can fetch it only while the sequencer still serves B, and the
  sequencer removes it once it moves on; `replay` can always rebuild the
  canonical side.

## Code map

`watchdog/`:

- `main.lua`: command dispatch, exit codes, `last_tick.json` and `status.prom`.
- `tick.lua`: one compare cycle. `canonical.lua`: advance a checkpoint through L1
  inputs (shared by `tick` and `replay`). `bootstrap.lua`: `init`. `replay.lua`.
- `machine.lua`: the in-process Cartesi Machine: executor with snapshot
  semantics, machine status, state sources, publishing.
- `l1.lua`, `abi.lua`, `jsonrpc.lua`: complete InputBox inputs from L1.
- `sequencer.lua`, `http.lua`: the sequencer's operator routes over lua-curl.
- `incident.lua`: the latch, evidence collection, and `clear`. `store.lua`: the state
  directory. `config.lua`, `metrics.lua`, `errors.lua` (failure classes).
- `sequencer-watchdog`: the production wrapper. `test-guest/`: a guest that
  drives every host outcome, for tests.

Native modules are vendored and built into `.deps/lua` by
`just watchdog-lua-deps`: lua-curl (`lcurl.so`, needs libcurl) and
LuaFileSystem (`lfs.so`). The `cartesi` module ships with the emulator.

## Tests

| Command | What it exercises |
|---|---|
| `just test-watchdog` | Unit tests: every module against fakes, including the shared partition vector and metrics golden files |
| `just watchdog lint` | `luacheck` |
| `just test-watchdog-e2e` | The real emulator with the test guest: executor against the CLI oracle (accept, reject, exception, halt), both state sources, full ticks, `replay`, and the wallet image's golden genesis state. Builds the test guest image on first use; needs the sepolia canonical image |
| `just test-rollups-e2e` | Devnet scenarios: the watchdog genesis compare and `watchdog_divergence_drill_test` on the Rust host, and the non-genesis compare through both hosts |
| `just watchdog docker-smoke` | The release image: native modules, commands, and the lock |
| `just doctor` | Toolchain: Lua, emulator, native modules, and the devnet image's state query |
