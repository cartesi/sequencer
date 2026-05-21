# Watchdog

The watchdog is an off-chain safety process that compares sequencer API state
against state produced by the canonical Cartesi Machine at an L1 safe block.

## V1 Shape

The implementation lives in `watchdog/` and is intentionally split into small
Lua modules:

- `http.lua`: HTTP adapter (`lua-curl` / `lcurl` when installed, otherwise `curl` CLI via `new_auto()`).
- `jsonrpc.lua`: JSON-RPC request/response validation.
- `l1.lua`: partitioned `eth_getLogs` scanning and strict L1 log ordering.
- `abi.lua`: decoding for the `InputAdded` / `EvmAdvance` envelope.
- `machine.lua`: narrow adapter boundary for Cartesi Machine bindings.
- `machine_cli.lua`: `cartesi-machine` CLI adapter for loading snapshot
  directories, writing raw input files, advancing, inspecting, and saving snapshots.
- `compare.lua`: raw byte comparison.
- `checkpoint.lua`: manifest-backed checkpoint persistence.
- `alarm.lua`: webhook alarm delivery.
- `retry.lua`: bounded retry helper used by the runtime.
- `runner.lua`: one-shot orchestration across checkpoint load, sequencer poll,
  L1 fetch, CM replay, raw compare, alarm, and checkpoint write.
- `main.lua`: compare or advance loop (daemon or `WATCHDOG_ONCE=1`).

The L1 reader follows the Rust partition strategy from
`sequencer/src/partition.rs`: if an RPC provider rejects a large range, the
range is split recursively and retried. Lua decodes and validates input
envelopes, but it does not classify payload tags. Direct input vs batch
submission remains scheduler logic inside the canonical machine.

`l1.lua` has the `InputAdded(address,uint256,bytes)` event topic baked in and
filters logs by `topic0 = InputAdded` and `topic1 = app address`, matching the
Rust reader's app-filtered InputBox scan.

## Runtime Contract

The sequencer exposes `GET /get_state` for byte-exact state comparison. The
endpoint is generic over app state bytes, even though the toy wallet app
currently returns deterministic JSON:

```json
{
  "safe_block": 123,
  "state": "{\"balances\":{},\"nonces\":{}}"
}
```

`state` must be the exact bytes produced by the bare-metal app serializer
for the app state anchored at `safe_block`. The watchdog compares those raw
bytes with the bytes returned by CM inspect. It must not canonicalize both
values before deciding pass/fail.

`get_state` reconstructs a safe-only app state by replaying the persisted
scheduler-accepted safe batch prefix into a fresh app instance. It intentionally
excludes the current soft-confirmed Tip and any valid closed batches that have
not been accepted by the L1 scheduler view yet.

The canonical scheduler answers `RollupRequest::Inspect` with query `state` by
calling `Application::export_state()` (see `examples/canonical-app`).

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

`manifest.json` records `safe_block`, timestamp, and optionally the CM image
hash. A new checkpoint directory is written first, then `current.json` is
atomically replaced to point at it.

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

`WATCHDOG_MODE=compare` replays safe L1 inputs into the CM, calls
`--cmio-inspect-state` with the `state` query, and compares the returned report
bytes against `GET /get_state`.

Useful runtime knobs:

- `WATCHDOG_CM_EXECUTABLE`: Cartesi Machine executable, default `cartesi-machine`.
- `WATCHDOG_CM_WORK_DIR`: temporary directory for staged input files, default `/tmp`.
- `WATCHDOG_RETRY_ATTEMPTS`: bounded retry attempts per run, default `3`.
- `WATCHDOG_RETRY_DELAY_SEC`: delay between retry attempts, default `5`.
- `WATCHDOG_TARGET_SAFE_BLOCK`: manual/test override for the target safe block.

## Local Tests

| Command | What it exercises |
|---------|-------------------|
| `just test-watchdog` | Lua unit tests (fake HTTP/RPC/CM; no live chain) |
| `just test-watchdog-e2e` | Real CM: advance, inspect; optional live compare if `WATCHDOG_E2E_SEQUENCER_URL` set |
| `just test-watchdog-compare-harness` | **Full E2E**: Anvil + devnet sequencer + `GET /get_state` + CM inspect + Lua compare |
| `just test-watchdog-webhook-drill` | Webhook delivery smoke (`WATCHDOG_WEBHOOK_URL` required) |

Prerequisites for CM-backed tests:

```bash
just canonical-build-machine-image   # once, if out/ image is missing
just watchdog-lua-deps               # lua-cjson into .deps/lua (system pkg or gcc)
```

`cartesi-machine`, `lua`, and `curl` on PATH. `lua-curl` is optional (CLI fallback).

### Lua unit tests

```bash
just test-watchdog
```

Covers raw comparison, golden InputAdded ABI decoding, L1 ordering, recursive
range partitioning, config, checkpoints, advance/compare runner (fakes), CM CLI
staging, retry, and alarm webhook encoding.

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

Spawns Anvil + rollups devnet + `sequencer-devnet`, proves CM inspect JSON at
genesis, then runs `watchdog/tests/run_compare_once.lua` in compare mode with
matching `WATCHDOG_*` addresses. Requires `RUN_WATCHDOG_E2E=1` (set by the recipe).

### Staging / operator drills

See [`staging-drills.md`](staging-drills.md) for webhook smoke, synthetic
divergence POST, and manual compare env vars.

## Related sequencer tests

```bash
cargo test -p sequencer get_state -- --test-threads=1
```

HTTP integration for `GET /get_state` lives in `sequencer/tests/e2e_sequencer.rs`.
Storage/replay semantics are covered in `sequencer/src/egress/app_state.rs` unit tests.
