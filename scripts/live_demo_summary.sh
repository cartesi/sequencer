#!/usr/bin/env bash
set -euo pipefail

# Build a slide-ready markdown summary from benchmark JSON outputs.
#
# Usage:
#   bash scripts/live_demo_summary.sh
#   ACK_JSON=path/to/ack.json RT_JSON=path/to/round-trip.json bash scripts/live_demo_summary.sh

RESULTS_DIR="${RESULTS_DIR:-benchmarks/results}"
ACK_JSON="${ACK_JSON:-}"
RT_JSON="${RT_JSON:-}"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required. Install with: sudo apt install -y jq" >&2
  exit 1
fi

if [[ -z "$ACK_JSON" ]]; then
  ACK_JSON="$(ls -1t "$RESULTS_DIR"/ack-*.json 2>/dev/null | head -n 1 || true)"
fi
if [[ -z "$RT_JSON" ]]; then
  RT_JSON="$(ls -1t "$RESULTS_DIR"/round-trip-*.json 2>/dev/null | head -n 1 || true)"
fi

if [[ -z "$ACK_JSON" || ! -f "$ACK_JSON" ]]; then
  echo "Could not find ack JSON. Set ACK_JSON=... or run live demo first." >&2
  exit 1
fi
rt_available="true"
if [[ -z "$RT_JSON" || ! -f "$RT_JSON" ]]; then
  rt_available="false"
fi

fmt_ms() {
  awk -v v="$1" 'BEGIN { printf("%.2f ms", v * 1000.0) }'
}

fmt_pct() {
  awk -v v="$1" 'BEGIN { printf("%.2f%%", v * 100.0) }'
}

ack_endpoint="$(jq -r '.report.endpoint' "$ACK_JSON")"
ack_count="$(jq -r '.report.count' "$ACK_JSON")"
ack_accepted="$(jq -r '.report.accepted' "$ACK_JSON")"
ack_rejected="$(jq -r '.report.rejected' "$ACK_JSON")"
ack_rej_rate="$(jq -r '.report.rejection_rate' "$ACK_JSON")"
ack_tps="$(jq -r '
  def secs_f64($d): (($d.secs // 0) + (($d.nanos // 0) / 1000000000));
  (.report.total_wall | secs_f64(.)) as $wall
  | if $wall > 0 then (.report.ack_latency_accepted.count / $wall) else 0 end
' "$ACK_JSON")"
ack_p50="$(jq -r '((.report.ack_latency_accepted.p50.secs // 0) + ((.report.ack_latency_accepted.p50.nanos // 0) / 1000000000))' "$ACK_JSON")"
ack_p95="$(jq -r '((.report.ack_latency_accepted.p95.secs // 0) + ((.report.ack_latency_accepted.p95.nanos // 0) / 1000000000))' "$ACK_JSON")"
ack_p99="$(jq -r '((.report.ack_latency_accepted.p99.secs // 0) + ((.report.ack_latency_accepted.p99.nanos // 0) / 1000000000))' "$ACK_JSON")"

if [[ "$rt_available" == "true" ]]; then
  rt_count="$(jq -r '.report.count' "$RT_JSON")"
  rt_accepted="$(jq -r '.report.accepted' "$RT_JSON")"
  rt_rejected="$(jq -r '.report.rejected' "$RT_JSON")"
  rt_rej_rate="$(jq -r '.report.rejection_rate' "$RT_JSON")"
  rt_tps="$(jq -r '
    def secs_f64($d): (($d.secs // 0) + (($d.nanos // 0) / 1000000000));
    (.report.total_wall | secs_f64(.)) as $wall
    | if $wall > 0 then (.report.round_trip_latency_accepted.count / $wall) else 0 end
  ' "$RT_JSON")"
  rt_p50="$(jq -r '((.report.round_trip_latency_accepted.p50.secs // 0) + ((.report.round_trip_latency_accepted.p50.nanos // 0) / 1000000000))' "$RT_JSON")"
  rt_p95="$(jq -r '((.report.round_trip_latency_accepted.p95.secs // 0) + ((.report.round_trip_latency_accepted.p95.nanos // 0) / 1000000000))' "$RT_JSON")"
  rt_p99="$(jq -r '((.report.round_trip_latency_accepted.p99.secs // 0) + ((.report.round_trip_latency_accepted.p99.nanos // 0) / 1000000000))' "$RT_JSON")"
fi

echo "## Live Sequencer Results"
echo
echo "- Endpoint: \`$ack_endpoint\`"
echo "- Ack JSON: \`$ACK_JSON\`"
if [[ "$rt_available" == "true" ]]; then
  echo "- Round-trip JSON: \`$RT_JSON\`"
else
  echo "- Round-trip JSON: _not found (ack-only run)_"
fi
echo "- Percentile legend: p50=median (typical), p95=tail (worst 5%), p99=deep tail (worst 1%)"
echo

print_row() {
  printf "| %-18s | %16s | %16s |\n" "$1" "$2" "$3"
}

print_sep() {
  printf "|--------------------|------------------|------------------|\n"
}

print_row "Metric" "Ack latency run" "Round-trip run"
print_sep
if [[ "$rt_available" == "true" ]]; then
  print_row "Total submitted" "$ack_count" "$rt_count"
  print_row "Accepted" "$ack_accepted" "$rt_accepted"
  print_row "Rejected" "$ack_rejected" "$rt_rejected"
  print_row "Rejection rate" "$(fmt_pct "$ack_rej_rate")" "$(fmt_pct "$rt_rej_rate")"
  print_row "Throughput" "$(awk -v v="$ack_tps" 'BEGIN { printf("%.2f tx/s", v) }')" "$(awk -v v="$rt_tps" 'BEGIN { printf("%.2f tx/s", v) }')"
  print_row "p50 latency" "$(fmt_ms "$ack_p50")" "$(fmt_ms "$rt_p50")"
  print_row "p95 latency" "$(fmt_ms "$ack_p95")" "$(fmt_ms "$rt_p95")"
  print_row "p99 latency" "$(fmt_ms "$ack_p99")" "$(fmt_ms "$rt_p99")"
else
  print_row "Total submitted" "$ack_count" "n/a"
  print_row "Accepted" "$ack_accepted" "n/a"
  print_row "Rejected" "$ack_rejected" "n/a"
  print_row "Rejection rate" "$(fmt_pct "$ack_rej_rate")" "n/a"
  print_row "Throughput" "$(awk -v v="$ack_tps" 'BEGIN { printf("%.2f tx/s", v) }')" "n/a"
  print_row "p50 latency" "$(fmt_ms "$ack_p50")" "n/a"
  print_row "p95 latency" "$(fmt_ms "$ack_p95")" "n/a"
  print_row "p99 latency" "$(fmt_ms "$ack_p99")" "n/a"
fi
