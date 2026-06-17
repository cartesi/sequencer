set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

check:
    cargo check --workspace

check-all-targets:
    cargo check --workspace --all-targets

test:
    cargo test --workspace

test-watchdog:
    just -f watchdog/justfile test

test-watchdog-e2e:
    just -f watchdog/justfile test-e2e

# Verify divergence signal via main.lua (drill exits 2 like production).
test-watchdog-divergence-drill: watchdog-lua-deps
    @just -f watchdog/justfile test-divergence-drill

# Build lcurl (lua-cURLv3) into .deps/lua; JSON is pure Lua under watchdog/third_party/.
watchdog-lua-deps:
    @just -f watchdog/justfile lua-deps

# Anvil + rollups + sequencer-devnet; prints WATCHDOG_* exports until Ctrl+C.
devnet-for-watchdog: setup ensure-machine-image
    cargo build -p sequencer --bin sequencer-devnet
    cargo build -p rollups-e2e --bin devnet-stack
    cargo run -p rollups-e2e --bin devnet-stack

test-watchdog-compare-harness: setup watchdog-lua-deps ensure-machine-image
    cargo build -p sequencer --bin sequencer-devnet -p rollups-e2e --bin rollups-e2e
    cargo run -p rollups-e2e --bin rollups-e2e -- watchdog_genesis_compare_test --exact --nocapture

# Run sequencer tests sequentially so partition static config (init) is not shared across parallel tests.
test-sequencer:
    cargo test -p sequencer --lib -- --test-threads=1
    cargo test -p sequencer --test e2e_sequencer -- --test-threads=1
    cargo test -p sequencer --test ws_broadcaster -- --test-threads=1
    cargo test -p sequencer --test batch_submitter_integration -- --test-threads=1

test-rollups-e2e: setup watchdog-lua-deps ensure-machine-image
    cargo build -p sequencer --bin sequencer-devnet -p rollups-e2e --bin rollups-e2e
    cargo run -p rollups-e2e --bin rollups-e2e

ensure-machine-image:
    @test -d examples/canonical-app/out/canonical-machine-image || just canonical-build-machine-image

bench target="all":
    just -f tests/benchmarks/justfile {{target}}

setup:
    just -f examples/canonical-app/justfile download-deps
    just -f tests/benchmarks/justfile setup
    just watchdog-lua-deps

doctor:
    just -f watchdog/justfile doctor

canonical-build-machine-image:
    just -f examples/canonical-app/justfile build-machine-image

canonical-build-machine-image-sepolia:
    just -f examples/canonical-app/justfile build-machine-image-sepolia

canonical-test-guest:
    just -f examples/canonical-app/justfile test-guest

canonical-print-build-hashes:
    just -f examples/canonical-app/justfile print-build-hashes

clean:
    cargo clean
    rm -rf sequencer-data
    just -f examples/canonical-app/justfile clean
    just -f tests/benchmarks/justfile clean

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
    SEQ_HTTP_ADDR={{addr}} SEQ_DATA_DIR={{data_dir}} cargo run -p sequencer --release
