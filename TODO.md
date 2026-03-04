# TODO

## North Star

Build a robust sequencer prototype for a future DeFi stack, with deterministic ordering, low-latency acks, and strong replay/canonical alignment.

---

## Done

### Sequencer Foundation

- Thin binary entrypoint plus library runtime (`sequencer::run`, `RunConfig`).
- Simplified runtime/config surface with explicit EIP-712 deployment inputs.
- Hardened write path: API -> inclusion lane -> app execution -> persistence -> ack.
- `L2Tx` broadcaster with WebSocket fanout of ordered `L2Tx`s.
- Bounded WebSocket catch-up window plus subscriber guardrails.
- Shared shutdown supervision across API, inclusion lane, and broadcaster.
- Paged replay/catch-up in inclusion lane and broadcaster to avoid unbounded startup memory growth.
- Persisted `safe_block` frontier model for frames, with leading direct inputs materialized when opening a new frame.

### Benchmarks & Tooling

- Benchmark harnesses in `benchmarks/` for ack latency, end-to-end latency, sweeps, and unit hot path.
- Baseline reporting for p50 / p95 / p99, throughput, and RSS trends.
- Same-host benchmark workflows and docs aligned with the current runtime/config model.

---

## MVP Scope (Remaining)

### 1) Sequencer Core

- Implement direct-input reader from blockchain (ingests into `direct_inputs`).
- Implement batch submitter (reads closed batches and submits on-chain).
- Implement inclusion fee estimator module that updates the suggested fee in DB (`recommended_fees`).
- Add paginated historical `L2Tx` sync endpoint so lagging readers can backfill over HTTP before switching to `/ws/subscribe` for live updates.
- Keep storage/replay semantics deterministic and catch-up-safe as direct-input ingestion, batch submission, and recovery flows land.

### 2) Recovery / Canonicality

- Define how canonical progress is derived from persisted facts so replay stays deterministic.
- Detect when scheduler/canonical execution invalidates previously closed batches.
- Define the recovery procedure when persisted batches are invalidated:
  - fail fast if the persisted state is inconsistent with canonical inputs
  - rebuild or flush invalidated batches before resuming normal service
  - notify readers when batches are invalidated
  - notify readers when batches become final on-chain

### 3) Canonical App / Scheduler

- Implement scheduler behavior in `examples/canonical-app` using shared `sequencer-core` + `examples/app-core`.
- Ensure deterministic ordering model compatible with persisted sequencer order.
- Keep the canonical app as the state-transition artifact used by verification flow (Cartesi Machine / RISC-V path), not by sequencer runtime itself.
- Add focused tests for queue/drain/backstop behavior and ordering invariants.

### 4) Benchmarks & Evaluation

- Add canonical network-aware benchmark runs (client/server on different hosts or with injected latency/jitter).
- Turn target evaluation into a real pass/fail mode for the canonical network profile, not just same-host comparison.
- Tune queue / broadcaster / buffer sizing from benchmark evidence instead of ad hoc guesses.
- Revisit inclusion-lane adaptive chunk sizing only after the baseline latency/throughput envelopes are stable.

### 5) Client / API Ergonomics

- Add API endpoint to query current suggested inclusion fee.
- Decide whether wallet-specific convenience endpoints belong in the sequencer or in the application/client layer:
  - current nonce / tx count
  - EIP-712 domain discovery
- If those helper endpoints stay in the sequencer, implement them with a clear separation between core sequencer state and wallet-app-specific state.

---

## Post-MVP (Nice to Have / Dogfooding Artifacts)

- `sdk/ts-client/`: TypeScript client library for browser/server JS callers.
- `sdk/cli/`: Rust CLI for manual tx submission and debugging flows.
- `examples/web-demo/`: browser demo app consuming `sdk/ts-client`.

Notes:

- These are intentionally outside MVP scope.
- Still valuable for dogfooding and contributor onboarding.
