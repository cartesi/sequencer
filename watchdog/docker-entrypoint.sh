#!/usr/bin/env bash
# Wrapper: run watchdog subcommands with image-bundled Lua paths.
set -euo pipefail

if [[ "${WATCHDOG_PRINT_RELEASE_INFO:-0}" == "1" ]]; then
  cat /opt/watchdog/RELEASE.json || true
  cartesi-machine --version 2>&1 | sed 's/^/cartesi-machine: /' || true
  exit 0
fi

cd /opt/watchdog/lua

if [[ "$#" -gt 0 && ( "$1" == "init" || "$1" == "tick" ) ]]; then
  : "${WATCHDOG_STATE_DIR:?WATCHDOG_STATE_DIR is required}"
  mkdir -p "$WATCHDOG_STATE_DIR"
  exec flock -n "$WATCHDOG_STATE_DIR/run.lock" lua5.4 watchdog/main.lua "$@"
fi

exec lua5.4 watchdog/main.lua "$@"
