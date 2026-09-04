set shell := ["bash", "-euo", "pipefail", "-c"]

# Nested justfiles as modules: `just watchdog <r>`, `just canonical <r>`,
# `just bench <r>` (also `just <mod> --list`). The curated recipes below are
# top-level shortcuts for the common cross-cutting operations.
mod watchdog 'watchdog/justfile'
mod canonical 'examples/canonical-app/justfile'
mod bench 'tests/benchmarks/justfile'

# ── Devnet dependency pins ───────────────────────────────────────────
# The pre-deployed Anvil state published by the rollups-contracts release,
# plus the forge-built MockERC20 fixture. Consumed by tests/harness — and
# through it e2e, benchmarks, and the watchdog compare harness — so the
# pins and the setup recipe live here at the root, not in any one consumer.
rollups_contracts_version := "3.0.0-alpha.6"
anvil_dump_name := "rollups-contracts-" + rollups_contracts_version + "-anvil-v1.4.3"
anvil_dump_dir := "tests/.deps/" + anvil_dump_name
anvil_dump_tar := "tests/.deps/" + anvil_dump_name + ".tar.gz"
anvil_dump_url := "https://github.com/cartesi/rollups-contracts/releases/download/v" + rollups_contracts_version + "/" + anvil_dump_name + ".tar.gz"
root_anvil_dump_tar := anvil_dump_name + ".tar.gz"
# The release publishes no checksum file, so the hash is pinned here
# (trust-on-first-use, same model as toolchain-pins.env) to catch a
# replaced/corrupted release asset.
anvil_dump_sha256 := "b140e31db2b04bb99c733fdf153718cd252335370f4b355849e2cbb3121fc30f"

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

# Rollups-contracts anvil dump (SHA256-verified) + tests/contracts fixtures.
setup-devnet-deps:
    @mkdir -p tests/.deps
    @if [[ ! -f {{anvil_dump_tar}} ]]; then if [[ -f {{root_anvil_dump_tar}} ]]; then cp {{root_anvil_dump_tar}} {{anvil_dump_tar}}; else wget {{anvil_dump_url}} -O {{anvil_dump_tar}}; fi; fi
    @echo "{{anvil_dump_sha256}}  {{anvil_dump_tar}}" | shasum -a 256 -c - >/dev/null || { echo "SHA256 mismatch for {{anvil_dump_tar}} — delete it and re-run 'just setup'"; exit 1; }
    @if [[ ! -f {{anvil_dump_dir}}/state.json ]]; then rm -rf {{anvil_dump_dir}}; mkdir -p {{anvil_dump_dir}}; tar -xzf {{anvil_dump_tar}} -C {{anvil_dump_dir}}; fi
    forge build --root tests/contracts

setup:
    just canonical download-deps
    just setup-devnet-deps
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
    rm -rf tests/.deps
    rm -rf tests/contracts/out tests/contracts/cache
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

# Genesis for the C-API wallet. Any C application starts with its own genesis tool, then
# `c-wallet-sequencer --state-file <state> setup|run`, which needs a deployed app and an L1.
c-wallet-genesis state="c-wallet-genesis-state" preset="devnet":
    rm -rf {{state}}
    cargo run -p c-wallet-engine --bin c-wallet-genesis -- {{state}} {{preset}}
