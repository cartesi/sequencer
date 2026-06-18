# Watchdog

The watchdog is an off-chain safety process that compares the sequencer's
**finalized SSZ state dump** against state produced by the canonical Cartesi
Machine at the same L1 inclusion block.

## Documentation

| Doc | Audience |
|-----|----------|
| **[`operator-deployment.md`](operator-deployment.md)** | **Production-like** — Sepolia and mainnet: internal snapshot API, live L1, checkpoints (Sepolia = mainnet dress rehearsal) |
| **[`getting-started.md`](getting-started.md)** | **Local dev only** — Anvil + `sequencer-devnet`, harness smoke, two-terminal flow |
| This file | Architecture, modules, runtime contract, checkpoints, test commands |
| [`staging-drills.md`](staging-drills.md) | Webhook smoke, synthetic alarms, staging compare daemon |
| [`sepolia.md`](sepolia.md) | Redirect → [`operator-deployment.md`](operator-deployment.md) |

### Quick start (pick your environment)

**Sepolia / mainnet (operator):** [`operator-deployment.md`](operator-deployment.md) — shared checklist, internal URL, Sepolia CM image, mainnet notes.

**Local devnet:**

One-time setup, then either a single automated check or an interactive run:

```bash
just setup && just canonical-build-machine-image && just watchdog-lua-deps

# Path A — full smoke (Anvil + sequencer + CM + compare), one command:
just test-watchdog-compare-harness

# Path B — two terminals: stack prints WATCHDOG_* exports, then init + tick:
just devnet-for-watchdog          # terminal 1 — leave running
# terminal 2: paste exports, then:
WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/main.lua init
WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/main.lua tick
```

The production container entrypoint wraps `init`/`tick` with an advisory
`flock` on `$WATCHDOG_STATE_DIR/run.lock`. Direct local `lua` runs are for
development; production schedulers must also prevent overlapping ticks
(`flock`, systemd, or Kubernetes `concurrencyPolicy: Forbid`).

Details: **[`getting-started.md`](getting-started.md)**.

## Host dependencies (`watchdog-lua-deps`)

The watchdog cycle and any test that hits HTTP need a native **`lcurl.so`** built into `.deps/lua/`. JSON is pure Lua (no compile step).

```bash
just watchdog-lua-deps    # idempotent; writes .deps/lua/lcurl.so
export WATCHDOG_LUA_DEPS="$(pwd)/.deps/lua"
```

You also need **`cartesi-machine`** on `PATH` (in-process `cartesi`
Lua module), **`lua`** (5.4 recommended), and a scheduler non-overlap
guard. The release Docker image uses Linux `flock`; Nix also provides the
same CLI on macOS/Linux via `nixpkgs#util-linux`:

```bash
nix shell nixpkgs#util-linux
```

### System packages

| OS | Packages |
|----|----------|
| Debian / Ubuntu / WSL | `libcurl4-openssl-dev` `liblua5.4-dev` `lua5.4` `build-essential` `util-linux` |
| Fedora | `libcurl-devel` `lua-devel` `util-linux` |
| Arch | `curl` `lua` `util-linux` |

Verify before building:

```bash
pkg-config --exists libcurl && echo "libcurl ok"
test -f /usr/include/lua5.4/lua.h && echo "lua headers ok"
```

On Debian/Ubuntu, Lua headers live under **`/usr/include/lua5.4/`**, not `/usr/include/`. lua-cURLv3 is **vendored in-tree** under `watchdog/third_party/lua-curl/src`; `scripts/watchdog-lua-deps.sh` compiles it locally (no build-time download), discovering the Lua headers via `pkg-config` (override with `LUA_INC`).

### Troubleshooting `just watchdog-lua-deps`

