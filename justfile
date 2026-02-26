set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

check:
    cargo check --workspace

check-all-targets:
    cargo check --workspace --all-targets

test:
    cargo test --workspace

test-sequencer:
    cargo test -p sequencer --lib --tests

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
    SEQ_HTTP_ADDR={{addr}} SEQ_DB_PATH={{db}} cargo run -p sequencer
