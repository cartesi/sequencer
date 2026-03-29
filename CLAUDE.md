# CLAUDE.md

## Shell Environment

This project uses Nix + direnv. Before running any command that needs project tools
(Foundry, TLA+, etc.), activate the direnv environment:

```bash
eval "$(direnv export bash 2>/dev/null)"
```

This makes `anvil`, `forge`, `cast`, `tlc`, and other Nix-provided tools available.
Cargo and rustc are available without direnv.

## Quick Reference

```bash
cargo check                                          # compile check
cargo test --workspace --exclude canonical-test       # run tests (canonical-test needs libslirp)
cargo fmt --all                                      # format
cargo clippy --all-targets --all-features -- -D warnings  # lint
cargo test -p sequencer --lib                        # includes Anvil-backed tests (needs Foundry on PATH)
```

## Project Overview

Sequencer prototype for a DeFi rollup stack. Orders user operations into frames and batches, posts them to L1, and provides a real-time WebSocket feed of sequenced transactions. Currently backed by a dummy wallet app (Transfer, Withdrawal).

Rust edition 2024 / Axum API / SQLite (rusqlite, WAL) / EIP-712 signing / SSZ encoding.

## Workspace Layout

- `sequencer/` - main sequencer binary and library
- `sequencer-core/` - shared domain types (`Application`, `SignedUserOp`, `SequencedL2Tx`, batch/frame types)
- `examples/app-core/` - wallet app implementing the `Application` trait
- `examples/canonical-app/` - on-chain scheduler (needs libslirp to build)
- `examples/canonical-test/` - e2e test harness for canonical app (needs libslirp)
- `sdk/rust-client/` - Rust client library for the sequencer API
- `tests/benchmarks/` - benchmark harnesses
- `tests/e2e/` - end-to-end test infrastructure
- `tests/harness/` - shared test harness utilities

## Key Concepts

- **Chunk**: bounded list of user ops processed together to amortize SQLite cost
- **Frame**: ordering boundary committing a `safe_block` + user ops; scheduler drains direct inputs up to `safe_block` before executing the frame's ops
- **Batch**: list of frames posted on-chain as one L1 transaction
- **Inclusion lane**: single-lane hot-path loop that dequeues, executes, persists, and rotates frame/batch boundaries
- **Batch submitter**: stateless worker that assigns nonces, bulk-submits all pending batches to L1 each tick
- **Input reader**: ingests safe inputs from L1 InputBox into SQLite

## Storage Tables (Key Ones)

- `batches`, `frames`, `user_ops` - batch/frame/op structure
- `sequenced_l2_txs` - append-only ordered replay rows (auto-populated via trigger)
- `safe_inputs` - L1 direct input payloads
- `batch_nonces` - maps batch_index to submission nonce (assigned by batch submitter)
- `safe_accepted_batches` - derived log of batch submissions the scheduler would execute (frontier-based)
- `invalid_batches` - append-only table of invalidated batch indices (cascade semantics)
- `batch_policy` / `batch_policy_derived` - fee and sizing parameters

## Recovery Design

Preemptive recovery: the batch submitter detects when the frontier batch approaches the staleness deadline (danger zone). On detection it crashes, and the startup sequence flushes the L1 mempool, re-syncs the safe head, then runs the atomic recovery (cascade-invalidate stale batches, open recovery batch). If L1 is unreachable, the sequencer falls back to wall-clock estimation (`elapsed / seconds_per_block`) to decide whether to proceed or block. See `docs/recovery/` for the full design, TLA+ specs, and design history.

## Sequencer/Scheduler Duality

The sequencer (off-chain) and scheduler (on-chain) must agree on transaction ordering. The `safe_block` in each frame is the synchronization primitive - the scheduler drains direct inputs up to that block before executing user ops. Both sides produce identical execution order.

## Important Conventions

- Storage is append-oriented; avoid mutable status flags
- Open batch/frame derived by "latest row" convention
- Cursor pagination uses SQLite rowid, not count-based offsets
- `batch_index` (local, monotonic) is distinct from batch `nonce` (contiguous over valid batches)
- `MAX_WAIT_BLOCKS` (1200, ~4h) is shared between sequencer and scheduler in `sequencer-core`
- All queries over batch data filter out `invalid_batches`

## Environment Variables

Required: `SEQ_ETH_RPC_URL`, `SEQ_CHAIN_ID`, `SEQ_APP_ADDRESS`, `SEQ_BATCH_SUBMITTER_PRIVATE_KEY` (or `_FILE`)

Optional: `SEQ_HTTP_ADDR`, `SEQ_DATA_DIR`, `SEQ_LONG_BLOCK_RANGE_ERROR_CODES`, `SEQ_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`, `SEQ_BATCH_SUBMITTER_CONFIRMATION_DEPTH`, `SEQ_PREEMPTIVE_MARGIN_BLOCKS` (default: 75), `SEQ_SECONDS_PER_BLOCK` (default: 12)

## Detailed Agent Guidelines

See `AGENTS.md` for full architecture map, domain truths, hot-path invariants, type boundaries, coding conventions, testing guidance, and always/ask-first/never rules.
