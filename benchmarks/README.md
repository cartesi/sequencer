# Benchmarks

This crate contains executable benchmark harnesses for the sequencer API.

Benchmark goals, UX-facing metrics, and initial SLO targets are defined in
[`BENCHMARK_SPEC.md`](./BENCHMARK_SPEC.md).

## Commands

From repository root:

```bash
just --justfile benchmarks/justfile bench-unit
just --justfile benchmarks/justfile bench-ack
just --justfile benchmarks/justfile bench-e2e
just --justfile benchmarks/justfile bench-hammer
just --justfile benchmarks/justfile bench-sweep
just --justfile benchmarks/justfile bench-compare-latest
just --justfile benchmarks/justfile all
just --justfile benchmarks/justfile all-and-compare
```

Or from inside `benchmarks/`:

```bash
just bench-unit
just bench-ack
just bench-e2e
just bench-hammer
just bench-sweep
just bench-compare-latest
just all
just all-and-compare
```

Or directly with `cargo`:

```bash
cargo run -p benchmarks --bin unit_hot_path -- --count 10000 --max-fee 0
cargo run -p benchmarks --bin ack_latency -- --http-url http://127.0.0.1:3000 --count 200 --max-fee 0 --concurrency 1
cargo run -p benchmarks --bin e2e_latency -- --http-url http://127.0.0.1:3000 --count 100 --max-fee 0 --from-offset 0 --concurrency 1
cargo run -p benchmarks --bin sweep --release -- --mode e2e --count 1000 --url http://127.0.0.1:3000 --max-fee 0 --from-offset 0 --concurrency-list "1 2 4 8 16 32 64 96 128"
cargo run -p benchmarks --bin compare_latest --release -- --results-dir benchmarks/results --kind all
```

## Notes

- `unit_hot_path`: measures local signing + request-encoding costs (no network).
- `ack_latency`: measures `POST /tx` acknowledgment latency for accepted txs.
- `e2e_latency`: measures submit-to-broadcast latency (`POST /tx` to `GET /ws/subscribe` message) for accepted txs.
- `bench-hammer`: high-concurrency e2e run that hammers the sequencer and verifies each accepted tx is observed on WS.
- `bench-sweep`: runs a concurrency sweep (default `1..128`, `count=1000`) and emits a CSV plus an estimated knee.
- `bench-compare-latest`: compares the latest two `ack`, `e2e`, and `sweep` artifacts and prints deltas.
- For newly generated benchmark accounts, included transactions usually require sequencer frame fee `0`.
- Networked benches fail by default if any tx is rejected. Pass `--allow-rejections` to inspect mixed traffic.
- `e2e_latency` drains existing WS backlog before timing to reduce stale-history noise.
- Sweep CSV columns: `concurrency,completed_per_s,p95_ms,rejected`.
- `bench-sweep mode=e2e` carries `from_offset` forward across rounds to avoid re-reading old WS history.
- If sweep hits `Too many open files`, increase shell limit (`ulimit -n 4096`) or lower `conc_list`.
- Self-contained variants automatically spawn a sequencer and persist logs/results.
- For non-self-contained networked benches, run a sequencer instance beforehand, for example:

```bash
just run
```
