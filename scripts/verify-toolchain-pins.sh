#!/usr/bin/env bash
# Fail when an in-tree copy of a toolchain-pins.env version drifts. Files that
# can read the pins (workflows, justfiles) do; this checks those that cannot:
# Cargo.toml, Rust constants, Dockerfile ARG defaults and vendoring provenance.
#
# `--upstream`, run before each release, also checks the pins that pair with
# the emulator: the kernel release and the guest tools must be the LINUX_TAG
# and MACHINE_GUEST_TOOLS_TAG of the emulator's dependencies.lock at
# CARTESI_MACHINE_VERSION, and the kernel's SHA-512 the one its release
# publishes. Nothing in-tree can check that pairing, since cartesi/linux tags
# name the kernel version. Needs curl and the network.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pins="${root}/toolchain-pins.env"

# load-toolchain-pins appends KEY=value lines to $GITHUB_ENV verbatim.
if grep -nvE '^(#.*|[A-Z][A-Z0-9_]*=[^[:space:]#]+)?$' "${pins}"; then
  echo "toolchain-pins.env: the lines above are not KEY=value" >&2
  exit 1
fi
# shellcheck disable=SC1090
source "${pins}"

errors=0
fail() { echo "$1" >&2; errors=$((errors + 1)); }
# require FILE TEXT: the repo-relative FILE must contain TEXT verbatim.
require() { grep -qF -- "$2" "${root}/$1" || fail "$1 does not contain: $2"; }

# A cartesi/linux release names its assets after its tag.
[[ "${CARTESI_LINUX_KERNEL_FILENAME}" == "linux-${CARTESI_LINUX_RELEASE#v}.bin" ]] \
  || fail "CARTESI_LINUX_KERNEL_FILENAME must be linux-${CARTESI_LINUX_RELEASE#v}.bin"
[[ "${CARTESI_LINUX_KERNEL_SHA512}" =~ ^[0-9a-f]{128}$ ]] \
  || fail "CARTESI_LINUX_KERNEL_SHA512 must be 128 lowercase hex characters"

require Cargo.toml "cartesi-rollups-contracts = \"=${ROLLUPS_CONTRACTS_VERSION}\""
require tests/harness/src/paths.rs \
  "cartesi-rollups-contracts-${ROLLUPS_CONTRACTS_VERSION}-anvil-${FOUNDRY_VERSION#v}\""

for dockerfile in examples/canonical-app/Dockerfile watchdog/test-guest/Dockerfile; do
  require "${dockerfile}" "ARG MACHINE_EMULATOR_TOOLS_VERSION=${MACHINE_GUEST_TOOLS_VERSION}"
done
tools_sha="$(grep '^ARG TOOLS_SHA512=' "${root}/examples/canonical-app/Dockerfile")"
require watchdog/test-guest/Dockerfile "${tools_sha}"
require sdk/guest/README.md "refs/tags/v${MACHINE_GUEST_TOOLS_VERSION}.tar.gz"
require sdk/guest/libcmt-sys/Cargo.toml "machine-guest-tools v${MACHINE_GUEST_TOOLS_VERSION}"

if [[ "${1:-}" == --upstream ]]; then
  lock="$(curl -fsSL "https://raw.githubusercontent.com/cartesi/machine-emulator/${CARTESI_MACHINE_VERSION}/dependencies.lock")"
  linux_tag="$(sed -n 's/^LINUX_TAG?=//p' <<<"${lock}")"
  tools_tag="$(sed -n 's/^MACHINE_GUEST_TOOLS_TAG?=//p' <<<"${lock}")"
  [[ "${linux_tag}" == "${CARTESI_LINUX_RELEASE}" ]] \
    || fail "CARTESI_LINUX_RELEASE is ${CARTESI_LINUX_RELEASE}; emulator ${CARTESI_MACHINE_VERSION} pairs ${linux_tag:-nothing}"
  [[ "${tools_tag}" == "v${MACHINE_GUEST_TOOLS_VERSION}" ]] \
    || fail "MACHINE_GUEST_TOOLS_VERSION is ${MACHINE_GUEST_TOOLS_VERSION}; emulator ${CARTESI_MACHINE_VERSION} pairs ${tools_tag:-nothing}"
  if ! published="$(curl -fsSL "https://github.com/cartesi/linux/releases/download/${CARTESI_LINUX_RELEASE}/${CARTESI_LINUX_KERNEL_FILENAME}.sha512")"; then
    fail "cannot fetch ${CARTESI_LINUX_KERNEL_FILENAME}.sha512 from cartesi/linux ${CARTESI_LINUX_RELEASE}"
  elif [[ "${published%% *}" != "${CARTESI_LINUX_KERNEL_SHA512}" ]]; then
    fail "CARTESI_LINUX_KERNEL_SHA512 differs from the one ${CARTESI_LINUX_RELEASE} publishes"
  fi
fi

if ((errors > 0)); then exit 1; fi
echo "toolchain pins aligned"
