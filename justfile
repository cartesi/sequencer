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

bench target="all":
    just -f benchmarks/justfile {{target}}

setup:
    just -f examples/canonical-app/justfile download-deps
    just -f benchmarks/justfile setup

canonical-build-machine-image:
    just -f examples/canonical-app/justfile build-machine-image

canonical-test-guest:
    just -f examples/canonical-app/justfile test-guest

clean:
    cargo clean
    rm -f sequencer.db sequencer.db-shm sequencer.db-wal
    just -f examples/canonical-app/justfile clean
    just -f benchmarks/justfile clean

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