| Message / error | Fix |
|-----------------|-----|
| `install libcurl dev package` | `sudo apt-get install -y libcurl4-openssl-dev` (or distro equivalent), then rerun `just watchdog-lua-deps` |
| `install Lua headers` | `sudo apt-get install -y liblua5.4-dev` |
| `fatal error: lua.h: No such file or directory` | Install `liblua5.4-dev`. If headers are present but build still fails, ensure you are on a tree where `scripts/watchdog-lua-deps.sh` passes **`LUA_INC`** (not `LUA_INCLUDE_DIR`) to make — see script in repo |
| `built lcurl.so but lua cannot load it` | Lua version mismatch: build with the same `lua` you run (`lua -v` vs headers under `lua5.4`) |

CI runs **`just test-watchdog`** (mocked HTTP), the divergence drill script, and watchdog rollups-e2e trials (`watchdog_genesis_compare_test`, non-genesis compare inside `deposit_transfer_withdrawal_test`, `watchdog_non_genesis_divergence_test`) plus a **`watchdog-docker`** image smoke job. Run **`just doctor`** locally before CM-backed work. Full local smoke: `just test-watchdog-compare-harness`.

## V1 Shape

The implementation lives in `watchdog/` and is intentionally split into small
Lua modules:

- `http.lua`: HTTP adapter via **lua-cURLv3** / `lcurl`, vendored in-tree and compiled by `just watchdog-lua-deps` (no build-time download).
- `json.lua` / `third_party/json.lua`: pure-Lua JSON (RPC + structured watchdog events).
- `jsonrpc.lua`: JSON-RPC request/response validation.
- `l1_reader.lua`: partitioned `eth_getLogs` scanning, strict L1 log ordering,
  and chunk callbacks so each successful provider response can be consumed and
  discarded.
- `abi.lua`: decoding for the `InputAdded` / `EvmAdvance` envelope.
- `machine_runner.lua`: CM driver (`load`, `advance`, `inspect`, `dump`).
- `machine_cartesi.lua`: in-process `cartesi` Lua module binding (production path).
- `sequencer_reader.lua`: sequencer HTTP client (`GET /finalized_state/inclusion_block`, `GET /finalized_state`).
- `compare.lua`: raw byte comparison.
- `checkpoint.lua`: manifest-backed checkpoint persistence (`head.json` pointer).
- `state.lua`: persisted `config.json` and single-run state lock.
- `retry.lua`: bounded retry helper used by the runtime.
- `runner.lua`: one compare cycle — cheap `/finalized_state/inclusion_block`
  poll, then (when finalized advanced) L1 fetch, CM replay, SSZ compare,
  checkpoint write.
- `main.lua`: dispatches `init` and `tick`; `tick` exits `0`/`1`/`2`.

The L1 reader follows the Rust partition strategy from
`sequencer/src/l1/partition.rs`: if an RPC provider rejects a large range, the
range is split recursively and retried. Lua decodes and validates input
envelopes, but it does not classify payload tags. Direct input vs batch
submission remains scheduler logic inside the canonical machine.

`l1_reader.lua` has the `InputAdded(address,uint256,bytes)` event topic baked in and
filters logs by `topic0 = InputAdded` and `topic1 = app address`, matching the
Rust reader's app-filtered InputBox scan.

## Runtime Contract

The sequencer exposes operator-internal snapshot routes (see `sequencer/src/egress/api/snapshot.rs`):

- `GET /finalized_state/inclusion_block` — cheap JSON `{ inclusion_block, l2_tx_index }` polled every compare tick.
- `GET /finalized_state` — streams the finalized SSZ state file (`application/octet-stream`) with `X-Inclusion-Block` and `X-L2-Tx-Index` headers.

**Idle optimization:** when `inclusion_block` has not advanced past the watchdog
checkpoint's `safe_block`, the tick returns
immediately — no `/finalized_state` download, no L1 `eth_getLogs`, no CM load/advance/inspect.

The watchdog compares the finalized SSZ bytes with the bytes returned by CM
inspect. It must not canonicalize either side before deciding pass/fail.

For the toy wallet app, SSZ encoding lives in `examples/app-core/src/wallet_snapshot.rs`
and is shared by `WalletApp::create_dump`, `Application::canonical_snapshot_bytes`,
and the canonical scheduler's `Inspect` handler (`examples/canonical-app`).

