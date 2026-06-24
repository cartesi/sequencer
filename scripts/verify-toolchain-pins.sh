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

fail() {
  echo "$1" >&2
  errors=$((errors + 1))
}

rust_channel="$(
  grep -E '^\s*channel\s*=' "${root}/rust-toolchain.toml" \
    | head -1 \
    | sed -E 's/.*=[[:space:]]*"([^"]+)".*/\1/'
)"
if [[ "${rust_channel}" != "${RUST_TOOLCHAIN}" ]]; then
  fail "rust-toolchain.toml channel=${rust_channel} != RUST_TOOLCHAIN=${RUST_TOOLCHAIN}"
fi

if [[ "${CARTESI_LINUX_IMAGE_RELEASE}" != "${CARTESI_MACHINE_VERSION}" ]]; then
  fail "CARTESI_LINUX_IMAGE_RELEASE=${CARTESI_LINUX_IMAGE_RELEASE} != CARTESI_MACHINE_VERSION=${CARTESI_MACHINE_VERSION}"
fi

expected_assert_version="${CARTESI_MACHINE_VERSION#v}"
if [[ "${CARTESI_LINUX_KERNEL_FILENAME}" != *"${CARTESI_LINUX_IMAGE_RELEASE}"* ]]; then
  fail "CARTESI_LINUX_KERNEL_FILENAME=${CARTESI_LINUX_KERNEL_FILENAME} does not include ${CARTESI_LINUX_IMAGE_RELEASE}"
fi

canonical_just="${root}/examples/canonical-app/justfile"
if [[ ! -f "${canonical_just}" ]]; then
  fail "missing ${canonical_just}"
else
  resolved_assert_version="$(
    sed -n 's/^CARTESI_MACHINE_VERSION=v//p' "${pins}"
  )"
  resolved_linux_release="$(
    grep '^CARTESI_LINUX_IMAGE_RELEASE=' "${pins}" | cut -d= -f2
  )"
  resolved_kernel_filename="$(
    grep '^CARTESI_LINUX_KERNEL_FILENAME=' "${pins}" | cut -d= -f2
  )"

  if ! grep -q 'pins := "../../toolchain-pins.env"' "${canonical_just}"; then
    fail "examples/canonical-app/justfile must read ../../toolchain-pins.env"
  fi

  if [[ -z "${resolved_assert_version}" ]]; then
    fail "could not resolve cartesi_machine_version from ${pins}"
  elif [[ "${resolved_assert_version}" != "${expected_assert_version}" ]]; then
    fail "resolved cartesi_machine_version=${resolved_assert_version} != ${expected_assert_version}"
  fi

  if [[ "${resolved_linux_release}" != "${CARTESI_LINUX_IMAGE_RELEASE}" ]]; then
    fail "resolved linux_image_release=${resolved_linux_release} != ${CARTESI_LINUX_IMAGE_RELEASE}"
  fi

  if [[ "${resolved_kernel_filename}" != "${CARTESI_LINUX_KERNEL_FILENAME}" ]]; then
    fail "resolved linux_kernel_filename=${resolved_kernel_filename} != ${CARTESI_LINUX_KERNEL_FILENAME}"
  fi
fi

if [[ "${errors}" -ne 0 ]]; then
  exit 1
fi

echo "toolchain pins aligned"
