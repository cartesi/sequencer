#!/usr/bin/env bash
# Link an external engine into the generic host and an independent Cargo consumer.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"
export CARGO_TARGET_DIR="${root}/target"

unset APPLICATION_ENGINE_LIB APPLICATION_ENGINE_HEADER
cargo build --locked -p c-wallet-engine
export APPLICATION_ENGINE_LIB="${CARGO_TARGET_DIR}/debug/libc_wallet_engine.a"
export APPLICATION_ENGINE_HEADER="${root}/bindings/c-app-engine/include/application-engine.h"
cargo build --locked -p c-app-sequencer
"${CARGO_TARGET_DIR}/debug/c-app-sequencer" --help >/dev/null

smoke_dir="$(mktemp -d "${TMPDIR:-/tmp}/sequencer-c-consumer.XXXXXX")"
trap 'rm -rf "${smoke_dir}"' EXIT
ln -s "${root}" "${smoke_dir}/source"
consumer_dir="${smoke_dir}/consumer"
mkdir -p "${consumer_dir}/src"
cat >"${consumer_dir}/Cargo.toml" <<'TOML'
[package]
name = "c-application-consumer-smoke"
version = "0.0.0"
edition = "2024"

[workspace]

[dependencies]
c-app-sequencer = { path = "../source/bindings/c-app-sequencer" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
TOML
cat >"${consumer_dir}/src/main.rs" <<'RUST'
#[tokio::main]
async fn main() -> std::process::ExitCode {
    c_app_sequencer::run().await
}
RUST

# Reuse CI's pinned dependencies, allowing Cargo to adjust only the consumer's lockfile.
cp Cargo.lock "${consumer_dir}/Cargo.lock"
cargo build --manifest-path "${consumer_dir}/Cargo.toml" --offline
"${CARGO_TARGET_DIR}/debug/c-application-consumer-smoke" --help >/dev/null
echo "C application archive and downstream consumer smoke passed"
