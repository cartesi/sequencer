# Watchdog Staging Drills

Operator drills for divergence detection and watchdog tick verification.

- **Sepolia / mainnet:** [`operator-deployment.md`](operator-deployment.md). **Local dev:** [`getting-started.md`](getting-started.md).
- Module map and local test recipes: [`README.md`](README.md).

This document covers staging and manual verification beyond the devnet tutorial.

## Prerequisites

- **Release bundle (staging/production):** deploy `sequencer-watchdog-vX` and `canonical-machine-image-*-vX` from the same git tag; toolchain pins live in [`toolchain-pins.env`](../../toolchain-pins.env).
- Built canonical machine image: `just canonical-build-machine-image`
- `cartesi-machine`, `lua`, and `curl` on PATH
- `just watchdog-lua-deps` — builds `lcurl.so` into `.deps/lua` (libcurl + Lua headers on host)
- JSON is pure Lua (`watchdog/third_party/json.lua`); no cjson compile step
- Staging or local sequencer reachable at `WATCHDOG_SEQUENCER_URL`
- L1 RPC + InputBox + app addresses matching that deployment
- Log collection for `watchdog_event` lines and process exit codes

## Drill 1 — Divergence signal (synthetic mismatch, no CM)

Verifies the production `main.lua` divergence path (`watchdog_event` + exit code `2`) with
injected fake deps — no sequencer required.

```bash
just watchdog-lua-deps
WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/tests/drill_divergence.lua   # exits 2
# or: just test-watchdog-divergence-drill   # wraps the drill; recipe exits 0
```

Expected: `main.lua` emits a structured `watchdog_event` with `kind=state_mismatch` and
non-zero `mismatch_offset`, then the drill process exits with code `2`.

Unit coverage: `just test-watchdog` (`runner returns state mismatch payload`).

## Drill 2 — Happy compare (local Anvil harness)

Full stack: Anvil + devnet rollups + sequencer + CM inspect + `GET /finalized_state`.

```bash
just test-watchdog-compare-harness
# equivalent:
# just setup && just watchdog-lua-deps && just ensure-machine-image
# cargo build -p sequencer --bin sequencer-devnet -p rollups-e2e
# cargo run -p rollups-e2e --bin rollups-e2e -- watchdog_genesis_compare_test --exact
```

Or run the Lua compare pass manually after starting a devnet sequencer yourself:

```bash
export WATCHDOG_SEQUENCER_URL=http://127.0.0.1:<port>
export WATCHDOG_L1_RPC_URL=http://127.0.0.1:8545
export WATCHDOG_INPUTBOX_ADDRESS=<from Anvil deployments>
export WATCHDOG_APP_ADDRESS=<deployed app>
export WATCHDOG_STATE_DIR=/tmp/watchdog-state
export WATCHDOG_CM_SNAPSHOT_DIR=examples/canonical-app/out/canonical-machine-image
export WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK=0
export WATCHDOG_LUA_ROOT="$(pwd)"
export WATCHDOG_LUA_BIN=lua
export WATCHDOG_LUA_DEPS=.deps/lua
./watchdog/sequencer-watchdog init
./watchdog/sequencer-watchdog tick
```

Expected: exit **0**; the tick may exit idle if the finalized block is unchanged.
The harness path also proves byte-identical **devnet** genesis SSZ on sequencer `/finalized_state` and CM inspect
(same bytes as `wallet_snapshot::encode(WalletConfig::devnet())`; the `.hex` fixture
is for Sepolia `default()` — do not use it as the devnet golden).

## Drill 3 — Production compare (scheduled)

Run the watchdog against staging. Each tick runs one cycle and exits; schedule re-runs
with a systemd timer / cron and alert on the exit code. `sequencer-watchdog`
takes a non-blocking `flock`; production scheduling should also prevent
overlapping ticks with systemd, Kubernetes, or an equivalent scheduler guard:

```bash
# ... all WATCHDOG_* vars from config.lua ...
sequencer-watchdog tick
```

Exit codes from `sequencer-watchdog tick`:

| Code | Meaning |
|------|---------|
| `0` | Compare cycle completed — clean, or idle when finalized is unchanged |
| `1` | Transient error after retries (RPC, CM, network) |
| `2` | Deterministic divergence — `watchdog_event` on stderr with `{kind, previous_safe_block, sequencer_inclusion_block, mismatch_offset?}` |

## Triage checklist

| Symptom | Likely cause |
|---------|----------------|
| `inspect endpoint not implemented` | Stale CM image — rebuild |
| `state_mismatch` at genesis | Checkpoint not aligned with sequencer history |
| Compare skipped in Lua e2e | Set `WATCHDOG_E2E_SEQUENCER_URL` to a live sequencer |
| CM inspect 27 bytes / harness byte mismatch | Rebuild devnet image: `just canonical-build-machine-image` — see [`README.md`](README.md#troubleshooting-just-test-watchdog-compare-harness) |
