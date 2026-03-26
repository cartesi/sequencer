#!/usr/bin/env bash
set -euo pipefail

# Run full live demo flow for one account index:
# 1) probe current expected nonce
# 2) run demo with STARTING_NONCE set automatically
#
# Usage:
#   bash scripts/live_demo_run_account.sh 2

if [[ $# -ne 1 ]]; then
  echo "Usage: bash scripts/live_demo_run_account.sh <account_index>" >&2
  exit 1
fi

ACCOUNT_INDEX="$1"
if ! [[ "$ACCOUNT_INDEX" =~ ^[0-9]+$ ]] || [[ "$ACCOUNT_INDEX" -lt 1 ]]; then
  echo "account_index must be a positive integer" >&2
  exit 1
fi

# Built-in defaults so this can run without exporting vars.
ENDPOINT="${ENDPOINT:-https://eth-sepolia.rollups.cartesi.io/v2}"
DOMAIN_CHAIN_ID="${DOMAIN_CHAIN_ID:-11155111}"
DOMAIN_VERIFYING_CONTRACT="${DOMAIN_VERIFYING_CONTRACT:-0x38d59b09c055D7485dc13AEC4d98b9436291B3e8}"
ACCOUNTS_FILE="${ACCOUNTS_FILE:-benchmarks/sepolia-demo.txt}"
WORKLOAD="${WORKLOAD:-funded-transfer}"
COUNT="${COUNT:-50}"
CONCURRENCY="${CONCURRENCY:-4}"
MAX_FEE="${MAX_FEE:-0}"
RUN_ROUND_TRIP="${RUN_ROUND_TRIP:-true}"
TRANSFER_AMOUNT="${TRANSFER_AMOUNT:-1}"
REQUEST_TIMEOUT_MS="${REQUEST_TIMEOUT_MS:-5000}"
MAX_WS_WAIT_MS="${MAX_WS_WAIT_MS:-10000}"

if [[ ! -f "$ACCOUNTS_FILE" ]]; then
  echo "accounts file not found: $ACCOUNTS_FILE" >&2
  exit 1
fi

selected_line="$(awk -v idx="$ACCOUNT_INDEX" 'NF>0 {n++; if (n==idx) {print; exit}}' "$ACCOUNTS_FILE")"
if [[ -z "$selected_line" ]]; then
  echo "No non-empty account line at ACCOUNT_INDEX=$ACCOUNT_INDEX in $ACCOUNTS_FILE" >&2
  exit 1
fi

tmp_accounts="$(mktemp)"
trap 'rm -f "$tmp_accounts"' EXIT
printf '%s\n' "$selected_line" > "$tmp_accounts"

echo "=== Probing nonce for account index $ACCOUNT_INDEX ==="
probe_output="$(ENDPOINT="$ENDPOINT" \
DOMAIN_CHAIN_ID="$DOMAIN_CHAIN_ID" \
DOMAIN_VERIFYING_CONTRACT="$DOMAIN_VERIFYING_CONTRACT" \
ACCOUNTS_FILE="$tmp_accounts" \
REQUEST_TIMEOUT_MS="$REQUEST_TIMEOUT_MS" \
WORKLOAD="$WORKLOAD" \
TRANSFER_AMOUNT="$TRANSFER_AMOUNT" \
MAX_FEE="$MAX_FEE" \
ACCOUNT_INDEX="$ACCOUNT_INDEX" \
bash scripts/live_probe_nonce.sh)"
echo "$probe_output"

starting_nonce="$(printf '%s\n' "$probe_output" | sed -n 's/.*Detected expected nonce: //p' | tail -n 1)"
if [[ -z "$starting_nonce" ]]; then
  echo "Failed to parse STARTING_NONCE from probe output." >&2
  exit 1
fi

echo "=== Running live demo with STARTING_NONCE=$starting_nonce (account $ACCOUNT_INDEX) ==="
ENDPOINT="$ENDPOINT" \
DOMAIN_CHAIN_ID="$DOMAIN_CHAIN_ID" \
DOMAIN_VERIFYING_CONTRACT="$DOMAIN_VERIFYING_CONTRACT" \
ACCOUNTS_FILE="$tmp_accounts" \
WORKLOAD="$WORKLOAD" \
COUNT="$COUNT" \
CONCURRENCY="$CONCURRENCY" \
MAX_FEE="$MAX_FEE" \
RUN_ROUND_TRIP="$RUN_ROUND_TRIP" \
TRANSFER_AMOUNT="$TRANSFER_AMOUNT" \
REQUEST_TIMEOUT_MS="$REQUEST_TIMEOUT_MS" \
MAX_WS_WAIT_MS="$MAX_WS_WAIT_MS" \
STARTING_NONCE="$starting_nonce" \
ACCOUNT_INDEX="$ACCOUNT_INDEX" \
bash scripts/live_sequencer_demo.sh
