# TODO

## North Star

Build a robust sequencer prototype for a future DeFi stack, with deterministic ordering, low-latency acks, and strong replay/canonical alignment.

---

## MVP Scope (In Scope)

### 1) Sequencer

- Keep and harden write path: API -> inclusion lane -> app execution -> persistence.
- Implement direct-input reader from blockchain (ingests into `direct_inputs`).
- Implement batch submitter (reads closed batches and submits on-chain).
- Implement `L2Tx` broadcaster (WebSocket fanout of ordered `L2Tx`s).
- Implement inclusion fee estimator module that updates the suggested fee in DB (`recommended_fees`).
- Add API endpoint to query current suggested inclusion fee.
- Keep storage/replay semantics deterministic and catch-up-safe.

---

### 2) Canonical App / Scheduler

- Implement scheduler behavior in `canonical-app` using shared `app-core`.
- Ensure deterministic ordering model compatible with persisted sequencer order.
- Canonical app is the state-transition artifact used by verification flow (Cartesi Machine / RISC-V path), not by sequencer runtime itself.
- Add focused tests for queue/drain/backstop behavior and ordering invariants.

---

### 3) Benchmarks & Latency

- Build benchmark harnesses in `benchmarks/` (using Rust client code paths).
- Measure ack latency and end-to-end latency.
- Report p50 / p95 / p99.
- Measure idle and under-load behavior.
- Include network-aware runs (client/server on different hosts).
- Note: end-to-end depends on `L2Tx` broadcaster being available.

---

## Post-MVP (Nice to Have / Dogfooding Artifacts)

- `sdk/ts-client/`: TypeScript client library for browser/server JS callers.
- `tools/cli/`: Rust CLI for manual tx submission and debugging flows.
- `examples/web-demo/`: browser demo app consuming `sdk/ts-client`.

Notes:

- These are intentionally outside MVP scope.
- Still valuable for dogfooding and contributor onboarding.
