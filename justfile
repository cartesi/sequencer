set shell := ["bash", "-euo", "pipefail", "-c"]

# Nested justfiles as modules: `just watchdog <r>`, `just canonical <r>`,
# `just bench <r>` (also `just <mod> --list`). The curated recipes below are
# top-level shortcuts for the common cross-cutting operations.
mod watchdog 'watchdog/justfile'
mod canonical 'examples/canonical-app/justfile'
mod bench 'tests/benchmarks/justfile'

default:
    @just --list

check:
    cargo check --workspace

check-all-targets:
    cargo check --workspace --all-targets

test:
    cargo test --workspace

test-watchdog:
    just watchdog test

test-watchdog-e2e:
    just watchdog test-e2e

# Verify divergence signal via main.lua (drill exits 2 like production).
test-watchdog-divergence-drill: watchdog-lua-deps
    @just watchdog test-divergence-drill

# Build lcurl (lua-cURLv3) into .deps/lua; JSON is pure Lua under watchdog/third_party/.
watchdog-lua-deps:
    @just watchdog lua-deps

# Anvil + rollups + wallet-sequencer-devnet; prints CARTESI_WATCHDOG_* exports until Ctrl+C.
devnet-for-watchdog: setup ensure-machine-image
    cargo build -p wallet-sequencer --bin wallet-sequencer-devnet
    cargo build -p rollups-e2e --bin devnet-stack
    cargo run -p rollups-e2e --bin devnet-stack

test-watchdog-compare-harness: setup watchdog-lua-deps ensure-machine-image
    cargo build -p wallet-sequencer --bin wallet-sequencer-devnet -p rollups-e2e --bin rollups-e2e
    cargo run -p rollups-e2e --bin rollups-e2e -- watchdog_genesis_compare_test --exact --nocapture

# Run sequencer tests sequentially so partition static config (init) is not shared across parallel tests.
test-sequencer:
    cargo test -p sequencer --lib -- --test-threads=1
    cargo test -p sequencer --test e2e_sequencer -- --test-threads=1
    cargo test -p sequencer --test ws_broadcaster -- --test-threads=1
    cargo test -p sequencer --test batch_submitter_integration -- --test-threads=1

test-rollups-e2e: setup ensure-machine-image ensure-sepolia-machine-image
    just watchdog-lua-deps
    cargo build -p wallet-sequencer --bin wallet-sequencer-devnet -p rollups-e2e --bin rollups-e2e
    cargo run -p rollups-e2e --bin rollups-e2e

ensure-machine-image:
    @test -d examples/canonical-app/out/canonical-machine-image || just canonical build-machine-image

ensure-sepolia-machine-image:
    @test -d examples/canonical-app/out/canonical-machine-image-sepolia || just canonical build-machine-image-sepolia

setup:
    just canonical download-deps
    just bench setup
    just watchdog-lua-deps

doctor:
    just watchdog doctor

canonical-build-machine-image:
    just canonical build-machine-image

canonical-build-machine-image-sepolia:
    just canonical build-machine-image-sepolia

canonical-test-guest:
    just canonical test-guest

canonical-print-build-hashes:
    just canonical print-build-hashes

clean:
    cargo clean
    rm -rf sequencer-data
    just canonical clean
    just bench clean

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

verify: fmt-check check test clippy

ci:
    cargo check --workspace --all-targets --locked
    cargo build --workspace --all-targets --locked
    cargo fmt --all -- --check
    cargo test --workspace --all-targets --all-features --locked

run addr="127.0.0.1:3000" data_dir="sequencer-data":
    rm -rf {{data_dir}}
    CARTESI_SEQUENCER_HTTP_ADDR={{addr}} CARTESI_SEQUENCER_DATA_DIR={{data_dir}} cargo run -p wallet-sequencer --release
