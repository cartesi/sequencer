#!/usr/bin/env bash
# Fail when an in-tree copy of a toolchain-pins.env version drifts. Files that
# can read the pins (workflows, justfiles) do; this checks those that cannot:
# Cargo.toml, Rust constants, Dockerfile ARG defaults and vendoring provenance.
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

[[ "${CARTESI_LINUX_IMAGE_RELEASE}" == "${CARTESI_MACHINE_VERSION}" ]] \
  || fail "CARTESI_LINUX_IMAGE_RELEASE must equal CARTESI_MACHINE_VERSION"
[[ "${CARTESI_LINUX_KERNEL_FILENAME}" == *"${CARTESI_LINUX_IMAGE_RELEASE}"* ]] \
  || fail "CARTESI_LINUX_KERNEL_FILENAME must name ${CARTESI_LINUX_IMAGE_RELEASE}"
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

if ((errors > 0)); then exit 1; fi
echo "toolchain pins aligned"
