# Benchmarks

This crate contains executable benchmark harnesses for the sequencer API.

Benchmark goals, measurement definitions, and reporting requirements live in
[`BENCHMARK_SPEC.md`](./BENCHMARK_SPEC.md).

## Domain Model

Networked benchmarks sign EIP-712 payloads, so they need to know which sequencer
instance they are targeting.

- **External target**: pass `--domain-chain-id` and `--domain-verifying-contract` to match the target sequencer deployment.
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

The `just` recipes default `max_fee=1200`, which is above the placeholder app's
base fee. A run whose `--max-fee` is below the base fee has **every** tx
rejected (`422 EXECUTION_REJECTED: "max fee N below base fee ..."`) and reports
no accepted txs — set a fee at or above the base fee.

Direct `cargo` examples:

```bash
# Round-trip latency, self-contained (spawns anvil + sequencer). Runs are
# time-bounded (`--duration-secs`), not count-bounded.
cargo run -p benchmarks --bin round_trip_latency --release -- --self-contained --duration-secs 30 --concurrency 4 --max-fee 1200
cargo run -p benchmarks --bin round_trip_latency --release -- --self-contained --duration-secs 60 --concurrency 16 --max-fee 1200 --evaluate

# Against an external sequencer (pass that deployment's EIP-712 domain):
cargo run -p benchmarks --bin round_trip_latency --release -- --endpoint http://127.0.0.1:3000 --domain-chain-id 31337 --domain-verifying-contract 0x1111111111111111111111111111111111111111 --duration-secs 30 --concurrency 4 --max-fee 1200

# Concurrency sweep — ack latency by default, round-trip with `--round-trip`:
cargo run -p benchmarks --bin sweep --release -- --self-contained --duration-secs 30 --max-fee 1200 --concurrency-list "1 2 4 8 16 32 64 128"
cargo run -p benchmarks --bin sweep --release -- --round-trip --self-contained --duration-secs 30 --max-fee 1200 --concurrency-list "1 2 4 8"

# Aggregate the JSON artifacts / compare the two latest of a kind:
cargo run -p benchmarks --bin report --release -- --results-dir tests/benchmarks/results
cargo run -p benchmarks --bin compare_latest --release -- --results-dir tests/benchmarks/results --kind round-trip
```

## Benchmarks

- `round_trip_latency`: measures submit-to-broadcast latency (`POST /tx` to the matching `GET /ws/subscribe` event) for accepted txs. Drains existing WS backlog before timing so stale history does not pollute the window.
- `sweep`: runs a concurrency sweep — ack latency (`POST /tx` acknowledgement) by default, or round-trip with `--round-trip` — and emits a CSV plus capacity summary. Reports separate first-of-each markers:
  - first rejection of any kind
  - first HTTP non-`200`
  - first `429`
  - first client-side failure (`io_*`, timeouts, connection failures)
- `report`: aggregates the JSON artifacts under `--results-dir` into a summary.
- `compare_latest`: compares the two latest artifacts of a `--kind` (`ack`, `round-trip`, `rt-sweep`, `sweep`, `all`) and prints deltas.
- `--evaluate` on `round_trip_latency` / `sweep`: prints a first-class target verdict block and stores it in JSON output. Today the verdict is expected to be `NOT_EVALUATED` because the harness only supports the same-host baseline, not the canonical network-aware profile from the spec.

## Notes

- Self-contained variants launch `anvil --load-state` from the preloaded rollups dump under `tests/benchmarks/.deps/`; run `just setup` first.
- Self-contained variants also deploy a local `Application` through `ApplicationFactory`, so they require a canonical machine image at `examples/canonical-app/out/canonical-machine-image`; run `just canonical-build-machine-image` first.
- Self-contained variants therefore require Foundry's `anvil` binary to be installed locally.
- `--max-fee` must be at or above the placeholder app's base fee, or every tx is rejected (`422 EXECUTION_REJECTED`) and the run reports no accepted txs. The error message includes the rejection breakdown and the first rejection body, which names the base fee.
- `round_trip_latency` drains existing WS backlog before timing so stale history does not pollute the measurement window.
- `sweep --round-trip` carries `from_offset` forward across rounds to avoid re-reading old WS history.
- If sweep hits `Too many open files`, increase the shell limit (`ulimit -n 4096`) or use a smaller concurrency list.
- Self-contained variants automatically build a temp DB, spawn `anvil`, start the sequencer, and persist logs/results under `tests/benchmarks/results`.
- For non-self-contained runs, start a sequencer instance first and make sure the benchmark domain matches the sequencer domain.
