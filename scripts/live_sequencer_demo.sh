#!/usr/bin/env bash
set -euo pipefail

# Live sequencer demo runner for Monday presentations.
# Runs ack + round-trip benchmarks against a deployed endpoint.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

ENDPOINT="${ENDPOINT:-https://eth-sepolia.rollups.cartesi.io/v2}"
DOMAIN_CHAIN_ID="${DOMAIN_CHAIN_ID:-11155111}"
DOMAIN_VERIFYING_CONTRACT="${DOMAIN_VERIFYING_CONTRACT:-}"
WORKLOAD="${WORKLOAD:-funded-transfer}"
ACCOUNTS_FILE="${ACCOUNTS_FILE:-}"
COUNT="${COUNT:-50}"
CONCURRENCY="${CONCURRENCY:-4}"
MAX_FEE="${MAX_FEE:-0}"
REQUEST_TIMEOUT_MS="${REQUEST_TIMEOUT_MS:-5000}"
MAX_WS_WAIT_MS="${MAX_WS_WAIT_MS:-10000}"
FROM_OFFSET="${FROM_OFFSET:-0}"
TRANSFER_AMOUNT="${TRANSFER_AMOUNT:-1}"
STARTING_NONCE="${STARTING_NONCE:-0}"
ROUND_TRIP_STARTING_NONCE="${ROUND_TRIP_STARTING_NONCE:-auto}"
ALLOW_REJECTIONS="${ALLOW_REJECTIONS:-false}"
OUT_DIR="${OUT_DIR:-benchmarks/results}"
RUN_LABEL="${RUN_LABEL:-live-sepolia}"
RUN_ROUND_TRIP="${RUN_ROUND_TRIP:-auto}"

mkdir -p "$OUT_DIR"

if [[ -z "$DOMAIN_VERIFYING_CONTRACT" ]]; then
  echo "Missing DOMAIN_VERIFYING_CONTRACT. Example:" >&2
  echo "  DOMAIN_VERIFYING_CONTRACT=0xYourSequencerAppAddress bash scripts/live_sequencer_demo.sh" >&2
  exit 1
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
ACK_JSON="$OUT_DIR/ack-${RUN_LABEL}-${TS}.json"
RT_JSON="$OUT_DIR/round-trip-${RUN_LABEL}-${TS}.json"

COMMON_ARGS=(
  --endpoint "$ENDPOINT"
  --domain-chain-id "$DOMAIN_CHAIN_ID"
  --domain-verifying-contract "$DOMAIN_VERIFYING_CONTRACT"
  --workload "$WORKLOAD"
  --transfer-amount "$TRANSFER_AMOUNT"
  --starting-nonce "$STARTING_NONCE"
  --count "$COUNT"
  --concurrency "$CONCURRENCY"
  --max-fee "$MAX_FEE"
  --request-timeout-ms "$REQUEST_TIMEOUT_MS"
)

if [[ -n "$ACCOUNTS_FILE" ]]; then
  COMMON_ARGS+=(--accounts-file "$ACCOUNTS_FILE")
fi

if [[ "$ALLOW_REJECTIONS" == "true" ]]; then
  COMMON_ARGS+=(--allow-rejections)
fi

if [[ "$ROUND_TRIP_STARTING_NONCE" == "auto" ]]; then
  ROUND_TRIP_STARTING_NONCE="$((STARTING_NONCE + COUNT))"
fi

echo "== Live sequencer demo =="
echo "endpoint=$ENDPOINT"
echo "domain_chain_id=$DOMAIN_CHAIN_ID"
echo "domain_verifying_contract=$DOMAIN_VERIFYING_CONTRACT"
echo "workload=$WORKLOAD count=$COUNT concurrency=$CONCURRENCY max_fee=$MAX_FEE"
echo "starting_nonce=$STARTING_NONCE"
echo "round_trip_starting_nonce=$ROUND_TRIP_STARTING_NONCE"
echo "accounts_file=${ACCOUNTS_FILE:-<default benchmark keys>}"
echo

echo "== 1/2 Ack latency =="
cargo run -p benchmarks --bin ack_latency -- \
  "${COMMON_ARGS[@]}" \
  --json-out "$ACK_JSON"

should_run_round_trip="true"
if [[ "$RUN_ROUND_TRIP" == "false" ]]; then
  should_run_round_trip="false"
elif [[ "$RUN_ROUND_TRIP" == "auto" && "$ENDPOINT" == https://* ]]; then
  should_run_round_trip="false"
fi

if [[ "$should_run_round_trip" == "true" ]]; then
  echo
  echo "== 2/2 Round-trip latency (HTTP ack + WS replay) =="
  cargo run -p benchmarks --bin round_trip_latency -- \
    "${COMMON_ARGS[@]}" \
    --starting-nonce "$ROUND_TRIP_STARTING_NONCE" \
    --from-offset "$FROM_OFFSET" \
    --max-ws-wait-ms "$MAX_WS_WAIT_MS" \
    --json-out "$RT_JSON"
else
  echo
  echo "== 2/2 Round-trip latency skipped =="
  echo "Set RUN_ROUND_TRIP=true to force round-trip run."
fi

echo
echo "Artifacts:"
echo "  $ACK_JSON"
if [[ "$should_run_round_trip" == "true" ]]; then
  echo "  $RT_JSON"
fi
