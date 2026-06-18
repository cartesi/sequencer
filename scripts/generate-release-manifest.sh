#!/usr/bin/env bash
# Emit release-manifest.json: ties sequencer, watchdog, CM images, and toolchain pins.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pins="${root}/toolchain-pins.env"

usage() {
  echo "usage: $0 --tag TAG [--git-sha SHA] [--output PATH]" >&2
  exit 1
}

tag=""
git_sha=""
output=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      tag="${2:-}"
      shift 2
      ;;
    --git-sha)
      git_sha="${2:-}"
      shift 2
      ;;
    --output)
      output="${2:-}"
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done

if [[ -z "${tag}" ]]; then
  usage
fi

if [[ -z "${git_sha}" ]]; then
  git_sha="$(git -C "${root}" rev-parse HEAD 2>/dev/null || echo unknown)"
fi

if [[ ! -f "${pins}" ]]; then
  echo "missing ${pins}" >&2
  exit 1
fi

# shellcheck disable=SC1090
set -a
source "${pins}"
set +a

artifacts_json="$(cat <<EOF
[
  {"name":"sequencer-${tag}-linux-amd64.tar.gz","kind":"sequencer","arch":"amd64"},
  {"name":"sequencer-${tag}-linux-arm64.tar.gz","kind":"sequencer","arch":"arm64"},
  {"name":"canonical-machine-image-devnet-${tag}.tar.gz","kind":"cm_image","chain":"devnet"},
  {"name":"canonical-machine-image-sepolia-${tag}.tar.gz","kind":"cm_image","chain":"sepolia"},
  {"name":"sequencer-watchdog-${tag}-linux-amd64.tar.gz","kind":"watchdog_image","arch":"amd64","format":"docker-save"},
  {"name":"sequencer-watchdog-${tag}-linux-arm64.tar.gz","kind":"watchdog_image","arch":"arm64","format":"docker-save"}
]
EOF
)"

emit_manifest() {
  python3 - "${tag}" "${git_sha}" "${artifacts_json}" <<'PY'
import json, os, sys

tag, git_sha, artifacts_json = sys.argv[1], sys.argv[2], sys.argv[3]
artifacts = json.loads(artifacts_json)

manifest = {
    "release_tag": tag,
    "git_commit": git_sha,
    "bundle_version": tag,
    "toolchain": {
        "rust": os.environ["RUST_TOOLCHAIN"],
        "xgenext2fs": os.environ["XGENEXT2FS_VERSION"],
        "cartesi_machine": os.environ["CARTESI_MACHINE_VERSION"],
    },
    "artifacts": artifacts,
    "alignment": {
        "rule": "All artifacts sharing release_tag were built from git_commit with the same toolchain pins.",
        "watchdog_cm_bootstrap": f"Use canonical-machine-image-<chain>-{tag}.tar.gz with WATCHDOG_CM_SNAPSHOT_DIR; cartesi-machine in the watchdog image matches CARTESI_MACHINE_VERSION.",
    },
}
json.dump(manifest, sys.stdout, indent=2)
sys.stdout.write("\n")
PY
}

if [[ -n "${output}" ]]; then
  emit_manifest > "${output}"
else
  emit_manifest
fi
