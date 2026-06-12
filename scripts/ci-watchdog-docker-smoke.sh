#!/usr/bin/env bash
# Build the watchdog OCI image and smoke-test cartesi-machine + cartesi Lua module.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

# shellcheck disable=SC1091
source release/versions.env

image="sequencer-watchdog:ci-smoke"
arch="$(dpkg --print-architecture)"
case "${arch}" in
  amd64) deb_sha="${CARTESI_MACHINE_SHA256_AMD64}" ;;
  arm64) deb_sha="${CARTESI_MACHINE_SHA256_ARM64}" ;;
  *)
    echo "unsupported arch for watchdog docker smoke: ${arch}" >&2
    exit 1
    ;;
esac

docker build \
  --build-arg "RELEASE_TAG=ci-smoke" \
  --build-arg "GIT_COMMIT=local" \
  --build-arg "CARTESI_MACHINE_VERSION=${CARTESI_MACHINE_VERSION}" \
  --build-arg "CARTESI_MACHINE_DEB_SHA256=${deb_sha}" \
  --build-arg "LUA_CURL_UPSTREAM_SHA=${LUA_CURL_UPSTREAM_SHA}" \
  -f watchdog/Dockerfile \
  -t "${image}" \
  .

docker run --rm -e WATCHDOG_PRINT_RELEASE_INFO=1 "${image}" >/dev/null
docker run --rm --entrypoint cartesi-machine "${image}" --version >/dev/null
docker run --rm --entrypoint lua5.4 "${image}" -e "require('cartesi'); print('cartesi ok')"

echo "watchdog docker smoke ok"
