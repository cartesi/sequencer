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

- [ ] Watchdog host can reach **internal** `WATCHDOG_SEQUENCER_URL` (not only the public `/tx` URL).
- [ ] Watchdog host can reach **L1 JSON-RPC** with `eth_getLogs` (archive recommended if replaying long history).
- [ ] `/finalized_state` is **not** exposed on the public internet.

Verify snapshot API before CM bootstrap:

```bash
curl -sS -o /dev/null -w "%{http_code}\n" "$WATCHDOG_SEQUENCER_URL/finalized_state/inclusion_block"
# expect 200 when a finalized snapshot exists (404 = not promoted yet or wrong host)
```

### 2. Watchdog runtime (release bundle or local build)

**Production (recommended):** use the **release bundle** for tag `vX` — same git tag as the sequencer binary and `canonical-machine-image-*-vX.tar.gz`. Load `sequencer-watchdog-vX-linux-<arch>.tar.gz` (`docker load`), verify alignment via `release-manifest-vX.json` and `/opt/watchdog/RELEASE.json` inside the image. See [`release/README.md`](../../release/README.md).

`cartesi-machine` in the watchdog image **must** match `CARTESI_MACHINE_VERSION` in `release/versions.env` (the emulator that built the CM image tarball). Mismatch causes load failures or false `state_mismatch`.

**Local / dev build:**

```bash
eval "$(direnv export bash 2>/dev/null)"
just watchdog-lua-deps    # .deps/lua/lcurl.so — needs libcurl + Lua dev headers; see README
```

