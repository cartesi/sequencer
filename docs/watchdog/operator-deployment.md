# Deploying the Watchdog

This guide deploys the watchdog next to a sequencer on a live chain: Sepolia,
where the procedure is rehearsed, and mainnet. The [watchdog README](README.md)
owns how the watchdog works, its commands, configuration, state directory, and
metrics; this guide links there instead of repeating them.
[Local development](getting-started.md) covers the devnet, and the
[incident runbook](incident-runbook.md) covers divergences.

In order: open the network paths, install the runtime, obtain the canonical
machine image, choose the state source, prepare storage, collect the
deployment facts, run `init`, and schedule `tick`.

## Network access

The watchdog only makes outbound requests: to an L1 JSON-RPC endpoint and to
the sequencer's operator routes.

| Route | When the watchdog calls it |
|---|---|
| `GET /finalized_state/inclusion_block` | Every tick |
| `GET /finalized_state/digest` | When the sequencer's accepted block has advanced |
| `GET /finalized_state` | After a mismatch, to keep the sequencer's bytes as evidence |

The sequencer serves these routes on the same listener as its public API,
without authentication. The
[API reference](../../README.md#operator-snapshot-endpoints-internal-only)
owns them and why they must stay private: expose only the public routes at the
public ingress, and give the watchdog host an internal path to the operator
routes. The sequencer hashes the comparison file on each digest request and
the watchdog waits up to 300 seconds for the answer, so a proxy in between
needs a read timeout at least that long for multi-GiB states.

Check both routes from the watchdog host:

```bash
curl -fsS "$CARTESI_WATCHDOG_SEQUENCER_URL/finalized_state/inclusion_block"
curl -fsS "$CARTESI_WATCHDOG_SEQUENCER_URL/finalized_state/digest"
```

Both answer JSON once the sequencer has a comparable checkpoint and 404 before
that: genesis is comparable at block zero, while a rebuilt baseline waits for
its next accepted batch
([snapshot lifecycle](../snapshots/lifecycle.md#acceptance-and-comparison)).
They answer 503 while the sequencer's own divergence marker is set.

The L1 RPC must serve `eth_chainId`, the `safe` block tag, `eth_getLogs` on the
InputBox, `eth_getCode`, and `eth_call` at past blocks: `init` reads the
InputBox count at the bootstrap block, and every comparison reads it at the
comparison block ([L1 completeness](README.md#l1-completeness)). Use an archive
node when the bootstrap block, or a block you `replay` from, is older than the
state your provider keeps. Long log ranges are split when the provider answers
with one of the codes `-32005,-32012,-32600,-32602,-32616`; if yours uses
another, set `CARTESI_WATCHDOG_LONG_BLOCK_RANGE_ERROR_CODES` at `init`.

## Runtime

### Release image

Each release tag `vX` publishes a multi-arch (`linux/amd64`, `linux/arm64`)
image built from [`watchdog/Dockerfile`](../../watchdog/Dockerfile):

```bash
docker pull ghcr.io/cartesi/sequencer-watchdog:vX   # mirror: docker.io/cartesi/sequencer-watchdog:vX
```

Its entrypoint is the `sequencer-watchdog` wrapper. The image carries the Lua
sources, the native modules (lua-curl as `lcurl`, LuaFileSystem as `lfs`), the
pinned `cartesi-machine` with its `cartesi` Lua module, and `flock`; tests run
from a checkout, not from the image. Run the sequencer, the watchdog image, and
the canonical machine image from the same release tag. To see what an image
holds:

```bash
docker run --rm -e CARTESI_WATCHDOG_PRINT_RELEASE_INFO=1 ghcr.io/cartesi/sequencer-watchdog:vX
```

It prints `/opt/watchdog/RELEASE.json` (release tag, commit, and Cartesi
Machine version) and `cartesi-machine --version`.

Mount one host directory holding both the state directory and the bootstrap
machine, so they share a filesystem ([storage](#storage)), and pass the
environment from a file ([Sepolia](#sepolia) has an example):

```bash
docker run --rm --env-file watchdog.env -v /srv/watchdog:/srv/watchdog \
  ghcr.io/cartesi/sequencer-watchdog:vX init
docker run --rm --env-file watchdog.env -v /srv/watchdog:/srv/watchdog \
  ghcr.io/cartesi/sequencer-watchdog:vX tick
```

### Local build

From a checkout, with `cartesi-machine` at `CARTESI_MACHINE_VERSION` from
[`toolchain-pins.env`](../../toolchain-pins.env) and its Lua module installed,
Lua 5.4, and `flock`:

```bash
just watchdog-lua-deps   # compiles lcurl.so and lfs.so into .deps/lua
export CARTESI_WATCHDOG_LUA_ROOT="$PWD" CARTESI_WATCHDOG_LUA_DEPS="$PWD/.deps/lua"
./watchdog/sequencer-watchdog status
```

The modules compile from vendored sources and need a C compiler, the Lua 5.4
headers, and libcurl development files that `pkg-config` finds. The
Dockerfile's build and runtime stages list the Debian packages for each. The
wrapper also searches the Lua paths where the emulator's Debian package
installs its module.

## Canonical machine image

A genesis `init` stores the canonical machine image the application was
deployed with: its root hash is the application's on-chain template hash, and
`init` refuses an image that differs. Take it for this chain from the release,
as `canonical-machine-image-<chain>-vX.tar.gz` (checked against the release's
`SHA256SUMS`), or build it:

| Chain | Recipe | Output |
|---|---|---|
| Sepolia | `just canonical-build-machine-image-sepolia` | `examples/canonical-app/out/canonical-machine-image-sepolia` |
| Devnet, local only | `just canonical-build-machine-image` | `examples/canonical-app/out/canonical-machine-image` |

The build prints the image's root hash. It needs the kernel
(`just canonical download-deps`), `cross`, Docker buildx able to build
`linux/riscv64`, `xgenext2fs`, and the pinned `cartesi-machine`. A stored
machine loads only in the Cartesi Machine version that stored it
([versions are all-or-nothing](../cartesi-machine.md#versions-are-all-or-nothing)),
so the image and the watchdog must use the same version.

## State source

`CARTESI_WATCHDOG_STATE_SOURCE` says how the watchdog extracts the application
state from the canonical machine, and `init` persists it
([state sources](README.md#state-sources)):

- **`inspect`** for the reference wallet, whose canonical guest answers the
  inspect query `state` with the SSZ bytes the sequencer serves. One report
  holds at most 2 MiB, so this source suits small states only.
- **`range:<label>`** for an application that keeps its state in a flash drive
  or NVRAM with the user label `<label>`. The watchdog reads the whole range
  without running the guest, and the sequencer's comparison file must be
  exactly those bytes: the "Comparison bytes" rules of the
  [application contract](../protocol/application-contract.md#6-checkpoint-lifecycle)
  own them. `init` refuses an image in which no range, or more than one,
  carries the label.

## Storage

### Filesystem

Put the state directory on a persistent volume, and on a copy-on-write
filesystem (XFS with reflink, btrfs) when the machine is large. Every tick
clones the head into a working machine and snapshots it before each input. On
a copy-on-write filesystem a clone costs metadata; elsewhere it is a full
sparse copy of the machine, which makes multi-GiB states slow
([Cartesi Machine storage](../cartesi-machine.md#storage-sharing-and-snapshots)).
Keep the bootstrap machine on the same filesystem, so the clone `init` makes
of it is cheap as well.

### Checkpoint disk usage and backups

Disk usage is roughly one machine for the head, plus a working clone and a
per-input snapshot while a tick runs (nearly free on copy-on-write), plus
incident evidence: the canonical machine at the divergence and two copies of
the comparison bytes. Cleared incidents stay under `incidents/` until you
remove them. With a `range` source, the watchdog also reads the whole range
into memory and hashes it in one piece, which takes about twice the range in
memory ([hashing](../cartesi-machine.md#hashing)).

Back up, from the state directory:

- the newest `checkpoints/<block>-<count>/`, the head: the stored machine at
  L1 block `<block>`, after `<count>` InputBox inputs, which the sequencer
  agreed with (or the trusted bootstrap, before the first agreement);
- `incident/` while a divergence is latched, and `incidents/`.

`init` writes `config.json`, so it needs no backup. Checkpoints are published
atomically, so the newest one is complete once a tick has finished. Run the
backup after the tick in the same scheduled job: the next tick removes the
previous head as soon as it publishes a new one. Names are zero-padded, so the
last one in sorted order is the newest. Stored machines are sparse files; copy
them with a sparse-aware tool (`cp --sparse=always`, `rsync --sparse`,
`tar --sparse`). Keep copies outside `checkpoints/`, which must hold nothing
but checkpoints.

A retained checkpoint is a trusted machine at a known block. It can bootstrap
a new state directory ([initialize](#initialize)), serve as the trusted source
for `replay` in the [runbook](incident-runbook.md#2-find-the-faulty-side), and
feed the canonical-to-native export of
[cockroach recovery](../recovery/cockroach.md#incident-playbook). It is not a
sequencer restore archive; that is `/finalized_snapshot`
([recovery exports](../snapshots/lifecycle.md#http-and-recovery-exports)).
Complete the application's
[recovery readiness drill](../recovery/cockroach.md#recovery-readiness-before-deployment)
before production.

## Deployment facts

The [configuration table](README.md#configuration) defines each variable; this
table says where its value comes from.

| Fact | Source | Variable |
|---|---|---|
| Chain id | The L1 network: `11155111` for Sepolia, `1` for mainnet. Pin it: `init` refuses an RPC that serves another chain | `CARTESI_WATCHDOG_BLOCKCHAIN_ID` |
| Application | The deployment; the sequencer's `CARTESI_SEQUENCER_APP_ADDRESS` | `CARTESI_WATCHDOG_APP_ADDRESS` |
| Sequencer URL | The internal address of the sequencer's listener ([network access](#network-access)) | `CARTESI_WATCHDOG_SEQUENCER_URL` |
| L1 RPC | Your provider ([network access](#network-access)); read at run time, never persisted | `CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT` |
| State source | The application ([state source](#state-source)) | `CARTESI_WATCHDOG_STATE_SOURCE` |
| Bootstrap machine and block | [Initialize](#initialize) | `CARTESI_WATCHDOG_CM_SNAPSHOT_DIR`, `CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK` |
| State directory | An absolute path on the [storage](#storage) above | `CARTESI_WATCHDOG_STATE_DIR` |

## Initialize

`init` stores a trusted bootstrap machine as the first checkpoint and writes
`config.json` ([commands](README.md#commands)):

```bash
sequencer-watchdog init
```

Choose the bootstrap:

- **Genesis.** The chain's canonical machine image, with
  `CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK` set to the block before the
  application's deployment block:

  ```bash
  cast call "$CARTESI_WATCHDOG_APP_ADDRESS" 'getDeploymentBlockNumber()(uint256)' \
      --rpc-url "$CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT"
  ```

  No input precedes that block, so `init` checks the image against the
  application's on-chain template hash. The sequencer reports its genesis at
  block 0, behind this block, so ticks idle until its first accepted batch
  passes it. The first comparing tick replays the whole input history; run it
  by hand before enabling the heartbeat alert.
- **A trusted checkpoint** at block `X`: a retained `checkpoints/<X>-<count>/`
  of this deployment, or a `replay --out` directory, with
  `CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK=X`. `init` checks that the machine
  waits for an input and yields the state source. It cannot check that its
  state is right: a wrong bootstrap surfaces as a `state_mismatch` at the next
  comparison, the runbook's "watchdog wrong" case. The
  [detection boundary](README.md#detection-boundary) states what `init` and
  idle ticks do not prove.

Either way, `init` reads the InputBox count at the bootstrap block, so its RPC
needs state at that block: an archive node for old blocks.

`init` is idempotent. On an initialized state directory it only checks that the
chain, application, and state source match, then exits 0, so a job
may run `init` before every `tick`; it still needs the full `init`
environment. It refuses a directory initialized for another deployment or state
source. An interrupted `init` leaves no `config.json`, and running it again
starts over.

## Schedule `tick`

`tick` runs one compare cycle and exits ([the tick](README.md#the-tick)). Run
it from a scheduler that never overlaps two runs: a systemd `Type=oneshot`
service driven by a timer (a timer does not start a unit that is still
running), or a Kubernetes CronJob with `concurrencyPolicy: Forbid` and a
persistent volume for the state directory. The wrapper's lock also refuses an
overlapping `init`, `tick`, or `clear`, which then exits 1.

The interval is the detection delay you accept. An idle tick makes one HTTP
request. A comparison happens only when the sequencer accepts a new batch, and
that tick runs as long as replaying the new inputs takes.

Each tick writes `status.prom`, a Prometheus textfile, into the state directory
or to `CARTESI_WATCHDOG_METRICS_FILE`; point that at the node_exporter
textfile-collector directory, with a name ending in `.prom`. Alert on the three
conditions in [metrics](README.md#metrics): a latched divergence (`failed`), a
stale last-tick timestamp, and a `warning` that persists for a few intervals.

Keep the watchdog's stderr. Each tick logs a `watchdog: tick: …` line with its
outcome or failure class; a newly latched divergence also writes a
`watchdog_event` JSON line, and the latching tick's log carries the guest's
console output.

## Responding to alerts

- **Divergence (`failed`).** Follow the [incident runbook](incident-runbook.md).
- **Stale heartbeat.** No tick has finished recently: the scheduler stopped, a
  tick hangs or is still replaying a long range, or every run exits at the lock
  because another process holds it.
- **Sustained warning.** `sequencer-watchdog status` shows the last tick's
  `outcome`, which is the failure class (`transient`, `operator`, or
  `internal`), and its `message`; [troubleshooting](#troubleshooting-live-deployments)
  lists the common ones. `status` only reads, and works while a tick runs.

## Upgrades

A release with the same Cartesi Machine version replaces the image or the
checkout, and the state directory carries over.

`config.json` carries a format version, currently 2, and the watchdog refuses
any other: wipe the state directory and run `init` again.

A deployed application is pinned to the Cartesi Machine version its template
was built with ([why](../cartesi-machine.md#versions-are-all-or-nothing)), and
so is its watchdog: run a watchdog release whose `cartesi_machine_version`
(printed with `CARTESI_WATCHDOG_PRINT_RELEASE_INFO=1`) matches the version the
application's template was built with. A release on another version cannot
load the state directory's machines and cannot reproduce the template.

## Sepolia

| Item | Value |
|---|---|
| Chain id | `11155111` |
| InputBox | `0x346B3df038FE9f8380071eC6514D5a83aD143939`: rollups-contracts v3.0.0-alpha.6, the version the root `justfile` pins, deploys it at the same address on every chain, the devnet included |
| Canonical image | `canonical-machine-image-sepolia-vX.tar.gz`, or `just canonical-build-machine-image-sepolia`. Its guest runs `WalletConfig::sepolia()`, the configuration of the release `wallet-sequencer` binary |
| State source | `inspect` |
| Application | Per deployment |

Public endpoints such as `https://eth-sepolia.rollups.cartesi.io/v2`, the
default of [`tests/scripts/demo_sepolia.py`](../../tests/scripts/demo_sepolia.py),
serve wallets; point the watchdog at the sequencer's internal address.

```bash
# watchdog.env
CARTESI_WATCHDOG_SEQUENCER_URL=http://<internal-sequencer>
CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT=https://<sepolia-archive-rpc>
CARTESI_WATCHDOG_BLOCKCHAIN_ID=11155111
CARTESI_WATCHDOG_APP_ADDRESS=<application>
CARTESI_WATCHDOG_STATE_SOURCE=inspect
CARTESI_WATCHDOG_STATE_DIR=/srv/watchdog/state
CARTESI_WATCHDOG_CM_SNAPSHOT_DIR=/srv/watchdog/canonical-machine-image-sepolia
CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK=<deployment block - 1>
```

Before mainnet, run this procedure on Sepolia with the alerts wired, and the
runbook's [staging drill](incident-runbook.md#drills) with the operators who
will be paged.

## Mainnet

Mainnet follows the Sepolia procedure with chain id `1`. This repository has
no mainnet wallet configuration or canonical image: `WalletConfig` defines
Sepolia and devnet constants only. A mainnet application needs a canonical
image built with the same constants as its sequencer binary and deployed as its
template; the watchdog derives the InputBox from the application. Plan the
first bootstrap as either a genesis replay through an archive RPC or a trusted
checkpoint from a watchdog that already follows the deployment.

## Sequencer restart policy

The sequencer's [exit codes](../../README.md#running) are its whole restart
contract: there is no database gate and no acknowledgement command. Standard
recovery is automatic on every boot, and a persistent terminal fault
re-detects fail-loud when the faulty state is next read, so the supervisor's
handling of 30 and SIGABRT (stop and page, do not auto-restart) is what bounds
a crash loop.

- **Supervisor recipes.** systemd can act on the code directly:
  `RestartPreventExitStatus=30 ABRT` (SIGABRT/134 is terminal-class too).
  Kubernetes Deployments restart regardless of exit code, and there is no
  boot gate — so on k8s a terminal exit will restart-loop through
  re-detection windows, serving traffic in between. There, the crash-loop
  bound is your alerting, not the restart policy: page immediately on
  `lastState.terminated.exitCode == 30` (and on signal exits / 134).
- **After an unclean death (OOM, node reboot, SIGKILL) no action is
  needed**: the next start re-derives everything from facts. For
  postmortems, the `terminal_faults` table records the cause of every
  command that returned through its bracket with a terminal verdict
  (best-effort, append-only, traveling with the data directory —
  `SELECT * FROM terminal_faults ORDER BY fault_id DESC`; the next `run`
  also logs the latest row once at startup). Any death that did not return
  through the bracket — SIGKILL, OOM, a node reboot, a terminal runtime abort,
  a controller panic — leaves only the process logs.
- **Untrustworthy local state requires manual recovery.** The reader's
  content-identity divergence marker freezes the acceptance frontier and
  blocks admission ([I15](../invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen)).
  A watchdog state mismatch independently signals application-state
  disagreement ([incident runbook](incident-runbook.md)). Diagnose the cause
  and follow the [incident playbook](../recovery/cockroach.md#incident-playbook)
  for a fresh-directory rebuild when needed; standard recovery does not repair
  bugs.

## Troubleshooting (live deployments)

The wrapper prints the lock message itself; the watchdog prints the others
after the command and failure class, as in
`watchdog: tick: transient failure: <message>`.

| Message | Cause |
|---|---|
| `watchdog state is already locked: <dir>/run.lock` | Another `init`, `tick`, or `clear` holds the state directory, so the scheduler overlaps runs |
| `flock is required (util-linux)` | Install util-linux; the wrapper needs `flock` for its lock |
| `state directory <dir> does not exist` | `status` or `replay` with a wrong `CARTESI_WATCHDOG_STATE_DIR` |
| `<dir>/config.json is not valid JSON` | A crash tore `config.json` right after `init`; wipe the state directory and re-run `init` |
| `<dir> is not initialized; run init` | `tick` or `replay` on an empty state directory: a missing volume or a changed `CARTESI_WATCHDOG_STATE_DIR` |
| `<dir>/config.json is not a version 2 watchdog config; wipe the state directory and re-run init` | See [upgrades](#upgrades) |
| `cannot load stored machine <dir>: …` | A machine stored by another Cartesi Machine version; see [upgrades](#upgrades) |
| `<dir> was initialized for another deployment or state source; wipe it to re-initialize` | The `init` environment names another chain, application, or state source than the directory holds |
| `no input precedes block <N>, so the machine at <dir> must be the application's template, but its root hash differs from the on-chain template hash` | The image is not this application's template: another chain's image, another build, or another application |
| `L1 RPC: eth_call: …` during `init` | The RPC has no state at the bootstrap block; use an archive node |
| `sequencer /finalized_state/inclusion_block: HTTP 404` | No comparable checkpoint yet, or the URL reaches the wrong listener |
| `sequencer /finalized_state/digest: HTTP 404` while `inclusion_block` answers | A proxy does not forward the digest route, or the sequencer comes from another release |
| `sequencer /finalized_state/inclusion_block: HTTP 503`, with `canonical divergence prevents accepted checkpoint selection` | The sequencer's own divergence marker is set: a sequencer incident for [cockroach recovery](../recovery/cockroach.md#incident-playbook) |
| `L1 RPC safe head <N> is behind target block <B>` | The RPC lags the sequencer's L1 view; the next tick retries |
| `InputBox counts <N> inputs at block <B> but the scan reached index <M>`, or `InputBox index <N> where <M> was expected: the L1 view is incomplete` | The provider returned incomplete logs; switch providers if it persists |
| `the L1 RPC does not serve chain <N>` | The RPC endpoint belongs to another chain |
| `no flash drive or NVRAM carries the label "<label>"` | The state source does not match the image |
