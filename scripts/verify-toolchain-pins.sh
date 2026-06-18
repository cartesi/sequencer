#!/usr/bin/env bash
# Fail if toolchain-pins.env drifts from other pinned artifacts in-tree.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pins="${root}/toolchain-pins.env"

if [[ ! -f "${pins}" ]]; then
  echo "missing ${pins}" >&2
  exit 1
fi

# shellcheck disable=SC1090
set -a
source "${pins}"
set +a

errors=0

rust_channel="$(
  grep -E '^\s*channel\s*=' "${root}/rust-toolchain.toml" \
    | head -1 \
    | sed -E 's/.*=[[:space:]]*"([^"]+)".*/\1/'
)"
if [[ "${rust_channel}" != "${RUST_TOOLCHAIN}" ]]; then
  echo "rust-toolchain.toml channel=${rust_channel} != RUST_TOOLCHAIN=${RUST_TOOLCHAIN}" >&2
  errors=$((errors + 1))
fi

if [[ "${errors}" -ne 0 ]]; then
  exit 1
fi

echo "toolchain pins aligned"
