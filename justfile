set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

check:
    cargo check --workspace

check-all-targets:
    cargo check --workspace --all-targets

test:
    cargo test --workspace

# Run sequencer tests sequentially so partition static config (init) is not shared across parallel tests.
test-sequencer:
    cargo test -p sequencer --lib -- --test-threads=1
    cargo test -p sequencer --test e2e_sequencer -- --test-threads=1
    cargo test -p sequencer --test ws_broadcaster -- --test-threads=1
    cargo test -p sequencer --test batch_submitter_integration -- --test-threads=1

test-rollups-e2e: setup canonical-build-machine-image
    cargo build -p sequencer --bin sequencer-devnet
    cargo build -p rollups-e2e
    cargo run -p rollups-e2e

bench target="all":
    just -f tests/benchmarks/justfile {{target}}

setup:
    just -f examples/canonical-app/justfile download-deps
    just -f tests/benchmarks/justfile setup

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
    rm -f sequencer.db sequencer.db-shm sequencer.db-wal
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

run addr="127.0.0.1:3000" db="sequencer.db":
    rm -f {{db}} {{db}}-shm {{db}}-wal
    SEQ_HTTP_ADDR={{addr}} SEQ_DB_PATH={{db}} cargo run -p sequencer --release
