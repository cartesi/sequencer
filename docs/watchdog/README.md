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

# Path B — two terminals: stack prints WATCHDOG_* exports, then run compare:
just devnet-for-watchdog          # terminal 1 — leave running
# terminal 2: paste exports, then:
WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/main.lua
```

Details: **[`getting-started.md`](getting-started.md)**.

## Host dependencies (`watchdog-lua-deps`)

Compare mode and any test that hits HTTP need a native **`lcurl.so`** built into `.deps/lua/`. JSON is pure Lua (no compile step).

```bash
just watchdog-lua-deps    # idempotent; writes .deps/lua/lcurl.so
export WATCHDOG_LUA_DEPS="$(pwd)/.deps/lua"
```

You also need **`cartesi-machine`** on `PATH` (in-process `cartesi` Lua module) and **`lua`** (5.4 recommended).

### System packages

| OS | Packages |
|----|----------|
| Debian / Ubuntu / WSL | `libcurl4-openssl-dev` `liblua5.4-dev` `lua5.4` `build-essential` |
| Fedora | `libcurl-devel` `lua-devel` |
| Arch | `curl` `lua` |

Verify before building:

```bash
pkg-config --exists libcurl && echo "libcurl ok"
test -f /usr/include/lua5.4/lua.h && echo "lua headers ok"
```

On Debian/Ubuntu, Lua headers live under **`/usr/include/lua5.4/`**, not `/usr/include/`. The repo script passes `LUA_INC` accordingly when invoking the vendored lua-cURL Makefile (`scripts/watchdog-lua-deps.sh`).

### Troubleshooting `just watchdog-lua-deps`

| Message / error | Fix |
|-----------------|-----|
| `install libcurl dev package` | `sudo apt-get install -y libcurl4-openssl-dev` (or distro equivalent), then rerun `just watchdog-lua-deps` |
| `install Lua headers` | `sudo apt-get install -y liblua5.4-dev` |
| `fatal error: lua.h: No such file or directory` | Install `liblua5.4-dev`. If headers are present but build still fails, ensure you are on a tree where `scripts/watchdog-lua-deps.sh` passes **`LUA_INC`** (not `LUA_INCLUDE_DIR`) to make — see script in repo |
| `built lcurl.so but lua cannot load it` | Lua version mismatch: build with the same `lua` you run (`lua -v` vs headers under `lua5.4`) |
| `need curl or wget` | Fetch tool to download pinned lua-cURL sources into `watchdog/third_party/lua-curl/` |

CI runs **`just test-watchdog`** (mocked HTTP) and the Rust watchdog compare harness (`watchdog_genesis_compare_test`) in the rollups-e2e job. Full local smoke remains available via `just test-watchdog-compare-harness`.

## V1 Shape

The implementation lives in `watchdog/` and is intentionally split into small
Lua modules:

- `http.lua`: HTTP adapter via in-tree **lua-cURLv3** / `lcurl` (`just watchdog-lua-deps`).
- `json.lua` / `third_party/json.lua`: pure-Lua JSON (RPC + structured watchdog events).
- `jsonrpc.lua`: JSON-RPC request/response validation.
- `l1_reader.lua`: partitioned `eth_getLogs` scanning and strict L1 log ordering.
- `abi.lua`: decoding for the `InputAdded` / `EvmAdvance` envelope.
- `machine_runner.lua`: CM driver (`load`, `advance`, `inspect`, `dump`).
- `machine_cartesi.lua`: in-process `cartesi` Lua module binding (production path).
- `machine_cli.lua`: CLI adapter (`cartesi-machine` subprocess). Used by the Rust
  compare harness (`run_compare_once.lua`) and Lua CM e2e; `main.lua` uses
  `machine_cartesi` in production.
- `sequencer_reader.lua`: sequencer HTTP client (`GET /finalized_state/inclusion_block`, `GET /finalized_state`).
- `compare.lua`: raw byte comparison.
- `checkpoint.lua`: manifest-backed checkpoint persistence.
- `retry.lua`: bounded retry helper used by the runtime.
- `runner.lua`: one-shot orchestration — cheap `/finalized_state/inclusion_block`
  poll, optional full pass (L1 fetch, CM replay, SSZ compare, checkpoint write).
- `main.lua`: compare or advance loop (daemon or `WATCHDOG_ONCE=1`).

The L1 reader follows the Rust partition strategy from
`sequencer/src/partition.rs`: if an RPC provider rejects a large range, the
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

**Idle optimization (compare mode):** when `inclusion_block` has not advanced past the
checkpoint's `safe_block` (the last verified inclusion block), the runner returns
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
checkpoint_dir/
  current.json
  checkpoints/
    00000000000001234567/
      snapshot/
      manifest.json
```

