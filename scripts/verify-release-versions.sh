#!/usr/bin/env bash
# Fail if release/versions.env drifts from other pinned artifacts in-tree.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
versions="${root}/release/versions.env"

if [[ ! -f "${versions}" ]]; then
  echo "missing ${versions}" >&2
  exit 1
fi

# shellcheck disable=SC1090
set -a
source "${versions}"
set +a

errors=0

rust_channel="$(
  grep -E '^\s*channel\s*=' "${root}/rust-toolchain.toml" \
    | head -1 \
    | sed -E 's/.*=\s*"([^"]+)".*/\1/'
)"
if [[ "${rust_channel}" != "${RUST_TOOLCHAIN}" ]]; then
  echo "rust-toolchain.toml channel=${rust_channel} != RUST_TOOLCHAIN=${RUST_TOOLCHAIN}" >&2
  errors=$((errors + 1))
fi

upstream_file="${root}/watchdog/third_party/lua-curl/UPSTREAM"
if [[ -f "${upstream_file}" ]]; then
  upstream_sha="$(grep -E '^Commit:' "${upstream_file}" | awk '{print $2}')"
  if [[ "${upstream_sha}" != "${LUA_CURL_UPSTREAM_SHA}" ]]; then
    echo "lua-curl UPSTREAM commit=${upstream_sha} != LUA_CURL_UPSTREAM_SHA=${LUA_CURL_UPSTREAM_SHA}" >&2
    errors=$((errors + 1))
  fi
else
  echo "missing ${upstream_file}" >&2
  errors=$((errors + 1))
fi

if [[ "${errors}" -ne 0 ]]; then
  exit 1
fi

echo "release version pins aligned"