Host packages and build errors: [`README.md` — Host dependencies](README.md#host-dependencies-watchdog-lua-deps).

Requires: `lua`, `cartesi-machine` (in-process `cartesi` Lua module), libcurl + Lua headers. Pin `cartesi-machine` to the same version as your CM bootstrap tarball.

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
| `WATCHDOG_MODE` | `compare` |
| `WATCHDOG_SEQUENCER_URL` | Ops: internal HTTP base (see network diagram) |
| `WATCHDOG_L1_RPC_URL` | Ops: chain RPC (archive for historical `getLogs`) |
| `WATCHDOG_APP_ADDRESS` | This rollup’s Cartesi **application** contract |
| `WATCHDOG_INPUTBOX_ADDRESS` | InputBox on that L1 ([Cartesi deployed contracts](https://docs.cartesi.io/cartesi-rollups/2.0/deployment/self-hosted.md)) |
| `WATCHDOG_CHECKPOINT_DIR` | Persistent volume on watchdog host |
| `WATCHDOG_CM_SNAPSHOT_DIR` | Bootstrap CM snapshot (first run only) |
| `WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK` | L1 block that bootstrap snapshot represents (= finalized `inclusion_block` at bootstrap) |
| `WATCHDOG_LUA_DEPS` | `.deps/lua` |
| `WATCHDOG_POLL_INTERVAL_SEC` | `120`–`300` on public L1 (finalized advances slowly) |

The sequencer discovers and pins `input_box_address` at startup; use the same values as `SEQ_ETH_RPC_URL` / `SEQ_APP_ADDRESS` configuration.

### 5. Bootstrap checkpoint (first run on a live chain)

On a long-lived deployment, **`WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK=0` is usually wrong** unless finalized state is still at genesis.

Pick one:

1. **Ops hands off** a CM snapshot directory + block number matching current finalized `inclusion_block`, or
2. **Watchdog reuses** `WATCHDOG_CHECKPOINT_DIR` from a prior successful compare on this deployment, or
3. **Replay from genesis** (only for new rollups / low block height — slow).

After bootstrap, the watchdog advances its own checkpoint each successful compare.

### 6. Run compare

One shot (smoke):

```bash
export WATCHDOG_ONCE=1
lua watchdog/main.lua
```

Daemon (staging / production):

```bash
unset WATCHDOG_ONCE
lua watchdog/main.lua
```

When `inclusion_block` ≤ last verified checkpoint, the runner only hits `/finalized_state/inclusion_block` and skips L1/CM work.

---

## Sepolia (testnet staging)

Use Sepolia to validate **the same procedure** you will run on mainnet: internal URLs, alarms, checkpoint persistence, RPC limits, CM image version pinning.

### Sepolia-specific values

| Item | Typical source |
|------|----------------|
| Chain ID | `11155111` |
| Public user ingress (tx demos only) | e.g. `https://eth-sepolia.rollups.cartesi.io/v2` — **may not** serve `/finalized_state` |
| Application instance | Per deployment (confirm with ops; demos have used `0x4CE633CA71071818cD73187765ee60F696dae083`) |
| InputBox (rollups v2.x on Sepolia) | Confirm on [deployed contracts](https://docs.cartesi.io/cartesi-rollups/2.0/deployment/self-hosted.md) (community examples use `0x58Df21fE097d4bE5dCf61e01d9ea3f6B81c2E1dB`) |
| CM image | `just canonical-build-machine-image-sepolia` |
| Tx / deposit demos | `tests/scripts/demo_sepolia.py` (copy `.env` locally; never commit secrets) |

### Example env block (fill from ops)

```bash
export WATCHDOG_MODE=compare
export WATCHDOG_SEQUENCER_URL="https://<internal-sepolia-sequencer>"
export WATCHDOG_L1_RPC_URL="https://<sepolia-archive-rpc>"
export WATCHDOG_APP_ADDRESS="0x..."
export WATCHDOG_INPUTBOX_ADDRESS="0x..."
export WATCHDOG_CHECKPOINT_DIR="/var/lib/watchdog/checkpoints-sepolia"
export WATCHDOG_CM_SNAPSHOT_DIR="/path/to/canonical-machine-image-sepolia"
export WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK="<finalized inclusion_block at bootstrap>"
export WATCHDOG_LUA_DEPS="/path/to/sequencer/.deps/lua"
export WATCHDOG_POLL_INTERVAL_SEC=120
```

### Operating the Sepolia sequencer

If your team runs the sequencer on Sepolia (not only the public endpoint):

1. `sequencer` / release binary with Sepolia `SEQ_*` (chain id, app address, batch submitter key, L1 RPC).
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
| `WATCHDOG_POLL_INTERVAL_SEC` | Often 300+; finalized promotion follows mainnet safe head |
| Security | Stricter firewall between public ingress and internal snapshot tier; secrets management for RPC credentials |
| Bootstrap | Almost always ops-provided CM snapshot or continued checkpoint dir — not genesis replay |

There is no `just devnet-for-watchdog` or automated harness on mainnet; treat Sepolia compare success as the gate before mainnet go-live.

---

## Compare mode behavior (all live chains)

Same on Sepolia and mainnet:

1. Load watchdog checkpoint (or bootstrap CM snapshot).
2. `GET /finalized_state/inclusion_block` — if unchanged, **stop** (cheap).
3. If advanced: `eth_getLogs` on InputBox for `(last_block+1)..inclusion_block`.
4. Advance CM incrementally; `inspect` → SSZ bytes.
5. `GET /finalized_state` → SSZ bytes.
6. Raw compare; emit `watchdog_event` + non-zero exit on mismatch.
7. Write new CM checkpoint on success.

Details: [`README.md`](README.md), [`docs/snapshots/lifecycle.md`](../snapshots/lifecycle.md).

---

## Troubleshooting (live deployments)

| Symptom | Likely cause |
|---------|----------------|
| `/finalized_state` missing on public URL | Wrong tier — use internal `WATCHDOG_SEQUENCER_URL` |
| `state_mismatch` | CM image / wallet constants ≠ sequencer build; or wrong bootstrap block |
| `inclusion_block_regressed` | Stale checkpoint dir vs sequencer finalized head |
| Slow or failing `getLogs` | RPC range limits — watchdog uses same partition strategy as sequencer |
| `inspect endpoint not implemented` | Rebuild CM image for the correct chain target |
| Works on Sepolia, fails on mainnet | Different deployment addresses or different guest build — do not reuse Sepolia env verbatim |

---

## Related

- **Local dev / CI:** [`getting-started.md`](getting-started.md)
- **Architecture:** [`README.md`](README.md)
- **Webhooks:** [`staging-drills.md`](staging-drills.md)
- **Public Sepolia latency demos** (not watchdog): [`docs/live-demo.md`](../live-demo.md)
