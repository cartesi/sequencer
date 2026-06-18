#!/usr/bin/env bash
# Build the watchdog OCI image and smoke-test cartesi-machine + cartesi Lua module.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

# shellcheck disable=SC1091
source toolchain-pins.env

image="sequencer-watchdog:ci-smoke"
if command -v dpkg >/dev/null 2>&1; then
  arch="$(dpkg --print-architecture)"
else
  case "$(uname -m)" in
    x86_64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *)
      echo "unsupported arch for watchdog docker smoke: $(uname -m)" >&2
      exit 1
      ;;
  esac
fi
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
  -f watchdog/Dockerfile \
  -t "${image}" \
  .

docker run --rm -e WATCHDOG_PRINT_RELEASE_INFO=1 "${image}" >/dev/null
docker run --rm --entrypoint cartesi-machine "${image}" --version >/dev/null
docker run --rm --entrypoint lua5.4 "${image}" -e "require('cartesi'); print('cartesi ok')"
# Validate the vendored lcurl build loads in the runtime image.
docker run --rm --entrypoint lua5.4 "${image}" -e "require('lcurl'); print('lcurl ok')"

echo "watchdog docker smoke ok"
