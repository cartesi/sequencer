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
docker run --rm --entrypoint flock "${image}" --version >/dev/null
docker run --rm --entrypoint lua5.4 "${image}" -e "require('cartesi'); print('cartesi ok')"
# Validate the vendored lcurl build loads in the runtime image.
docker run --rm --entrypoint lua5.4 "${image}" -e "require('lcurl'); print('lcurl ok')"

state_dir="$(mktemp -d)"
lock_container=""
cleanup() {
  if [[ -n "${lock_container}" ]]; then
    docker rm -f "${lock_container}" >/dev/null 2>&1 || true
  fi
  rm -rf "${state_dir}"
}
trap cleanup EXIT

set +e
tick_output="$(
  docker run --rm \
    -e WATCHDOG_STATE_DIR=/state \
    -v "${state_dir}:/state" \
    "${image}" tick 2>&1
)"
tick_status=$?
set -e
if [[ "${tick_status}" -ne 1 || "${tick_output}" != *"failed to load config.json"* ]]; then
  echo "expected unlocked tick to reach Lua and fail on missing config.json" >&2
  echo "status=${tick_status}" >&2
  echo "${tick_output}" >&2
  exit 1
fi

lock_container="sequencer-watchdog-lock-smoke-$$"
docker run -d \
  --name "${lock_container}" \
  -v "${state_dir}:/state" \
  --entrypoint sh \
  "${image}" \
  -c 'flock /state/run.lock sleep 30' >/dev/null

locked=""
for _ in $(seq 1 30); do
  set +e
  lock_output="$(
    docker run --rm \
      -e WATCHDOG_STATE_DIR=/state \
      -v "${state_dir}:/state" \
      "${image}" tick 2>&1
  )"
  lock_status=$?
  set -e
  if [[ "${lock_status}" -eq 1 && "${lock_output}" == *"watchdog state is already locked"* ]]; then
    locked=1
    break
  fi
  sleep 0.2
done
if [[ -z "${locked}" ]]; then
  echo "expected tick to fail on held watchdog lock" >&2
  echo "last status=${lock_status}" >&2
  echo "${lock_output}" >&2
  exit 1
fi

echo "watchdog docker smoke ok"