## Checkpoints

V1 persists only the resulting Cartesi Machine checkpoint, not the fetched L1
inputs.

```text
state_dir/
  config.json
  head.json
  run.lock       # advisory lock handle; file existence is not lock state
  checkpoints/
    00000000000001234567/
      snapshot/
      manifest.json
```

`manifest.json` records `safe_block` (the L1 reference block the CM snapshot
covers — the finalized `inclusion_block`), timestamp,
and optionally the CM image hash. A new checkpoint directory is written first,
then `head.json` is atomically replaced to point at it.

`init` stores the operator-provided bootstrap CM snapshot into this layout. `tick`
requires both `config.json` and `head.json`; it never bootstraps from env.
`WATCHDOG_L1_RPC_URL` is intentionally read at tick time, not persisted in
`config.json`, so operators can rotate RPC endpoints without rewriting watchdog
state.

- `WATCHDOG_CM_SNAPSHOT_DIR`
- `WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK`

## How it runs

The watchdog has two subcommands:

```bash
lua watchdog/main.lua init   # one-time setup: writes config.json + head.json
lua watchdog/main.lua tick   # one compare cycle; schedule this
```

`tick` does one cycle per process, then exits — infra schedules re-runs
(systemd timer / k8s CronJob) and reacts to the exit code. There is no daemon
loop. The production Docker entrypoint takes a non-blocking `flock` for
`init`/`tick`; direct host scheduling should provide the same non-overlap
guarantee. Each tick:

1. Loads the watchdog checkpoint from `head.json`.
2. Polls `/finalized_state/inclusion_block`. If it has not advanced past a
   watchdog checkpoint, exits `0` (idle). Otherwise:
3. Streams and decodes `InputAdded` logs for the new block range.
4. Replays each successful L1 partition into the in-process Cartesi Machine,
   then inspects with query `state`.
5. Byte-compares the SSZ report against `GET /finalized_state`; on match writes a
   new checkpoint, on mismatch emits a `watchdog_event` and exits `2`.

Runtime knobs:

- `WATCHDOG_L1_RPC_URL`: current L1 JSON-RPC endpoint for tick.
- `WATCHDOG_RETRY_ATTEMPTS`: bounded retry attempts per run, default `3`.
- `WATCHDOG_RETRY_DELAY_SEC`: delay between retry attempts, default `5`.

## Local Tests

| Command | What it exercises |
|---------|-------------------|
| `just test-watchdog` | Lua unit tests (fake HTTP/RPC/CM; no live chain) |
| `just test-watchdog-e2e` | Real CM: advance, inspect; optional live compare if `WATCHDOG_E2E_SEQUENCER_URL` set |
| `just test-watchdog-compare-harness` | **Full E2E**: Anvil + devnet sequencer + `/finalized_state` + CM inspect + Lua `init`/`tick` |
| `just test-rollups-e2e` | All rollups e2e scenarios; includes watchdog genesis/non-genesis compare plus `watchdog_non_genesis_divergence_test` (needs Sepolia CM image) |
| `just test-watchdog-divergence-drill` | Synthetic divergence signal drill (`watchdog_event` + exit `2`) |
| `just doctor` | Toolchain sanity: lua, cartesi-machine, lcurl, devnet CM image loadable via `machine_cartesi` |

