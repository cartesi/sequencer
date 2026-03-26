#!/usr/bin/env bash
set -euo pipefail

# One-command live demo runner.
# Usage:
#   bash scripts/live_demo.sh <account_index>

if [[ $# -ne 1 ]]; then
  echo "Usage: bash scripts/live_demo.sh <account_index>" >&2
  exit 1
fi

ACCOUNT_INDEX="$1"
if ! [[ "$ACCOUNT_INDEX" =~ ^[0-9]+$ ]] || [[ "$ACCOUNT_INDEX" -lt 1 ]]; then
  echo "account_index must be a positive integer" >&2
  exit 1
fi

# Hardcoded demo defaults (override only if needed).
ENDPOINT="${ENDPOINT:-https://eth-sepolia.rollups.cartesi.io/v2}"
DOMAIN_CHAIN_ID="${DOMAIN_CHAIN_ID:-11155111}"
DOMAIN_VERIFYING_CONTRACT="${DOMAIN_VERIFYING_CONTRACT:-0x38d59b09c055D7485dc13AEC4d98b9436291B3e8}"
ACCOUNTS_FILE="${ACCOUNTS_FILE:-benchmarks/sepolia-demo.txt}"
WORKLOAD="${WORKLOAD:-funded-transfer}"
COUNT="${COUNT:-30}"
CONCURRENCY="${CONCURRENCY:-4}"
MAX_FEE="${MAX_FEE:-0}"
TRANSFER_AMOUNT="${TRANSFER_AMOUNT:-1}"
REQUEST_TIMEOUT_MS="${REQUEST_TIMEOUT_MS:-5000}"
MAX_WS_WAIT_MS="${MAX_WS_WAIT_MS:-10000}"
OUT_DIR="${OUT_DIR:-benchmarks/results}"

if [[ ! -f "$ACCOUNTS_FILE" ]]; then
  echo "accounts file not found: $ACCOUNTS_FILE" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
ACK_JSON="$OUT_DIR/ack-live-sepolia-${TS}.json"
RT_JSON="$OUT_DIR/round-trip-live-sepolia-${TS}.json"

pick_account_line() {
  awk -v idx="$1" 'NF>0 {n++; if (n==idx) {print; exit}}' "$2"
}

account_line="$(pick_account_line "$ACCOUNT_INDEX" "$ACCOUNTS_FILE")"
if [[ -z "$account_line" ]]; then
  echo "No non-empty account line at ACCOUNT_INDEX=$ACCOUNT_INDEX in $ACCOUNTS_FILE" >&2
  exit 1
fi

tmp_accounts="$(mktemp)"
trap 'rm -f "$tmp_accounts"' EXIT
printf '%s\n' "$account_line" > "$tmp_accounts"

probe_nonce() {
  local out
  out="$(ENDPOINT="$ENDPOINT" \
    DOMAIN_CHAIN_ID="$DOMAIN_CHAIN_ID" \
    DOMAIN_VERIFYING_CONTRACT="$DOMAIN_VERIFYING_CONTRACT" \
    ACCOUNTS_FILE="$tmp_accounts" \
    ACCOUNT_INDEX=1 \
    REQUEST_TIMEOUT_MS="$REQUEST_TIMEOUT_MS" \
    WORKLOAD="$WORKLOAD" \
    TRANSFER_AMOUNT="$TRANSFER_AMOUNT" \
    MAX_FEE="$MAX_FEE" \
    bash scripts/live_probe_nonce.sh)"
  printf '%s\n' "$out" | sed -n 's/.*Detected expected nonce: //p' | tail -n 1
}

echo "== Live demo =="
echo "account_index=$ACCOUNT_INDEX"
echo "endpoint=$ENDPOINT"
echo "count=$COUNT concurrency=$CONCURRENCY"
echo

echo "== 1/4 Probe nonce for ACK run =="
ACK_NONCE="$(probe_nonce)"
if [[ -z "$ACK_NONCE" ]]; then
  echo "Failed to detect nonce for ACK run." >&2
  exit 1
fi
echo "starting_nonce(ack)=$ACK_NONCE"
echo

echo "== 2/4 Ack latency =="
cargo run -p benchmarks --bin ack_latency -- \
  --endpoint "$ENDPOINT" \
  --domain-chain-id "$DOMAIN_CHAIN_ID" \
  --domain-verifying-contract "$DOMAIN_VERIFYING_CONTRACT" \
  --workload "$WORKLOAD" \
  --accounts-file "$tmp_accounts" \
  --transfer-amount "$TRANSFER_AMOUNT" \
  --starting-nonce "$ACK_NONCE" \
  --count "$COUNT" \
  --concurrency "$CONCURRENCY" \
  --max-fee "$MAX_FEE" \
  --request-timeout-ms "$REQUEST_TIMEOUT_MS" \
  --json-out "$ACK_JSON"
echo

echo "== 3/4 Probe nonce for round-trip run =="
RT_NONCE="$(probe_nonce)"
if [[ -z "$RT_NONCE" ]]; then
  echo "Failed to detect nonce for round-trip run." >&2
  exit 1
fi
echo "starting_nonce(round-trip)=$RT_NONCE"
echo

echo "== 4/4 Round-trip latency =="
cargo run -p benchmarks --bin round_trip_latency -- \
  --endpoint "$ENDPOINT" \
  --domain-chain-id "$DOMAIN_CHAIN_ID" \
  --domain-verifying-contract "$DOMAIN_VERIFYING_CONTRACT" \
  --workload "$WORKLOAD" \
  --accounts-file "$tmp_accounts" \
  --transfer-amount "$TRANSFER_AMOUNT" \
  --starting-nonce "$RT_NONCE" \
  --count "$COUNT" \
  --concurrency "$CONCURRENCY" \
  --max-fee "$MAX_FEE" \
  --request-timeout-ms "$REQUEST_TIMEOUT_MS" \
  --max-ws-wait-ms "$MAX_WS_WAIT_MS" \
  --json-out "$RT_JSON"
echo

echo "Artifacts:"
echo "  $ACK_JSON"
echo "  $RT_JSON"
echo
echo "Summary:"
ACK_JSON="$ACK_JSON" RT_JSON="$RT_JSON" bash scripts/live_demo_summary.sh