`manifest.json` records `safe_block` (the L1 reference block the CM snapshot
covers — in compare mode this is the finalized `inclusion_block`), timestamp,
and optionally the CM image hash. A new checkpoint directory is written first,
then `current.json` is atomically replaced to point at it.

When bootstrapping without an existing checkpoint, the operator provides both:

- `WATCHDOG_CM_SNAPSHOT_DIR`
- `WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK`

## Modes

The default `WATCHDOG_MODE` is `advance`. In this mode the watchdog does not
poll the sequencer. It:

1. Loads the latest checkpoint, or the bootstrap snapshot directory.
2. Reads the L1 safe block from the RPC (or `WATCHDOG_TARGET_SAFE_BLOCK` when
   provided for tests/manual runs).
3. Fetches and decodes `InputAdded` logs for the block range.
4. Feeds the raw InputBox input bytes into the CM adapter.
5. Saves a new snapshot directory and advances `current.json`.

`WATCHDOG_MODE=compare` polls `/finalized_state/inclusion_block` first; when the
block advances, replays L1 inputs into the CM, inspects with query `state`, and
compares the SSZ report bytes against `GET /finalized_state`.

Useful runtime knobs:

- `WATCHDOG_CM_EXECUTABLE` / `WATCHDOG_CM_WORK_DIR`: used only by `machine_cli.lua`
  tests; production `main.lua` uses the in-process `cartesi` Lua module.
- `WATCHDOG_RETRY_ATTEMPTS`: bounded retry attempts per run, default `3`.
- `WATCHDOG_RETRY_DELAY_SEC`: delay between retry attempts, default `5`.
- `WATCHDOG_TARGET_SAFE_BLOCK`: manual/test override for the target safe block.

## Local Tests

| Command | What it exercises |
|---------|-------------------|
| `just test-watchdog` | Lua unit tests (fake HTTP/RPC/CM; no live chain) |
| `just test-watchdog-e2e` | Real CM: advance, inspect; optional live compare if `WATCHDOG_E2E_SEQUENCER_URL` set |
| `just test-watchdog-compare-harness` | **Full E2E**: Anvil + devnet sequencer + `/finalized_state` + CM inspect + Lua compare (genesis) |
| `just test-rollups-e2e` | All rollups e2e scenarios; includes `deposit_transfer_withdrawal_test` (wallet workload + **non-genesis** watchdog compare) and `watchdog_genesis_compare_test` |
| `just test-watchdog-divergence-drill` | Synthetic divergence signal drill (`watchdog_event` + exit `2`) |

Prerequisites for CM-backed tests: see **[Host dependencies](#host-dependencies-watchdog-lua-deps)** above, then:

```bash
just canonical-build-machine-image   # once, if out/ image is missing
just watchdog-lua-deps
export WATCHDOG_LUA_DEPS="$(pwd)/.deps/lua"
```

### Lua unit tests

```bash
just test-watchdog
```

Covers raw comparison, golden InputAdded ABI decoding, L1 ordering, recursive
range partitioning, config, checkpoints, advance/compare runner (fakes), CM CLI
staging, and retry behavior.

### Lua CM end-to-end

```bash
just test-watchdog-e2e
```

Scenarios (verbose `step NN/NN` logging):

- `prerequisites` — `cartesi-machine` on PATH and machine image present.
- `advance-empty-range` — real CM advance + checkpoint write with zero new inputs.
- `cm-inspect-state-query` — real `--cmio-inspect-state` with query `state`.
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
`watchdog/tests/run_compare_once.lua` (CLI `machine_cli` binding) in compare mode.
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

See [`staging-drills.md`](staging-drills.md) for divergence signal and compare-mode drills.

## Related sequencer tests

```bash
cargo test -p sequencer snapshot_endpoints -- --test-threads=1
cargo test -p app-core wallet_snapshot -- --test-threads=1
```

HTTP integration for snapshot routes lives in `sequencer/tests/snapshot_endpoints.rs`.
SSZ golden bytes for the toy wallet live in `tests/fixtures/wallet_snapshot_v1_empty.{hex,bin}`.
