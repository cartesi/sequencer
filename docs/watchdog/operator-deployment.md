# Watchdog — operator deployment (Sepolia and mainnet)

This is the **production-like** runbook for running the watchdog next to a **live** sequencer on a public L1 (Sepolia today, mainnet when deployed).

**Sepolia is the dress rehearsal for mainnet.** The watchdog process, compare algorithm, internal snapshot API, checkpoint model, and network boundaries are the same. What changes per chain is: L1 RPC URL, deployed contract addresses, CM machine image build, wallet portal/token constants, and poll cadence.

For **local development only** (Anvil + `sequencer-devnet`, CI smoke tests), use [`getting-started.md`](getting-started.md) instead.

## Two deployment tiers

```text
                    ┌─────────────────────────────────────┐
  Internet / users  │  Public ingress (POST /tx, WS)     │  ← benchmarks, wallets
                    └─────────────────┬───────────────────┘
                                      │
                    ┌─────────────────▼───────────────────┐
  Operator network  │  Sequencer process                 │
                    │  + internal snapshot HTTP          │  ← watchdog ONLY here
                    │    /finalized_state*             │
                    └─────────┬───────────────┬─────────┘
                              │               │
                    ┌─────────▼───┐   ┌───────▼────────┐
                    │  L1 (Sepolia │   │  Watchdog host │
                    │   or mainnet)│   │  (compare)    │
                    └─────────────┘   └────────────────┘
```

The watchdog never substitutes for the sequencer. It reads **finalized SSZ** the sequencer already committed and independently replays L1 through the canonical CM.

---

## Shared operator checklist (Sepolia and mainnet)

Use this checklist for any live deployment. Chain-specific values are in the tables below.

### 1. Network access

- [ ] Watchdog host can reach **internal** `CARTESI_WATCHDOG_SEQUENCER_URL` (not only the public `/tx` URL).
- [ ] Watchdog host can reach **L1 JSON-RPC** with `eth_getLogs` (archive recommended if replaying long history).
- [ ] `/finalized_state` is **not** exposed on the public internet.

Verify snapshot API before CM bootstrap:

```bash
curl -sS -o /dev/null -w "%{http_code}\n" "$CARTESI_WATCHDOG_SEQUENCER_URL/finalized_state/inclusion_block"
# expect 200 when a finalized snapshot exists (404 = not promoted yet or wrong host)
```

### 2. Watchdog runtime (release image or local build)

**Production (recommended):** pull the **release container image** for tag `vX` — same
git tag as the sequencer binary and `canonical-machine-image-*-vX.tar.gz`.

**Container images (preferred for Dockerfile / Fly.io assembly):**

| Registry | Image |
|----------|-------|
| GHCR | `ghcr.io/cartesi/sequencer-watchdog:vX` |
| Docker Hub | `docker.io/cartesi/sequencer-watchdog:vX` |

Multi-arch manifest (`amd64` + `arm64`).

Verify alignment via `release-manifest-vX.json` and `/opt/watchdog/RELEASE.json`
inside the image. Toolchain pins live in [`toolchain-pins.env`](../../toolchain-pins.env).

`cartesi-machine` in the watchdog image **must** match
`CARTESI_MACHINE_VERSION` in [`toolchain-pins.env`](../../toolchain-pins.env)
(the emulator that built the CM image tarball). Mismatch causes load failures
or false `state_mismatch`.

**Compose a custom image (e.g. Fly.io rootfs)** — base on `debian:trixie-slim`,
install the same runtime packages as the release image, then copy from the
published watchdog image:

```dockerfile
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    lua5.4 libcurl4 libslirp0 libgomp1 ca-certificates util-linux \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ghcr.io/cartesi/sequencer-watchdog:vX /opt/watchdog /opt/watchdog
COPY --from=ghcr.io/cartesi/sequencer-watchdog:vX /usr/local/bin/sequencer-watchdog /usr/local/bin/sequencer-watchdog
COPY --from=ghcr.io/cartesi/sequencer-watchdog:vX /usr/local/bin/cartesi-machine /usr/local/bin/cartesi-machine
COPY --from=ghcr.io/cartesi/sequencer-watchdog:vX /usr/local/lib/ /usr/local/lib/
COPY --from=ghcr.io/cartesi/sequencer-watchdog:vX /usr/local/lib/lua/ /usr/local/lib/lua/
COPY --from=ghcr.io/cartesi/sequencer-watchdog:vX /usr/local/share/lua/ /usr/local/share/lua/
COPY --from=ghcr.io/cartesi/sequencer-watchdog:vX /usr/local/share/cartesi-machine/ /usr/local/share/cartesi-machine/

ENV CARTESI_WATCHDOG_LUA_DEPS=/opt/watchdog/lib \
    LUA_PATH="/opt/watchdog/lua/?.lua;/opt/watchdog/lua/?/init.lua;/usr/local/share/lua/5.4/?.lua;/usr/local/share/lua/5.4/?/init.lua;;" \
    LUA_CPATH="/opt/watchdog/lib/?.so;/usr/local/lib/lua/5.4/?.so;;" \
    LD_LIBRARY_PATH="/usr/local/lib" \
    PATH="/usr/local/share/cartesi-machine:${PATH}"

ENTRYPOINT ["/usr/local/bin/sequencer-watchdog"]
```

**Run from the published image:**

```bash
docker pull ghcr.io/cartesi/sequencer-watchdog:vX

docker run --rm \
  -e CARTESI_WATCHDOG_SEQUENCER_URL="https://<internal-sequencer>" \
  -e CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS="0x..." \
  -e CARTESI_WATCHDOG_APP_ADDRESS="0x..." \
  -e CARTESI_WATCHDOG_STATE_DIR=/watchdog-state \
  -e CARTESI_WATCHDOG_CM_SNAPSHOT_DIR=/cm-bootstrap \
  -e CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK="<bootstrap inclusion_block>" \
  -v /var/lib/watchdog/state:/watchdog-state \
  -v /var/lib/watchdog/cm-bootstrap:/cm-bootstrap:ro \
  ghcr.io/cartesi/sequencer-watchdog:vX init

docker run --rm \
  -e CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT="https://<archive-rpc>" \
  -e CARTESI_WATCHDOG_STATE_DIR=/watchdog-state \
  -v /var/lib/watchdog/state:/watchdog-state \
  ghcr.io/cartesi/sequencer-watchdog:vX tick
```

**Local / dev build:**

```bash
eval "$(direnv export bash 2>/dev/null)"
just watchdog-lua-deps    # .deps/lua/lcurl.so — needs libcurl + Lua dev headers; see README
```

