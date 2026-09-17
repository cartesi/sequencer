# Track 3 integration validation — 2026-09-16

Scope: validate application-history commit
`799a5d3aba71d7ec1c5f48fda3b05bfd536b1a11` with the pinned canonical machine,
a complete cold replica, and a same-host latency comparison against
`91e25780854bb641c63135751f951f9f7ee1e744`.

Retained for the [Track 3 integration gates](../plans/2026-07-track3-feed-replay-design.md):
this is the wallet baseline against which native-engine and deployment results
can be assessed. Replace or delete it when those decisions no longer use these
measurements. It describes the named revisions, not ongoing validation of HEAD.

## Environment and canonical agreement

The CLI, Lua module, and native library used emulator 0.20.0 from the same Nix
package in the shared development environment outside this repository.

Rust 1.95.0, Lua 5.4.7, and Foundry 1.5.1 were used. A fresh devnet canonical
image was built from this checkout with the pinned cross image and kernel;
no 0.21 machine archive was reused.

| Artifact | Hash |
|---|---|
| Canonical machine root | `fb93d09eeb69b2fbd1fc63232d428e285fa0a2000430c256e9e7489e0b89df96` |
| Devnet guest binary SHA-256 | `154250d5095bfb507314cc3291c8679a43427223c6642d3440ad6e35142e74d4` |
| Root filesystem SHA-256 | `e92485f9b34ba7cbbab9e750b55d2ffa32dd18afabecbc6ddef6108fcd275ee5` |
| Pinned Anvil fixture SHA-256 | `b140e31db2b04bb99c733fdf153718cd252335370f4b355849e2cbb3121fc30f` |

All four selected canonical-machine gates passed:

| Scenario | What it checks | Time |
|---|---|---:|
| `watchdog_genesis_compare_test` | Native genesis bytes equal CM inspection; production watchdog initializes and idles twice | 1.94 s |
| `deposit_transfer_withdrawal_test` | Ordinary application execution and non-genesis watchdog comparison | 10.49 s |
| `recovery_after_stale_batches_test` | Stale-batch recovery followed by independent from-genesis CM comparison | 11.38 s |
| `setup_recovery_round_trip_test` | Real `/finalized_snapshot` download, database wipe, `setup --recovery`, resumed execution, and independent CM comparison | 15.36 s |

These are selected integration gates, not a claim that the entire E2E suite or
private DEX adapter was tested.

## Cold replica

`cold_replica_snapshot_backlog_live_recovery_test` passed in 18.12 seconds,
including its test-owned 120-second deadline. Its claims come from HTTP headers and consumed inputs,
without querying storage for the consumer's history identity.

The test restores a nonempty tar archive, deletes the downloaded source, and
compares all application bytes/count/clock with an independently accumulated
genesis replay. Writes commit after snapshot selection but before restoration,
and between restoration and subscription. A two-way barrier pauses the consumer
after its first backlog entry until more writes have committed, proving that
producer progress overlaps incomplete client catch-up. It then verifies live
direct inputs and user operations.

An actual stale outage/restart invalidates the suffix. The old claim receives
`STALE_GENERATION`; a new archive has the same era and the next generation.
The expected replacement branch retains the accepted prefix and replays only
its retained L1 directs. Optimistic transfers disappear, and a new transfer at
the recovered nonce succeeds.

## Latency comparison

Four release-build runs used an ABBA order: baseline, current, current,
baseline. Each had 5 seconds of warmup and a 45-second measured window,
16 closed-loop workers, funded transfers of one unit, one WS observer, an
explicit max fee of 2000, a 3-second request deadline, and a 5-second WS deadline.
The host was an Apple M5 Max (18 logical CPUs, 36 GiB) running macOS 26.6.2.
No builds, correctness tests, or injected network shaping ran during measurement.
An initial attempt with max fee 1200 (below the frame fee of 1356) was discarded
during warmup; all four compared runs used the explicit 2000 limit.

