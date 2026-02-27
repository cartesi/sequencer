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
2. `tps_at_first_non_200`: throughput point where first non-`200` response appears.
3. `tps_at_first_429`: throughput point where first `429 OVERLOADED` response appears.

### 2.4 Memory Metrics

1. `rss_start_mb`: sequencer process RSS at run start.
2. `rss_peak_mb`: peak sequencer process RSS during run.
3. `rss_end_mb`: sequencer process RSS at run end.
4. `rss_growth_mb`: `rss_end_mb - rss_start_mb`.
5. `rss_growth_per_1k_accepted_tx_mb`: RSS growth normalized by accepted tx volume.

## 3. Measurement Method

### 3.1 Percentiles and Sample Size

1. Required latency percentiles: p50, p95, p99, p99.9.
2. Target percentile is `p99` for both latency goals.
3. `p99.9` is diagnostic in this phase.
4. Runs are valid for target evaluation only when `accepted_count >= 5,000`.
5. `p99.9` is considered reliable when `accepted_count >= 10,000`; otherwise it must be marked diagnostic-low-confidence.

### 3.2 Request Outcome Classification

1. `accepted`: HTTP status `200` from `POST /tx`.
2. `rejected`: non-`200`, timeout, or network/client failure.

### 3.3 Workload Validity Rules

1. Target evaluation must use valid-only benchmark traffic (no intentionally invalid transactions).
2. Non-`200` responses must be broken down by status code so overload (`429`) is distinguishable from invalid-input failures (`400`/`422`).
3. If no non-`200` appears, `tps_at_first_non_200` and `tps_at_first_429` must be reported as `not reached`.

### 3.4 Memory Collection Rules

1. Memory metrics must be collected for the sequencer process.
2. Reports must include collection method/tool (`ps`, `/proc`, container stats, etc.).
3. Reports must include sampling interval (recommended `250ms` to `1000ms`).
4. Memory has no hard pass/fail threshold in this phase; it is profiling-critical.

### 3.5 Sanity Assertions (Correctness, Non-Metric)

1. For end-to-end runs, every HTTP-accepted request must have a matching WS `user_op` within the configured wait timeout.
2. Any sanity assertion failure indicates a correctness bug and invalidates the benchmark run.

## 4. Benchmark Scenarios

### 4.1 Profiling Scenarios

1. Same-host baseline (`no injected latency`) for low-noise branch comparisons.
2. Canonical network-aware scenario.
3. Throughput/concurrency sweeps to locate knee and overload behavior.

### 4.2 Canonical Network-Aware Scenario (Target Evaluation)

1. The injected profile must be applied to both HTTP `POST /tx` and WS `GET /ws/subscribe` paths.
2. Base delay: `+50ms one-way` in each direction (`~100ms RTT`).
3. Jitter: `+/-10ms` around base delay.
4. Packet loss: `0%`.
5. No artificial bandwidth cap.
6. Packet reordering, duplication, and corruption injection disabled.
7. Reports must include the network shaping tool and exact shaping config.

## 5. Targets and Verdicts

### 5.1 Target Goals

1. `ack_latency_ms p99 <= 500ms`
2. `soft_confirm_latency_ms p99 <= 1000ms`

### 5.2 Target Evaluation Conditions

The latency goals above are evaluated under these conditions:

1. `rejection_rate == 0%` (equivalently `rejected_count == 0`).
2. Canonical network-aware scenario is satisfied.
3. Run is valid per sample-size policy.

### 5.3 Separate Verdicts

1. `ACK_TARGET` is `PASS` when all hold:
   - target evaluation conditions are satisfied
   - `ack_latency_ms p99 <= 500ms`
2. `SOFT_CONFIRM_TARGET` is `PASS` when all hold:
   - target evaluation conditions are satisfied
   - `soft_confirm_latency_ms p99 <= 1000ms`
   - sanity assertions are satisfied

## 6. Required Report Content

Each benchmark report must include:

1. Latency percentiles for measured metrics.
2. Accepted/rejected counts and rejection rate.
3. Accepted TPS and total run duration.
4. Rejection reason breakdown (when available).
5. Network profile and shaping method/config.
6. Memory metrics and memory sampling method/interval.
7. Full run configuration (command, concurrency/arrival settings, timeout settings).
8. Target verdict lines: `ACK_TARGET` and `SOFT_CONFIRM_TARGET` (when target evaluation is performed).
9. Sanity assertion status and failure summary (if any).

## 7. Mapping to Current Harnesses

1. `ack_latency`: primary path for `ack_latency_ms`.
2. `e2e_latency`: primary path for `soft_confirm_latency_ms`.
3. `sweep`: capacity exploration and overload discovery.
4. `unit_hot_path`: micro profiling.

## 8. Current Gaps

1. Add first-class support for canonical injected-latency runs.
2. Add explicit `p99.9` export in sweep CSV/JSON outputs.
3. Add standard `ACK_TARGET` and `SOFT_CONFIRM_TARGET` verdict lines in benchmark outputs.
4. Add rejection-reason breakdown consistently across relevant reports.
5. Ensure reports always include network shaping tool and exact shaping config.
6. Add first-class memory collection/reporting in benchmark tooling.
