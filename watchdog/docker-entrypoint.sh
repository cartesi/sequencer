#!/usr/bin/env bash
# Wrapper: run compare/advance loop with image-bundled Lua paths.
set -euo pipefail

if [[ "${WATCHDOG_PRINT_RELEASE_INFO:-0}" == "1" ]]; then
  cat /opt/watchdog/RELEASE.json
  cartesi-machine --version 2>&1 | sed 's/^/cartesi-machine: /'
fi

cd /opt/watchdog/lua
exec lua watchdog/main.lua "$@"
