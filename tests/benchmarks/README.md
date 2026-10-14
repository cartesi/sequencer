# Benchmarks

This crate contains executable benchmark harnesses for the sequencer API.

Benchmark goals, measurement definitions, and reporting requirements live in
[`BENCHMARK_SPEC.md`](./BENCHMARK_SPEC.md).

## Domain Model

Networked benchmarks sign EIP-712 payloads, so they need to know which sequencer
instance they are targeting.

- **External target**: pass `--domain-chain-id` and `--domain-verifying-contract` to match the target sequencer deployment. Round-trip runs also read the feed from the sequencer's internal listener (`--internal-endpoint`, default `http://127.0.0.1:3001`).
- **Self-contained target**: the harness spawns `anvil`, deploys a local `Application` through `ApplicationFactory`, and uses that deployed app address as the verifying contract for both signer and sequencer:
  - chain ID: `31337`
  - verifying contract: dynamically deployed local `Application`
  - domain name: `CartesiAppSequencer`
  - domain version: `1`

## Commands

From repository root:

```bash
just setup
just --justfile tests/benchmarks/justfile bench-ack-self
just --justfile tests/benchmarks/justfile bench-round-trip-self
just --justfile tests/benchmarks/justfile bench-sweep-self
just --justfile tests/benchmarks/justfile bench-rt-sweep-self
just --justfile tests/benchmarks/justfile bench-capacity-sweep-self
just --justfile tests/benchmarks/justfile bench-report
just --justfile tests/benchmarks/justfile bench-compare-latest
just --justfile tests/benchmarks/justfile all
just --justfile tests/benchmarks/justfile all-and-compare
```

The CLI and `just` recipes default `max_fee=2000`, above the self-contained
harness's initial frame fee of 1356. External deployments can charge a different
fee. A run whose `--max-fee` is below the frame fee rejects its transactions
with `422 EXECUTION_REJECTED`; choose a sufficient limit and require accepted
transactions and matching WS events before interpreting latency results.

Direct `cargo` examples:

```bash
# Round-trip latency, self-contained (spawns anvil + sequencer). Runs are
# time-bounded (`--duration-secs`), not count-bounded.
cargo run -p benchmarks --bin sweep --release -- --round-trip --self-contained --duration-secs 30 --concurrency-list 4 --max-fee 2000

# Against an external sequencer (pass that deployment's EIP-712 domain):
cargo run -p benchmarks --bin sweep --release -- --round-trip --endpoint http://127.0.0.1:3000 --internal-endpoint http://127.0.0.1:3001 --domain-chain-id 31337 --domain-verifying-contract 0x1111111111111111111111111111111111111111 --duration-secs 30 --concurrency-list 4 --max-fee 2000

# Concurrency sweep — ack latency by default, round-trip with `--round-trip`:
cargo run -p benchmarks --bin sweep --release -- --self-contained --duration-secs 30 --max-fee 2000 --concurrency-list "1 2 4 8 16 32 64 128"
cargo run -p benchmarks --bin sweep --release -- --round-trip --self-contained --duration-secs 30 --max-fee 2000 --concurrency-list "1 2 4 8"

# Aggregate the JSON artifacts / compare the two latest of a kind:
cargo run -p benchmarks --bin report --release -- --results-dir tests/benchmarks/results
cargo run -p benchmarks --bin compare_latest --release -- --results-dir tests/benchmarks/results --kind round-trip
```

## Benchmarks

- `sweep`: runs a concurrency sweep — ack latency (`POST /tx` acknowledgement) by default, or with `--round-trip` also submit-to-broadcast latency (`POST /tx` to the matching feed event) for accepted txs — and writes its rows and capacity summary to `ack-sweep-<ts>.json` or `rt-sweep-<ts>.json` under `--results-dir` (or to `--json-out`), plus a CSV of the rows. Reports separate first-of-each markers:
  - first rejection of any kind
  - first HTTP non-`200`
  - first `429`
  - first client-side failure (`io_*`, timeouts, connection failures)
- `report`: aggregates the JSON artifacts under `--results-dir` into a summary.
- `compare_latest`: prints deltas between the two latest artifacts of a `--kind`. `ack` and `round-trip` compare single-concurrency runs, `sweep` and `rt-sweep` multi-concurrency runs, and `all` does all four.
- `report` and `compare_latest` read only `ack-sweep-*.json` and `rt-sweep-*.json`, and skip with a warning any they cannot parse as the current format; `just --justfile tests/benchmarks/justfile clean` clears old results.

## Notes

- Self-contained variants launch `anvil --load-state` from the preloaded rollups dump under `tests/.deps/`; run `just setup` first.
- Self-contained sweeps run the load clients and sequencer on the same machine. At high concurrency the clients can starve the sequencer, producing a throughput plateau and rising latency without offering more usable server load. Treat this as host-level regression evidence, not the sequencer's capacity ceiling; use a separate-machine load generator for capacity measurement.
- Self-contained variants also deploy a local `Application` through `ApplicationFactory`, so they require a canonical machine image at `examples/canonical-app/out/canonical-machine-image`; run `just canonical-build-machine-image` first.
- Self-contained variants therefore require Foundry's `anvil` binary to be installed locally.
- `--max-fee` must be at or above the placeholder app's base fee, or every tx is rejected (`422 EXECUTION_REJECTED`) and the run reports no accepted txs. The error message includes the rejection breakdown and the first rejection body, which names the base fee.
- `sweep --round-trip` subscribes from the latest snapshot's history claim and drains the existing backlog before timing, so stale history does not pollute the measurement window.
- If sweep hits `Too many open files`, increase the shell limit (`ulimit -n 4096`) or use a smaller concurrency list.
- Self-contained variants automatically build a temp DB, spawn `anvil`, start the sequencer, and persist logs/results under `tests/benchmarks/results`.
- For non-self-contained runs, start a sequencer instance first and make sure the benchmark domain matches the sequencer domain.