Host packages and build errors: [`README.md` — Host dependencies](README.md#host-dependencies-watchdog-lua-deps).

Requires: `lua`, `cartesi-machine` (in-process `cartesi` Lua module),
libcurl + Lua headers, and a scheduler non-overlap guard. The release image
installs Linux `flock` from `util-linux`; for Nix shells, the package is
`nixpkgs#util-linux`. Pin `cartesi-machine` to the same version as your CM
bootstrap tarball.

### 3. Build the CM image for **this chain**

The RISC-V guest must use the same wallet/scheduler constants as the deployed app.

| Chain | Command | Image directory |
|-------|---------|-----------------|
| **Sepolia** | `just canonical-build-machine-image-sepolia` | `examples/canonical-app/out/canonical-machine-image-sepolia` |
| **Local devnet** | `just canonical-build-machine-image` | `.../canonical-machine-image` (not for public L1) |
| **Mainnet** | *Ship a mainnet-targeted guest build when available* | Match production scheduler artifact |

Today `WalletApp::default()` / `WalletConfig::sepolia()` align with Sepolia staging; mainnet production will need matching mainnet portal/token addresses in app-core before rebuilding the CM image.

### 4. Collect deployment facts

| Variable | Where it comes from |
|----------|---------------------|
| `CARTESI_WATCHDOG_SEQUENCER_URL` | Ops: internal HTTP base (see network diagram) |
| `CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT` | Ops: current chain RPC for `tick` (archive for historical `getLogs`; not persisted by `init`). Also needed at `init` if auto-detecting `BLOCKCHAIN_ID` via `eth_chainId` |
| `CARTESI_WATCHDOG_APP_ADDRESS` | This rollup’s Cartesi **application** contract (normalized to lowercase `0x`+hex at load) |
| `CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS` | InputBox on that L1 ([Cartesi deployed contracts](https://docs.cartesi.io/cartesi-rollups/2.0/deployment/self-hosted.md); same lowercase normalization) |
| `CARTESI_WATCHDOG_STATE_DIR` | Persistent volume on watchdog host. If the path embeds an address, use **lowercase** — Linux paths are case-sensitive and EIP-55 vs lowercase create sibling dirs |
| `CARTESI_WATCHDOG_CM_SNAPSHOT_DIR` | Bootstrap CM snapshot (`init` only) |
| `CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK` | L1 block that bootstrap snapshot represents (= finalized `inclusion_block` at bootstrap) |
| `CARTESI_WATCHDOG_BLOCKCHAIN_ID` | Chain id label for `status.prom` metrics (prefer set at `init`; optional auto-detect via `eth_chainId` when L1 endpoint is present at `init`) |
| `CARTESI_WATCHDOG_METRICS_FILE` | Override path for the Prometheus textfile written by each `tick` |
| `CARTESI_WATCHDOG_LUA_DEPS` | `.deps/lua` |
| `CARTESI_WATCHDOG_LONG_BLOCK_RANGE_ERROR_CODES` | Optional CSV of RPC error codes that trigger `eth_getLogs` partition retry. **Evaluated only at `init` and persisted in `config.json`** — not a tick-time override; re-running idempotent `init` does not refresh it. Wipe state and re-init (or edit `config.json`) to change. Default matches the sequencer: `-32005,-32012,-32600,-32602,-32616`. |

The sequencer discovers and pins `input_box_address` at startup; use the same values as `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT` / `CARTESI_SEQUENCER_APP_ADDRESS` configuration.

### 5. Initialize watchdog state (first run on a live chain)

On a long-lived deployment, **`CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK=0` is usually wrong** unless finalized state is still at genesis.

Pick one:

1. **Ops hands off** a CM snapshot directory + block number matching current finalized `inclusion_block`, or
2. **Watchdog reuses** `CARTESI_WATCHDOG_STATE_DIR` from a prior run on this deployment, or
3. **Replay from genesis** (only for new rollups / low block height — slow).

Run `init` once to store the bootstrap CM snapshot into the watchdog state
layout. Re-running `init` on a **complete** already-initialized state directory
is a no-op success (exit `0`), matching `sequencer setup` — safe for process
supervisors that always invoke init before tick. If `head.json` exists but
`config.json` or the selected snapshot is missing/corrupt, `init` fails (exit
`1`) and asks you to wipe `state_dir` and re-run — it will not certify an
unusable state. The L1 RPC URL is not persisted — each `tick` reads
`CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT` so it can rotate without editing
state. If `CARTESI_WATCHDOG_BLOCKCHAIN_ID` is unset at `init`, auto-detect also
needs that endpoint present then (prefer setting the chain id explicitly):

```bash
sequencer-watchdog init
```

After init, schedule `tick`; tick will fail if `head.json` is missing.

### Missing or corrupt `head.json` (tick exit `1`)

`tick` never bootstraps a checkpoint from env. A missing/corrupt head is an
operator/state error (not a transient RPC/L1 failure): the compare cycle fails
fast, writes `status.prom` with `state="warning"`, and does not burn the retry
budget.

1. `ls $CARTESI_WATCHDOG_STATE_DIR` — expect `config.json`, `head.json`, and
   `checkpoints/<safe_block>/`.
2. If `head.json` is missing: run `sequencer-watchdog init` with
   `CARTESI_WATCHDOG_CM_SNAPSHOT_DIR` and `CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK`
   set to a CM snapshot whose safe block equals the sequencer's current
   finalized `inclusion_block`, then run `tick`.
3. Schedule `sequencer-watchdog init && sequencer-watchdog tick` (init is a
   no-op when state is already complete).
4. If `head.json` exists but `config.json` or the selected snapshot is
   missing/corrupt: wipe `state_dir` and re-init.
5. Ensure `CARTESI_WATCHDOG_STATE_DIR` is a persistent volume (empty/ephemeral
   dirs lose `head.json` across restarts).

Each `tick` atomically writes a Prometheus textfile to
`$CARTESI_WATCHDOG_STATE_DIR/status.prom` (override with
`CARTESI_WATCHDOG_METRICS_FILE`). Operators can scrape or push it from their
side. Gauges:

- `cartesi_watchdog_status{chain,app_address,state="ok|warning|failed"}` — `1` on the active state
- `cartesi_watchdog_divergence_info{chain,app_address,kind}` — present on exit `2`

Exit mapping is `0→ok`, `1→warning`, `2→failed` (no separate exit-code /
last-tick gauges — Prom already timestamps samples).

Set `CARTESI_WATCHDOG_BLOCKCHAIN_ID` at `init` so `chain` is labeled. If unset,
`init` queries `eth_chainId` from `CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT` and
persists the result. At `tick`, the env var overrides a missing persisted value;
the exit path never blocks on RPC (falls back to `unknown` only when neither
source is set).

Example `status.prom` after a successful tick:

```prometheus
cartesi_watchdog_status{app_address="0x4ce...",chain="11155111",state="ok"} 1
cartesi_watchdog_status{app_address="0x4ce...",chain="11155111",state="warning"} 0
cartesi_watchdog_status{app_address="0x4ce...",chain="11155111",state="failed"} 0
```

On divergence (exit `2`), `state="failed"` is `1` and
`cartesi_watchdog_divergence_info{kind="state_mismatch"}` (or
`inclusion_block_regressed`) is present. Example alert:

```promql
cartesi_watchdog_status{state="failed"} == 1
```

Cron + push pattern (operator pushes to Prometheus after each tick):

```bash
#!/bin/sh
set -eu
sequencer-watchdog tick || true    # exit code still written to status.prom
# push $CARTESI_WATCHDOG_STATE_DIR/status.prom via your exporter
```

### 6. Run tick

The watchdog runs **one tick per process, then exits** — there is no daemon
loop. Run it once as a smoke check, then schedule it (cron, systemd timer, k8s
CronJob). Alert on `$CARTESI_WATCHDOG_STATE_DIR/status.prom` (preferred for
Prometheus push/pull) or on the process exit code. If the process is killed
mid-tick, `status.prom` keeps the last completed value until the next run.

```bash
sequencer-watchdog tick   # exit 0 = clean/idle, 1 = transient, 2 = divergence
```

`sequencer-watchdog` wraps `init` and `tick` with a non-blocking `flock` on
`$CARTESI_WATCHDOG_STATE_DIR/run.lock`, which is released by the kernel if the process
dies. Use the scheduler's non-overlap primitive as well (for example systemd or
Kubernetes CronJob `concurrencyPolicy: Forbid`). A leftover `run.lock` path is
only a lock handle; by itself it does not mean a lock is held.

When `inclusion_block` ≤ the watchdog checkpoint, the runner only hits `/finalized_state/inclusion_block` and skips L1/CM work.

---

## Sepolia (testnet staging)

Use Sepolia to validate **the same procedure** you will run on mainnet: internal URLs, alarms, checkpoint persistence, RPC limits, CM image version pinning.

### Sepolia-specific values

| Item | Typical source |
|------|----------------|
| Chain ID | `11155111` |
| Public user ingress (tx demos only) | e.g. `https://eth-sepolia.rollups.cartesi.io/v2` — **may not** serve `/finalized_state` |
| Application instance | Per deployment (confirm with ops; demos have used `0x4CE633CA71071818cD73187765ee60F696dae083`) |
| InputBox (rollups v3.0.0-alpha.6, deterministic cross-chain address) | Confirm against the [v3.0.0-alpha.6 release deployment addresses](https://github.com/cartesi/rollups-contracts/releases/tag/v3.0.0-alpha.6) (`0x346B3df038FE9f8380071eC6514D5a83aD143939` on Sepolia) |
| CM image | `just canonical-build-machine-image-sepolia` |
| Tx / deposit demos | `tests/scripts/demo_sepolia.py` (copy `.env` locally; never commit secrets) |

### Example env block (fill from ops)

```bash
export CARTESI_WATCHDOG_SEQUENCER_URL="https://<internal-sepolia-sequencer>"
export CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT="https://<sepolia-archive-rpc>"
export CARTESI_WATCHDOG_APP_ADDRESS="0x..."
export CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS="0x..."
export CARTESI_WATCHDOG_STATE_DIR="/var/lib/watchdog/state-sepolia"
export CARTESI_WATCHDOG_CM_SNAPSHOT_DIR="/path/to/canonical-machine-image-sepolia"
export CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK="<finalized inclusion_block at bootstrap>"
export CARTESI_WATCHDOG_LUA_DEPS="/path/to/sequencer/.deps/lua"
```

### Operating the Sepolia sequencer

If your team runs the sequencer on Sepolia (not only the public endpoint):

1. `sequencer` / release binary with Sepolia `CARTESI_SEQUENCER_*` (chain id, app address, batch submitter key, L1 RPC).
2. Inclusion lane promotes finalized snapshots when L1 safe advances — required for `/finalized_state` 200.
3. Snapshot routes on an **internal** bind / port reachable by the watchdog host.
4. Sequencer binary built with **`WalletApp::new(WalletConfig::sepolia())`** (see `sequencer-devnet` vs production binary choice in your release pipeline).

---

## Mainnet (production)

When the rollup runs on Ethereum mainnet, **reuse the same operator checklist above**. Differences are operational scale, not watchdog logic:

| Topic | Mainnet notes |
|-------|----------------|
| L1 RPC | Production-grade archive provider; rate limits matter for wide `getLogs` ranges |
| Contracts | Mainnet InputBox, application, portals from production deployment manifest |
| CM image | Build from production app/scheduler artifacts (mainnet wallet constants when defined in app-core) |
| Schedule cadence | A cron/timer interval of 300s+ is fine; finalized promotion follows mainnet safe head |
| Security | Stricter firewall between public ingress and internal snapshot tier; secrets management for RPC credentials |
| Bootstrap | Almost always ops-provided CM snapshot or continued state dir — not genesis replay |

There is no `just devnet-for-watchdog` or automated harness on mainnet; treat Sepolia compare success as the gate before mainnet go-live.

---

## Compare Cycle Behavior (All Live Chains)

Same on Sepolia and mainnet:

1. Load watchdog checkpoint from `head.json`.
2. `GET /finalized_state/inclusion_block` — if unchanged, **stop** (cheap).
3. If advanced: `eth_getLogs` on InputBox for `(last_block+1)..inclusion_block`.
4. Advance CM incrementally; `inspect` → SSZ bytes.
5. `GET /finalized_state` → SSZ bytes.
6. Raw compare; emit `watchdog_event` + non-zero exit on mismatch.
7. Write new CM checkpoint on success.

Details: [`README.md`](README.md), [`docs/snapshots/lifecycle.md`](../snapshots/lifecycle.md).

---

## Checkpoint disk usage and backups

Each successful promotion stores a full CM snapshot under
`$CARTESI_WATCHDOG_STATE_DIR/checkpoints/<block>/`, and the watchdog **keeps only
the selected one** — after the atomic `head.json` flip it deletes the
checkpoint it superseded (crash-safe: `head.json` always names a complete
checkpoint). Local disk therefore stays bounded at a single snapshot; no
operator cleanup is required.

For backups / rollback history, schedule the watchdog tick (it runs one cycle and
exits) and **after it exits** `aws s3 sync $CARTESI_WATCHDOG_STATE_DIR/checkpoints/
s3://…` (without `--delete`). Because the process has exited there is no race
with its store or prune, and omitting `--delete` **accumulates a per-block
history in S3** while local disk stays at one snapshot. Restore feeds a chosen
snapshot back through the watchdog/sequencer recovery workflow.

## Sequencer restart policy

The sequencer's exit codes are the restart contract (R4): 10
restart-expect-recovery, 20 restart-transient, 30 terminal — **page an
operator, do not auto-restart**, 40 wipe + `setup --recovery`, 1
unclassified restart-with-backoff. Operational notes:

- **The exit code is the whole restart contract.** There is no database
  gate and no acknowledgement command (removed 2026-08-19, review L2):
  standard recovery is automatic on every boot, and a persistent terminal
  fault re-detects fail-loud when the faulty state is next read. Configure
  the supervisor to honor 30 (stop and page) — that configuration is what
  bounds a crash loop.
- **A terminal containment that cannot drain within two seconds exits via
  `abort()` (SIGABRT, status 134), not code 30.** Treat 134 from the
  sequencer as terminal-class; the cause is in the logs and in the
  `terminal_faults` black box when the write got through.
- **After an unclean death (OOM, node reboot, SIGKILL) no action is
  needed**: the next start re-derives everything from facts. For
  postmortems, the `terminal_faults` table records every terminal cause
  (best-effort, append-only, traveling with the data directory —
  `SELECT * FROM terminal_faults ORDER BY fault_id DESC`); an unclean
  death that never reached containment leaves only the process logs.
- **Canonical divergence is the one manual path**: the sequencer freezes
  the acceptance frontier, refuses all commands, and the remedy is a
  fresh-directory `setup --recovery` (cockroach). You will typically learn
  of it from the watchdog before the sequencer tells you.

## Troubleshooting (live deployments)

| Symptom | Likely cause |
|---------|----------------|
| `/finalized_state` missing on public URL | Wrong tier — use internal `CARTESI_WATCHDOG_SEQUENCER_URL` |
| `failed to load watchdog head` / missing `head.json` | Uninitialized or wiped `STATE_DIR` — see [Missing or corrupt head.json](#missing-or-corrupt-headjson-tick-exit-1) |
| `state_mismatch` | CM image / wallet constants ≠ sequencer build; or wrong bootstrap block |
| `inclusion_block_regressed` | Stale watchdog state vs sequencer finalized head |
| Slow or failing `getLogs` | RPC range limits — watchdog uses same partition strategy as sequencer |
| Transient `L1 RPC latest head lags target block` | Fallback RPC is behind the sequencer's finalized inclusion block; watchdog retries until the node has indexed through the target (avoids truncated `eth_getLogs` false mismatches) |
| `inspect endpoint not implemented` | Rebuild CM image for the correct chain target |
| Works on Sepolia, fails on mainnet | Different deployment addresses or different guest build — do not reuse Sepolia env verbatim |

---

## Related

- **Local dev / CI:** [`getting-started.md`](getting-started.md)
- **Architecture:** [`README.md`](README.md)
- **Webhooks:** [`staging-drills.md`](staging-drills.md)
