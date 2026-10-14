# Benchmark Spec

This document defines benchmark intent, measurement definitions, methodology, target scenario, target goals, and reporting requirements for the sequencer.

## 1. Purpose

The external requirement is abstract: `latency <= 500ms`.
This spec makes that requirement measurable and repeatable.

## 2. Measurement Catalog

### 2.1 Latency Metrics

1. `ack_latency_ms`: elapsed time from client send of `POST /tx` to successful HTTP ack (`200`).
2. `soft_confirm_latency_ms`: elapsed time from client send of `POST /tx` to receipt of matching `user_op` event on `GET /ws/subscribe`.

### 2.2 Throughput and Volume Metrics

1. `accepted_tps`: accepted tx count divided by total run duration.
2. `accepted_count`: number of accepted requests.
3. `rejected_count`: number of rejected requests.
4. `rejection_rate`: `rejected_count / (accepted_count + rejected_count) * 100`.

### 2.3 Capacity and Overload Metrics

1. `max_sustainable_tps_at_0_rejections`: highest accepted TPS observed while `rejection_rate == 0%`.
2. `tps_at_first_any_rejection`: throughput point where the first rejection of any kind appears.
3. `tps_at_first_non_200`: throughput point where the first HTTP non-`200` response appears.
4. `tps_at_first_429`: throughput point where the first `429 OVERLOADED` response appears.
5. `tps_at_first_client_failure`: throughput point where the first timeout/network/client-side submission failure appears.

### 2.4 Memory Metrics

1. `rss_start_mb`: sequencer process RSS at run start.
2. `rss_peak_mb`: peak sequencer process RSS during run.
3. `rss_end_mb`: sequencer process RSS at run end.
4. `rss_growth_mb`: `rss_end_mb - rss_start_mb`.
5. `rss_growth_per_1k_accepted_tx_mb`: RSS growth normalized by accepted tx volume.

### 2.5 Storage and Drain Metrics

1. `logical_db_bytes`: `(page_count - freelist_count) * page_size`, read over a read-only connection at run end. It counts pages still in the WAL and excludes free pages.
2. `db_bytes` and `wal_bytes`: on-disk footprint of the SQLite file and its WAL at run end.
3. `growth_bytes_per_1k_accepted`: growth of `logical_db_bytes` over the measured steps, normalized by accepted tx volume.
4. `batches_pending`: valid sealed batches whose nonce is not yet in `safe_accepted_batches` when the drain starts. The drain starts when the last measured step returns: every load worker is past its deadline, and in round-trip mode the WS matcher has finished, bounded by `--max-ws-wait-ms`.
5. `drain_secs`: time from the drain start until `safe_accepted_batches` holds the highest valid sealed batch nonce read at the start. The open batch is excluded, so this is the submission backlog, not the batch-open window.

## 3. Measurement Method

### 3.1 Percentiles and Sample Size

1. Required latency percentiles: p50, p95, p99, p99.9.
2. Target percentile is `p99` for both latency goals.
3. `p99.9` is diagnostic in this phase.
4. Runs are valid for target evaluation only when `accepted_count >= 5,000`.
5. `p99.9` is diagnostic below `accepted_count` 10,000.

### 3.2 Request Outcome Classification

1. `accepted`: HTTP status `200` from `POST /tx`.
2. `rejected`: non-`200`, timeout, or network/client failure.

### 3.3 Workload Validity Rules

1. Target evaluation must use valid-only benchmark traffic (no intentionally invalid transactions).
2. Non-`200` responses must be broken down by status code so overload (`429`) is distinguishable from invalid-input failures (`400`/`422`).
3. Client/network failures must be reported separately from HTTP non-`200` responses so client saturation is not mistaken for sequencer overload.
4. If no non-`200` appears, `tps_at_first_non_200` and `tps_at_first_429` must be reported as `not reached`.
5. If no client/network failure appears, `tps_at_first_client_failure` must be reported as `not reached`.

### 3.4 Memory Collection Rules

