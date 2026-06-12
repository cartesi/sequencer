#!/usr/bin/env bash
# Run the divergence drill; recipe success requires exit 2 (production divergence code).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export WATCHDOG_LUA_DEPS="${WATCHDOG_LUA_DEPS:-${root}/.deps/lua}"

set +e
lua "${root}/watchdog/tests/drill_divergence.lua"
code=$?
set -e

if [[ "${code}" -eq 0 ]]; then
    echo "expected drill exit 2, got 0" >&2
    exit 1
fi
if [[ "${code}" -ne 2 ]]; then
    echo "expected drill exit 2, got ${code}" >&2
    exit 1
fi
