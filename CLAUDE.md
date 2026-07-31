# CLAUDE.md

Quick reference for working in this repository. For the full guide — architecture, duality, recovery, invariants, threat model, and rules — read [`AGENTS.md`](AGENTS.md).

## Shell Environment

This project uses Nix + direnv. Before running any command that needs project tools (Foundry, TLA+, etc.), activate the direnv environment:

```bash
eval "$(direnv export bash 2>/dev/null)"
```

This makes `anvil`, `forge`, `cast`, `tlc`, and other Nix-provided tools available. Cargo and rustc are available without direnv.

## Commands

```bash
cargo check                                              # compile check
cargo test --workspace --exclude canonical-test          # run tests (canonical-test needs libslirp)
cargo fmt --all                                          # format
cargo clippy --all-targets --all-features -- -D warnings # lint
cargo test -p sequencer --lib                            # includes Anvil-backed tests (needs Foundry on PATH)
```

## What This Is

Off-chain sequencer for an app-specific DeFi rollup. Accepts signed user operations, issues low-latency soft confirmations, and posts batches to L1. Currently backed by a placeholder wallet app (transfer, withdrawal). **Security-critical infrastructure** — handle every change accordingly.

Rust edition 2024 / Axum API / SQLite (rusqlite, WAL) / EIP-712 signing / SSZ encoding.

## Workspace Layout

- `sequencer/` — sequencer library (no binary; app crates build the binary).
- `sequencer-core/` — shared domain types consumed by both sequencer and scheduler.
- `examples/app-core/` — placeholder wallet app implementing `Application`.
- `examples/wallet-sequencer/` — binary crate: wallet app + sequencer library.
- `examples/canonical-app/` — on-chain scheduler reference implementation.
- `examples/canonical-test/` — e2e test harness for the canonical app.
- `sdk/rust-client/` — Rust client library for the sequencer API.
- `tests/{benchmarks,e2e,harness}/` — test infrastructure.

## Sequencer Module Layout

`sequencer/src/` is organized by writer role; `storage/<role>.rs` holds each role's storage half.

- `runtime/` — bootstrap, config, shutdown, shared clock.
- `ingress/` — public write path: `api.rs` (`POST /tx`) + `inclusion_lane/` (hot path).
- `egress/` — internal read path: `api/` (WS subscribe + health) + `l2_tx_feed/`.
- `l1/` — reader, submitter, fee oracle, provider, partition helper.
- `recovery/` — startup preemptive-recovery procedure, runtime danger detector, mempool flusher.
- `storage/` — SQLite persistence, split per writer role.
- `http.rs` — shared HTTP error type + `axum::serve` orchestration.

## Before You Start Real Work

- **[`AGENTS.md`](AGENTS.md)** — mission, requirements, invariants, duality, recovery, conventions, rules.
- **[`docs/protocol/`](docs/protocol/)** — the authoritative protocol contracts: [`scheduler-semantics.md`](docs/protocol/scheduler-semantics.md) (canonical acceptance algorithm) and [`application-contract.md`](docs/protocol/application-contract.md) (the `Application` FFI trait). Read before touching the scheduler, the gold frontier, the fold, or an `Application` impl.
- **[`docs/invariants.md`](docs/invariants.md)** — cross-module invariants register + the fail-loud check policy. Check it before changing anything it lists as load-bearing.
- **[`docs/review/`](docs/review/)** — dated correctness-review ledgers: known-open findings, settled designs, work packages. Check for open findings in code you're about to touch.
- **[`docs/threat-model/README.md`](docs/threat-model/README.md)** — trust boundaries and in-scope threats.
- **[`docs/recovery/README.md`](docs/recovery/README.md)** — preemptive recovery design + TLA+ proofs.
- **[`docs/snapshots/lifecycle.md`](docs/snapshots/lifecycle.md)** — snapshot lifecycle design + invariants (take/promote/GC, crash-safety). Read before touching the inclusion lane's safe-frontier/snapshot path.
- **[`docs/watchdog/operator-deployment.md`](docs/watchdog/operator-deployment.md)** — watchdog on live L1 (Sepolia / mainnet, production-like).
- **[`docs/watchdog/getting-started.md`](docs/watchdog/getting-started.md)** — local dev: watchdog + `sequencer-devnet` on Anvil.
