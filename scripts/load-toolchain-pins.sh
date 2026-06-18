#!/usr/bin/env bash
# Parse toolchain-pins.env and append KEY=value lines to a GitHub env file.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pins="${TOOLCHAIN_PINS_FILE:-${root}/toolchain-pins.env}"
output="${1:-${GITHUB_ENV:-}}"

usage() {
  echo "usage: $0 OUTPUT_FILE" >&2
  echo "       $0 -            # print parsed KEY=value lines to stdout" >&2
  exit 1
}

trim() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "${value}"
}

emit() {
  if [[ "${output}" == "-" ]]; then
    printf '%s\n' "$1"
  else
    printf '%s\n' "$1" >> "${output}"
  fi
}

if [[ -z "${output}" ]]; then
  usage
fi
if [[ ! -f "${pins}" ]]; then
  echo "missing ${pins}" >&2
  exit 1
fi

while IFS= read -r line || [[ -n "${line}" ]]; do
  line="${line%%#*}"
  line="$(trim "${line}")"
  if [[ -z "${line}" ]]; then
    continue
  fi
  if [[ ! "${line}" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; then
    echo "invalid line in ${pins}: ${line}" >&2
    exit 1
  fi
  emit "${line}"
done < "${pins}"
