#!/usr/bin/env bash
set -euo pipefail

# Probe expected nonce for one funded account without consuming nonce.
# It sends one intentionally invalid-nonce tx (nonce=u32::MAX) and parses the
# rejection detail: "bad nonce: expected X, got 4294967295".

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

ENDPOINT="${ENDPOINT:-https://eth-sepolia.rollups.cartesi.io/v2}"
DOMAIN_CHAIN_ID="${DOMAIN_CHAIN_ID:-11155111}"
DOMAIN_VERIFYING_CONTRACT="${DOMAIN_VERIFYING_CONTRACT:-}"
ACCOUNTS_FILE="${ACCOUNTS_FILE:-}"
ACCOUNT_INDEX="${ACCOUNT_INDEX:-1}" # 1-based index in accounts file
REQUEST_TIMEOUT_MS="${REQUEST_TIMEOUT_MS:-5000}"
WORKLOAD="${WORKLOAD:-funded-transfer}"
TRANSFER_AMOUNT="${TRANSFER_AMOUNT:-1}"
MAX_FEE="${MAX_FEE:-0}"

if [[ -z "$DOMAIN_VERIFYING_CONTRACT" ]]; then
  echo "Missing DOMAIN_VERIFYING_CONTRACT" >&2
  exit 1
fi
if [[ -z "$ACCOUNTS_FILE" ]]; then
  echo "Missing ACCOUNTS_FILE" >&2
  exit 1
fi
if [[ ! -f "$ACCOUNTS_FILE" ]]; then
  echo "accounts file not found: $ACCOUNTS_FILE" >&2
  exit 1
fi

line="$(awk -v idx="$ACCOUNT_INDEX" 'NF>0 {n++; if (n==idx) {print; exit}}' "$ACCOUNTS_FILE")"
if [[ -z "$line" ]]; then
  echo "No non-empty account line at ACCOUNT_INDEX=$ACCOUNT_INDEX in $ACCOUNTS_FILE" >&2
  exit 1
fi

tmp_accounts="$(mktemp)"
trap 'rm -f "$tmp_accounts"' EXIT
printf '%s\n' "$line" > "$tmp_accounts"

set +e
probe_output="$(cargo run -p benchmarks --bin ack_latency -- \
  --endpoint "$ENDPOINT" \
  --domain-chain-id "$DOMAIN_CHAIN_ID" \
  --domain-verifying-contract "$DOMAIN_VERIFYING_CONTRACT" \
  --workload "$WORKLOAD" \
  --accounts-file "$tmp_accounts" \
  --transfer-amount "$TRANSFER_AMOUNT" \
  --count 1 \
  --concurrency 1 \
  --max-fee "$MAX_FEE" \
  --request-timeout-ms "$REQUEST_TIMEOUT_MS" \
  --starting-nonce 4294967295 \
  2>&1)"
status=$?
set -e

if [[ $status -eq 0 ]]; then
  echo "Probe unexpectedly succeeded; expected an invalid-nonce rejection." >&2
  exit 1
fi

expected="$(printf '%s\n' "$probe_output" | sed -n 's/.*bad nonce: expected \([0-9]\+\), got .*/\1/p' | tail -n 1)"
if [[ -z "$expected" ]]; then
  echo "Probe did not return a bad-nonce rejection. Got:" >&2
  echo "$probe_output" >&2
  exit 1
fi

echo "Detected expected nonce: $expected"
echo "Use:"
echo "  STARTING_NONCE=$expected bash scripts/live_sequencer_demo.sh"
