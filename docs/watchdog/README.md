# Watchdog

The watchdog is an off-chain safety process that compares sequencer API state
against state produced by the canonical Cartesi Machine at an L1 safe block.

## V1 Shape

The implementation lives in `watchdog/` and is intentionally split into small
Lua modules:

- `http.lua`: HTTP adapter, currently `lua-curl` oriented.
- `jsonrpc.lua`: JSON-RPC request/response validation.
- `l1.lua`: partitioned `eth_getLogs` scanning and strict L1 log ordering.
- `abi.lua`: decoding for the `InputAdded` / `EvmAdvance` envelope.
- `machine.lua`: narrow adapter boundary for Cartesi Machine bindings.
- `machine_cli.lua`: `cartesi-machine` CLI adapter for loading snapshot
  directories, writing raw input files, advancing, and saving a new snapshot
  directory.
- `compare.lua`: raw byte comparison.
- `checkpoint.lua`: manifest-backed checkpoint persistence.
- `alarm.lua`: webhook alarm delivery.
- `retry.lua`: bounded retry helper used by the runtime.
- `runner.lua`: one-shot orchestration across checkpoint load, sequencer poll,
  L1 fetch, CM replay, raw compare, alarm, and checkpoint write.

The L1 reader follows the Rust partition strategy from
`sequencer/src/partition.rs`: if an RPC provider rejects a large range, the
range is split recursively and retried. Lua decodes and validates input
envelopes, but it does not classify payload tags. Direct input vs batch
submission remains scheduler logic inside the canonical machine.

`l1.lua` has the `InputAdded(address,uint256,bytes)` event topic baked in and
filters logs by `topic0 = InputAdded` and `topic1 = app address`, matching the
Rust reader's app-filtered InputBox scan.

## Runtime Contract

The future sequencer endpoint shape should be generic over the app state bytes,
even though the toy wallet app will likely use JSON:

```json
{
  "safe_block": 123,
  "state": "{\"balances\":{}}"
}
```

`state` must be the exact bytes produced by the bare-metal app serializer
for the app state anchored at `safe_block`. The watchdog compares those raw
bytes with the bytes returned by CM inspect. It must not canonicalize both
values before deciding pass/fail.

The main design gate is safe-state semantics: if the sequencer has already
applied soft-confirmed transactions beyond L1 safety, `get_state` still needs a
safe-only state view through snapshotting, replay, or a separate projection.

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

`WATCHDOG_MODE=compare` is reserved for the future state comparison flow once
CM inspect and the sequencer state endpoint are available.

Useful runtime knobs:

- `WATCHDOG_CM_EXECUTABLE`: Cartesi Machine executable, default
  `cartesi-machine`.
- `WATCHDOG_CM_WORK_DIR`: temporary directory for staged input files, default
  `/tmp`.
- `WATCHDOG_RETRY_ATTEMPTS`: bounded retry attempts per run, default `3`.
- `WATCHDOG_RETRY_DELAY_SEC`: delay between retry attempts, default `5`.
- `WATCHDOG_TARGET_SAFE_BLOCK`: manual/test override for the target safe block.

## Local Tests

Run the pure Lua tests with:

```bash
just test-watchdog
```

These cover raw comparison, golden InputAdded ABI decoding, L1 ordering,
recursive range partitioning, JSON-RPC `eth_getLogs` filter construction,
config parsing, checkpoint writes, advance-mode runner behavior, the
fake-backed compare runner, the CLI adapter's input file staging, and retry
exhaustion/success behavior.

End-to-end comparison tests will be added once CM inspect and the sequencer
`get_state` endpoint are available.