1. Memory metrics must be collected for the sequencer process.
2. Reports must include collection method/tool (`ps`, `/proc`, container stats, etc.).
3. Reports must include sampling interval (recommended `250ms` to `1000ms`).
4. Memory has no hard pass/fail threshold in this phase; it is profiling-critical.

### 3.5 Sanity Assertions (Correctness, Non-Metric)

1. For end-to-end runs, every HTTP-accepted request must have a matching WS `user_op` within the configured wait timeout.
2. Any sanity assertion failure indicates a correctness bug and invalidates the benchmark run.

### 3.6 Storage and Drain Collection Rules

1. Self-contained runs only. The start size is sampled after warmup, the end size after the drain and before shutdown, because a clean close checkpoints the WAL away.
2. The WAL file fills to SQLite's default autocheckpoint size (1000 pages, about 4 MB) and stays there, because a checkpoint reuses it rather than truncating it. That is why growth uses the logical size; the file sizes are footprint only.
3. During the drain the harness mines one L1 block per second at Anvil's own clock and stops after 120 s; `drain_secs` is then reported as not reached.
4. `drain_secs` is shaped by the environment: the submitter's idle poll (5 s by default), Anvil's safe-head lag (`--slots-in-an-epoch 1`) and the 1 s poll. It does not measure submitter throughput.
5. Neither metric has a threshold in this phase.
6. A recovery cascade during the run reuses batch nonces, which makes `drain_secs` meaningless.

## 4. Benchmark Scenarios

### 4.1 Profiling Scenarios

1. Same-host baseline (`no injected latency`) for low-noise branch comparisons.
2. Canonical network-aware scenario (not implemented; see §4.2).
3. Throughput/concurrency sweeps to locate knee and overload behavior.

### 4.2 Canonical Network-Aware Scenario (Target Evaluation)

Target evaluation injects `+50ms` one-way (`~100ms RTT`, `+/-10ms` jitter, no loss) on both `POST /tx` and the WS subscription.
No shaper exists, so every run is a same-host baseline and carries no verdict.

## 5. Targets

### 5.1 Target Goals

1. `ack_latency_ms p99 <= 500ms`
2. `soft_confirm_latency_ms p99 <= 1000ms`

### 5.2 Target Evaluation Conditions

A run evaluates the goals above only when it has zero rejections, runs under the canonical network-aware scenario (§4.2), and meets the sample-size rule (§3.1).
The soft-confirm goal also requires the sanity assertions (§3.5) to hold.

## 6. Required Report Content

Each benchmark report must include:

1. Latency percentiles for measured metrics.
2. Accepted/rejected counts and rejection rate.
3. Accepted TPS and total run duration.
4. Rejection reason breakdown (when available).
5. Network profile (a fixed string; same-host baseline until a shaper exists).
6. Memory metrics and memory sampling method/interval.
7. Storage and drain metrics (self-contained runs).
8. Full run configuration (command, deployment domain, concurrency/arrival settings, timeout settings).
9. Sanity assertion status and failure summary (if any).
10. For sweeps, separate first-knee metrics for any rejection, HTTP non-`200`, `429`, and client-side failure.

## 7. Mapping to Current Harnesses

Recipes are in [`justfile`](./justfile).

1. `bench-ack-self` (`sweep` at concurrency 1): `ack_latency_ms`.
2. `bench-round-trip-self` (`sweep --round-trip` at one concurrency): `soft_confirm_latency_ms`.
3. `bench-sweep-self`, `bench-capacity-sweep-self` and `bench-rt-sweep-self` (`sweep`): capacity exploration and overload discovery.
4. `bench-report` and `bench-compare-latest` (`report`, `compare_latest`): summaries and deltas over the saved results.

## 8. Current Gaps

1. Add first-class support for canonical injected-latency runs; target verdicts (§5) need it.
2. Strengthen tx identity matching for round-trip correlation if workloads ever stop guaranteeing effectively unique sender/payload pairs.