Both exact revisions used their matching SDK/protocol and the same fresh machine
image, Anvil fixture, and toolchain. Per-request latency excludes funding,
startup, snapshot acquisition, backlog draining, and signing. RSS was sampled
with `ps` every 500 ms over warmup and measured traffic. These are local regression
measurements, not a fixed-arrival capacity test or a deployment/network SLO.
The harness correctly marked its network-aware target as `not_evaluated`.

| Run order | Revision | Accepted and WS-matched | TPS | Peak RSS (MiB) |
|---|---|---:|---:|---:|
| 1. baseline-1 | `91e2578` | 45,846 | 1017.46 | 23.88 |
| 2. current-1 | `799a5d3` | 45,696 | 1014.04 | 23.64 |
| 3. current-2 | `799a5d3` | 45,562 | 1010.98 | 23.36 |
| 4. baseline-2 | `91e2578` | 45,368 | 1006.40 | 23.66 |

| Run | Metric (ms) | p50 | p95 | p99 | p99.9 |
|---|---|---:|---:|---:|---:|
| baseline-1 | ACK | 14.938 | 27.268 | 28.724 | 40.252 |
| baseline-1 | Matching WS | 28.807 | 44.223 | 53.596 | 59.817 |
| current-1 | ACK | 14.975 | 27.209 | 28.139 | 37.738 |
| current-1 | Matching WS | 29.246 | 44.255 | 53.123 | 57.818 |
| current-2 | ACK | 14.990 | 27.232 | 28.123 | 34.405 |
| current-2 | Matching WS | 29.147 | 44.413 | 53.827 | 57.566 |
| baseline-2 | ACK | 15.020 | 27.361 | 28.275 | 39.366 |
| baseline-2 | Matching WS | 29.059 | 44.445 | 53.115 | 57.615 |

All 182,472 measured requests were accepted, with no client failures and a
matching WS event for each. Per-run latency and throughput overlap: there is no
clear regression at this workload. This does not bound a larger application's
execution/dump cost, deep-history replay, many subscribers, or deployment network
latency. Percentiles above belong to individual runs, not a pooled distribution.

Build both exact checkouts in release mode, retain each matching binary pair,
and run the following against a fresh self-contained stack in ABBA order:

```sh
cargo build --release --locked -p wallet-sequencer --bin wallet-sequencer-devnet -p benchmarks --bin round_trip_latency
target/release/round_trip_latency --self-contained \
  --sequencer-bin "$PWD/target/release/wallet-sequencer-devnet" \
  --accounts-file tests/benchmarks/anvil_1000_accounts.txt \
  --duration-secs 45 --warmup-secs 5 --concurrency 16 --max-fee 2000 \
  --request-timeout-ms 3000 --max-ws-wait-ms 5000 \
  --evaluate --json-out RUN.json
```

## Checks

- Workspace/all-target check and strict workspace/all-target/all-feature Clippy.
- Workspace formatting and diff whitespace checks.
- Watchdog unit tests: 62/62.
- Fresh image build, `just doctor`, and the five integration scenarios above.
- A short benchmark smoke run with the corrected implicit fee default:
  138 accepted, zero rejected, and 138 matching WS events.

In the local shared environment, commands use
`direnv exec /Users/gcdepaula/projects/cartesi-dev/sequencer` before the command:

```sh
just setup
just canonical-build-machine-image
just doctor
just test-watchdog
cargo build -p wallet-sequencer --bin wallet-sequencer-devnet -p rollups-e2e --bin rollups-e2e --locked
# Repeat for each scenario in the tables above.
target/debug/rollups-e2e SCENARIO --exact --nocapture
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --all --check
```

Native reference-bridge/DEX conformance and representative deployment latency
remain separate integration work. Sparse snapshots, archival replay, and
resumable transfers remain deferred until a consumer requires them.
