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
just --justfile benchmarks/justfile bench-unit
just --justfile benchmarks/justfile bench-ack-self
just --justfile benchmarks/justfile bench-e2e-self
just --justfile benchmarks/justfile bench-hammer-self
just --justfile benchmarks/justfile bench-sweep-self
just --justfile benchmarks/justfile bench-compare-latest
just --justfile benchmarks/justfile all
just --justfile benchmarks/justfile all-and-compare
```

Direct `cargo` examples:

```bash
cargo run -p benchmarks --bin unit_hot_path -- --count 10000 --max-fee 0
cargo run -p benchmarks --bin ack_latency -- --self-contained --count 200 --max-fee 0 --concurrency 1
cargo run -p benchmarks --bin e2e_latency -- --self-contained --count 100 --max-fee 0 --from-offset 0 --concurrency 1
cargo run -p benchmarks --bin ack_latency -- --self-contained --count 5000 --max-fee 0 --concurrency 32 --evaluate
cargo run -p benchmarks --bin e2e_latency -- --self-contained --count 5000 --max-fee 0 --from-offset 0 --concurrency 16 --evaluate
cargo run -p benchmarks --bin ack_latency -- --endpoint http://127.0.0.1:3000 --domain-chain-id 31337 --domain-verifying-contract 0x1111111111111111111111111111111111111111 --count 200 --max-fee 0 --concurrency 1
cargo run -p benchmarks --bin e2e_latency -- --endpoint http://127.0.0.1:3000 --domain-chain-id 31337 --domain-verifying-contract 0x1111111111111111111111111111111111111111 --count 100 --max-fee 0 --from-offset 0 --concurrency 1
cargo run -p benchmarks --bin sweep -- --self-contained --mode e2e --count 1000 --max-fee 0 --from-offset 0 --concurrency-list "1 2 4 8 16 32 64 96 128"
cargo run -p benchmarks --bin compare_latest --release -- --results-dir benchmarks/results --kind all --sweep-mode e2e
```

## Benchmarks

- `unit_hot_path`: measures local signing plus request JSON encoding.
- `ack_latency`: measures `POST /tx` acknowledgement latency for accepted txs.
- `e2e_latency`: measures submit-to-broadcast latency (`POST /tx` to matching `GET /ws/subscribe` event) for accepted txs.
- `--evaluate` on `ack_latency` and `e2e_latency`: prints a first-class target verdict block and stores it in JSON output. Today the verdict is expected to be `NOT_EVALUATED` because the harness only supports the same-host baseline, not the canonical network-aware profile from the spec.
- `bench-hammer`: high-concurrency e2e run that verifies each accepted tx is observed on WS.
- `bench-sweep`: runs a concurrency sweep and emits a CSV plus capacity summary. Sweep reports separate:
  - first rejection of any kind
  - first HTTP non-`200`
  - first `429`
  - first client-side failure (`io_*`, timeouts, connection failures)
- `bench-compare-latest`: compares the latest two benchmark artifacts and prints deltas. Use `--sweep-mode ack|e2e` to choose which sweep family to compare.
- `bench-soak-low-lat-self` and `bench-soak-high-throughput-self`: write timestamped JSON outputs by default so repeated runs do not overwrite previous soak artifacts. Pass `out=...` to force a specific path.

## Notes

- Self-contained variants launch `anvil --load-state` from the preloaded rollups dump under `benchmarks/.deps/`; run `just setup` first.
- Self-contained variants also deploy a local `Application` through `ApplicationFactory`, so they require a canonical machine image at `examples/canonical-app/out/canonical-machine-image`; run `just canonical-build-machine-image` first.
- Self-contained variants therefore require Foundry's `anvil` binary to be installed locally.
- Networked benches fail by default if any tx is rejected. Pass `--allow-rejections` to inspect mixed traffic.
- `e2e_latency` drains existing WS backlog before timing so stale history does not pollute the measurement window.
- `bench-sweep mode=e2e` carries `from_offset` forward across rounds to avoid re-reading old WS history.
- `--stop-on-first-non-200` now does exactly what it says: it stops on the first HTTP non-`200`, not on client-side transport failures.
- If sweep hits `Too many open files`, increase the shell limit (`ulimit -n 4096`) or use a smaller concurrency list.
- Self-contained variants automatically build a temp DB, spawn `anvil`, start the sequencer, and persist logs/results under `benchmarks/results`.
- For non-self-contained runs, start a sequencer instance first and make sure the benchmark domain matches the sequencer domain.