Prerequisites for CM-backed tests: see **[Host dependencies](#host-dependencies-watchdog-lua-deps)** above, then:

```bash
just doctor                          # fail fast before long harness runs
just canonical-build-machine-image   # once, if out/ image is missing
just canonical-build-machine-image-sepolia   # rollups-e2e divergence trial (auto-built by test-rollups-e2e)
just watchdog-lua-deps
export WATCHDOG_LUA_DEPS="$(pwd)/.deps/lua"
```

### Lua unit tests

```bash
just test-watchdog
```

Covers raw comparison, golden InputAdded ABI decoding, L1 ordering, recursive
range partitioning, streamed L1 chunks, config, checkpoints, the compare runner
(fakes), and retry behavior.

### Lua CM end-to-end

```bash
just test-watchdog-e2e
```

Scenarios (verbose `step NN/NN` logging):

- `prerequisites` — `cartesi-machine` on PATH and machine image present.
- `cm-inspect-state-query` — real `--cmio-inspect-state` with query `state`.
- `machine-cartesi-store-reload-advance` — store checkpoint snapshot, reload, advance again (in-process binding).
- `compare-runner-with-sequencer` — skipped unless `WATCHDOG_E2E_SEQUENCER_URL` is set.

Rebuild the machine image after changing the canonical scheduler/dapp. A stale
image makes `cm-inspect-state-query` skip with `inspect endpoint not implemented`.

### Rust compare harness (most complete integration test)

```bash
just test-watchdog-compare-harness
```

Spawns Anvil + rollups devnet + `sequencer-devnet`, proves CM inspect SSZ at
genesis matches `wallet_snapshot::encode(WalletConfig::devnet())` (same as
`tests/fixtures/wallet_snapshot_v1_empty.hex` only for Sepolia `default()`), then runs
`watchdog/main.lua init` and `watchdog/main.lua tick`.
When `inclusion_block` is unchanged at genesis, the runner skips L1/CM work (idle-cheap);
`deposit_transfer_withdrawal_test` drives a gold batch first so compare replays real L1 inputs.
**Before first run (or after changing scheduler / SSZ / inspect code):**

```bash
just watchdog-lua-deps
just canonical-build-machine-image   # not only ensure-machine-image — rebuild when the guest changed
just test-watchdog-compare-harness
```

`ensure-machine-image` only checks that `examples/canonical-app/out/canonical-machine-image`
exists; it does **not** detect a stale guest. If you pulled SSZ/inspect changes, rebuild the image.

### Troubleshooting `just test-watchdog-compare-harness`

| Symptom | Likely cause | Fix |
|---------|----------------|-----|
| `install libcurl dev package` / `lua.h: No such file` | Missing host deps for `lcurl.so` | [Host dependencies](#host-dependencies-watchdog-lua-deps) |
| `could not determine which binary to run` | `rollups-e2e` crate has two bins | Use the just recipe, or `cargo run -p rollups-e2e --bin rollups-e2e -- …` |
| `invalid utf-8` / timeout on step 1 (older trees) | Harness treated SSZ body as UTF-8 | Update `tests/e2e/src/watchdog_compare.rs` (current tree decodes binary + chunked bodies) |
| `finalized_state bytes mismatch (len 87 vs expected 76)` | Wrong golden (Sepolia fixture vs devnet sequencer) and/or raw HTTP chunked framing | Harness expects **devnet** SSZ; `lcurl` decodes chunked responses automatically |
| `CM inspect bytes mismatch (len 27 vs expected 76)` | **Stale CM image** still returns JSON `{"balances":{},"nonces":{}}` from pre-SSZ inspect | `just canonical-build-machine-image` then rerun harness |
| `inspect endpoint not implemented` | Older guest without inspect handler | Same rebuild as above |
| Harness passes step 1–2 but Lua compare fails | `WATCHDOG_LUA_DEPS` or checkpoint/bootstrap | Set `export WATCHDOG_LUA_DEPS="$(pwd)/.deps/lua"`; see [`getting-started.md`](getting-started.md) env table |

Manual equivalent of the recipe:

```bash
cargo run -p rollups-e2e --bin rollups-e2e -- \
  watchdog_genesis_compare_test --exact --nocapture
```

### Staging / operator drills

See [`staging-drills.md`](staging-drills.md) for divergence signal and watchdog tick drills.

## Related sequencer tests

```bash
cargo test -p sequencer snapshot_endpoints -- --test-threads=1
cargo test -p app-core wallet_snapshot -- --test-threads=1
```

HTTP integration for snapshot routes lives in `sequencer/tests/snapshot_endpoints.rs`.
SSZ golden bytes for the toy wallet live in `tests/fixtures/wallet_snapshot_v1_empty.{hex,bin}`.
