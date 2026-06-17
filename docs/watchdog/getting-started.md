# Watchdog + sequencer: local development

Step-by-step guide for running the watchdog alongside a **local** `sequencer-devnet` stack (Anvil + ephemeral ports). Use this for CI smoke tests and debugging the watchdog itself.

**Running on Sepolia or mainnet?** That follows the same operator model as production — internal snapshot URL, live L1, persistent checkpoints, chain-specific CM image. See **[`operator-deployment.md`](operator-deployment.md)** (Sepolia is the usual dress rehearsal before mainnet).

- Architecture and module map: [`README.md`](README.md)
- **Sepolia / mainnet (production-like):** [`operator-deployment.md`](operator-deployment.md)
- Staging drills: [`staging-drills.md`](staging-drills.md)
- Implementation: [`watchdog/`](../../watchdog/) (Lua)

## Contents

1. [What you are running](#what-you-are-running)
2. [Prerequisites](#prerequisites)
3. [Path A — Full automated smoke](#path-a--full-automated-smoke-recommended-first)
4. [Path B — Interactive (two terminals)](#path-b--interactive-sequencer--watchdog-two-terminals)
5. [Production-like deployments](#production-like-deployments-sepolia--mainnet)
6. [Environment reference](#environment-reference-compare-mode)
7. [Troubleshooting](#troubleshooting)
8. [Related commands](#related-commands)

---

## What you are running

| Process | Role |
|---------|------|
| **Anvil** | Local L1 with Cartesi rollups contracts pre-deployed (`just setup`) |
| **sequencer-devnet** | Off-chain sequencer (wallet app, batches, snapshot promotion) |
| **watchdog** (`WATCHDOG_MODE=compare`) | Polls `/finalized_state/inclusion_block`, replays L1 inputs in CM, compares SSZ to `/finalized_state` |

The sequencer exposes (operator-internal, same HTTP listener today):

- `GET /finalized_state/inclusion_block` — cheap cursor poll
- `GET /finalized_state` — SSZ state file when compare runs

---

## Prerequisites

From the repo root:

1. **Rust** — `cargo` (edition 2024 workspace).

2. **Nix / direnv (recommended)** — Foundry `anvil`, Cartesi tools, and consistent Lua headers:

   ```bash
   eval "$(direnv export bash 2>/dev/null)"
   ```

   Without direnv you need on `PATH`: `anvil`, `lua`, `cartesi-machine`, and a C compiler for `lcurl`.

3. **System packages for watchdog HTTP** — see [`README.md` — Host dependencies](README.md#host-dependencies-watchdog-lua-deps) (Debian/WSL: `libcurl4-openssl-dev`, `liblua5.4-dev`, `lua5.4`, then `just watchdog-lua-deps`).

4. **Cartesi Machine** — `cartesi-machine` on `PATH` so the in-process `cartesi` Lua module loads (ships with Cartesi Machine install / nix shell).

5. **One-time repo setup**:

   ```bash
   just setup                      # Anvil state + contract artifacts
   just canonical-build-machine-image   # CM image (~minutes, needs cross toolchain)
   just watchdog-lua-deps          # builds .deps/lua/lcurl.so
   just doctor                     # lua + cartesi + lcurl + machine_cartesi load probe
   ```

6. **Unit smoke (optional)**:

   ```bash
   just test-watchdog
   ```

---

## Path A — Full automated smoke (recommended first)

Proves Anvil + devnet sequencer + CM inspect + Lua compare in one command:

```bash
just test-watchdog-compare-harness
```

This builds `sequencer-devnet`, spawns the stack, waits for `GET /finalized_state`, compares genesis **devnet** SSZ to the CM inspect bytes, and runs one Lua compare pass. Expect exit code 0.

**First time or after scheduler/SSZ changes:** run `just watchdog-lua-deps` and `just canonical-build-machine-image` before the harness (see [compare harness troubleshooting](README.md#troubleshooting-just-test-watchdog-compare-harness)).

---

## Path B — Interactive: sequencer + watchdog (two terminals)

### Terminal 1 — Devnet stack (Anvil + sequencer)

```bash
just devnet-for-watchdog
```

This starts Anvil and `sequencer-devnet` on **ephemeral local ports** (not fixed 8545/3000) and prints a block of `export WATCHDOG_*=...` lines. **Copy those exports** into Terminal 2.

Leave Terminal 1 running until you are done; Ctrl+C stops Anvil and the sequencer.

### Wait for finalized snapshot

Compare mode needs a **finalized** SSZ dump. Right after boot, the cheap endpoint may return **404** until the sequencer has promoted a snapshot.

In another shell (use the printed `WATCHDOG_SEQUENCER_URL`):

```bash
curl -s "$WATCHDOG_SEQUENCER_URL/finalized_state/inclusion_block"
```

When you see JSON like `{"inclusion_block":0,"l2_tx_index":0}` (numbers may differ), the watchdog can compare. If it stays 404 for a long time, check sequencer logs in `tests/e2e/results/` and that L1 is mining (devnet Anvil auto-mines by default).

Optional — inspect SSZ size:

```bash
curl -s -D - "$WATCHDOG_SEQUENCER_URL/finalized_state" -o /tmp/finalized-state.bin
head -c 32 /tmp/finalized-state.bin | xxd
```

### Terminal 2 — Watchdog (compare mode)

From repo root, after `just watchdog-lua-deps`:

```bash
# Paste exports from Terminal 1, then:
export WATCHDOG_MODE=compare
export WATCHDOG_ONCE=1          # single pass; omit for daemon (poll every 30s)

WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/main.lua
```

Success: exit **0**, stderr shows `watchdog_step` lines ending in `compare pass complete`.

Exit codes from `watchdog/main.lua`: **0** clean, **1** transient failure (RPC/CM/network after retries), **2** deterministic divergence (`watchdog_event` emitted on stderr before exit).

Daemon mode (production-like loop):

```bash
export WATCHDOG_POLL_INTERVAL_SEC=30
unset WATCHDOG_ONCE
WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/main.lua
```

When `inclusion_block` has not advanced since the last verified checkpoint, the runner **skips** L1/CM work (idle-cheap).

### One-shot compare (same stack, no daemon)

```bash
WATCHDOG_ONCE=1 WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/main.lua
```

---

## Production-like deployments (Sepolia / mainnet)

Local paths A–B do **not** apply to public L1. There is no `just devnet-for-watchdog` on Sepolia or mainnet.

| Local devnet | Sepolia / mainnet |
|--------------|-------------------|
| You spawn Anvil + `sequencer-devnet` | Sequencer already run by ops |
| `canonical-machine-image` (devnet guest) | `canonical-machine-image-sepolia` (today); mainnet guest when released |
| Snapshot HTTP on localhost | **Internal** operator network only |
| Genesis bootstrap (`safe_block=0`) usual | Bootstrap must match **current** finalized `inclusion_block` |

**Sepolia is the dress rehearsal for mainnet** — same checklist, alarms, checkpoint volume, and firewall rules; only chain IDs, RPC URLs, and contract addresses change.

Full operator runbook: **[`operator-deployment.md`](operator-deployment.md)**.

---

## Environment reference (compare mode)

| Variable | Required | Description |
|----------|----------|-------------|
| `WATCHDOG_MODE` | yes | `compare` |
| `WATCHDOG_SEQUENCER_URL` | yes | e.g. `http://127.0.0.1:54321` |
| `WATCHDOG_L1_RPC_URL` | yes | L1 JSON-RPC |
| `WATCHDOG_INPUTBOX_ADDRESS` | yes | InputBox contract |
| `WATCHDOG_APP_ADDRESS` | yes | Rollup application contract |
| `WATCHDOG_CHECKPOINT_DIR` | yes | CM checkpoint storage |
| `WATCHDOG_CM_SNAPSHOT_DIR` | first run | Genesis CM image dir if no checkpoint yet |
| `WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK` | with above | Usually `0` on fresh devnet |
| `WATCHDOG_LUA_DEPS` | yes | `.deps/lua` after `just watchdog-lua-deps` |
| `WATCHDOG_ONCE` | no | `1` = one pass then exit |
| `WATCHDOG_POLL_INTERVAL_SEC` | no | Default `30` (daemon) |

See `watchdog/config.lua` for the full list.

---

## Troubleshooting

| Symptom | What to check |
|---------|----------------|
| `install libcurl dev package` | Install `libcurl4-openssl-dev` (or distro equivalent); see [Host dependencies](README.md#host-dependencies-watchdog-lua-deps) |
| `lua.h: No such file or directory` when building lcurl | Install `liblua5.4-dev` (Debian/WSL), or set `LUA_INC` to your Lua headers directory (Homebrew/nix) before `just watchdog-lua-deps` |
| `lcurl` / `cURL` not found at runtime | Run `just watchdog-lua-deps`, set `WATCHDOG_LUA_DEPS=.deps/lua` |
| `cartesi Lua module is required` | Install Cartesi Machine; use nix/direnv shell; ensure `cartesi-machine` on `PATH` |
| `inspect endpoint not implemented` | Rebuild CM image: `just canonical-build-machine-image` |
| CM inspect ~27 bytes / JSON in error | Stale image (old JSON inspect); rebuild: `just canonical-build-machine-image` |
| HTTP 404 on `/finalized_state/inclusion_block` | Sequencer not promoted yet; wait or drive L1 + batches |
| `state_mismatch` at genesis | Wrong `WATCHDOG_CM_SNAPSHOT_*` or stale CM image vs sequencer build |
| `inclusion_block_regressed` | Checkpoint ahead of sequencer (reset checkpoint dir or fix bootstrap block) |
| `could not determine which binary to run` | Use `just test-watchdog-compare-harness` (not bare `cargo run -p rollups-e2e`) |
| Harness `87 vs 76` or `27 vs 76` byte mismatch | Stale CM image and/or wrong fixture; see [harness troubleshooting](README.md#troubleshooting-just-test-watchdog-compare-harness) |

Full harness failure table: **[`README.md` — Troubleshooting compare harness](README.md#troubleshooting-just-test-watchdog-compare-harness)**.

---

## Related commands

```bash
just doctor                           # toolchain sanity before CM-backed tests
just test-watchdog                    # Lua unit tests (no live chain)
just test-watchdog-e2e                # CM advance/inspect (optional live sequencer URL)
just test-watchdog-compare-harness    # Full stack smoke
cargo test -p sequencer --test snapshot_endpoints
cargo test -p app-core wallet_snapshot
```
