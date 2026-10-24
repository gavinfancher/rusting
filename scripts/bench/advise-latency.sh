#!/usr/bin/env bash
# advise-latency.sh - measure POST /api/advise latency (experiment E6).
#
#   scripts/bench/advise-latency.sh [-n REQUESTS] [-s SAMPLES] [-c CONCURRENCY] [-d DEADLINE_MS]
#
#   -n  total requests                      (default 200)
#   -s  Monte Carlo samples per request     (default 2000)
#   -c  concurrent clients                  (default 1)
#   -d  deadline_ms sent in the request     (default 2000, the coordinator's default)
#
# Uses the opening position from docs/CONTRACTS.md with a different seed per
# request so the work is identical in cost but not identical in result. Prints,
# for the client-side wall time (curl time_total) and the server-reported
# latency_ms: p50 / p95 / p99 / max, plus how many answers were partial and the
# average number of shards that answered.
#
# Typical use for E6: scale workers (make scale-workers N=1, 2, 3, ...), wait for
# them to be Ready, then run this script and note the numbers.
#
# Environment: BASE_URL (see scripts/lib.sh), NS.

set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
. "$DIR/../lib.sh"

N="${N:-200}" SAMPLES="${SAMPLES:-2000}" CONC="${CONC:-1}" DEADLINE="${DEADLINE:-2000}"   # env or flags
while getopts "n:s:c:d:h" opt; do
  case "$opt" in
    n) N="$OPTARG" ;;
    s) SAMPLES="$OPTARG" ;;
    c) CONC="$OPTARG" ;;
    d) DEADLINE="$OPTARG" ;;
    *) sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
  esac
done
require curl jq awk sort

OUT=$(mktemp "${TMPDIR:-/tmp}/advise-latency.XXXXXX")
trap 'rm -f "$OUT"' EXIT

body() { # $1 = seed
  jq -cn --argjson samples "$SAMPLES" --argjson deadline "$DEADLINE" --argjson seed "$1" '{
    seat: 0,
    hand: ["3C","7C","JC","4D","6D","9D","QS","5S","TS","3H","8H","KH","AH"],
    trick_leader: 3, trick: ["2C"], played: [],
    voids: [[],[],[],[]], hearts_broken: false, tricks_played: 0,
    points: [0,0,0,0], passed_to: 1, passed_cards: ["AS","KS","9H"],
    rules: {no_points_first_trick: true},
    samples: $samples, deadline_ms: $deadline, seed: $seed}'
}

# client INDEX COUNT   send COUNT requests; append "client_ms<TAB>server_ms<TAB>shards<TAB>partial"
client() {
  local i resp t
  for ((i = 0; i < $2; i++)); do
    # curl prints the response body, then a newline and the total time in seconds
    resp=$(curl -sS --max-time 30 -H 'Content-Type: application/json' \
      -d "$(body $((1000 * $1 + i + 1)))" -w '\n%{time_total}' "$BASE_URL/api/advise" 2>/dev/null) || {
      printf 'ERR\t0\t0\t0\n' >>"$OUT"
      continue
    }
    t="${resp##*$'\n'}"
    jq -r --arg t "$t" '[($t | tonumber * 1000), .latency_ms, .shards, (.partial | tostring)] | @tsv' \
      <<<"${resp%$'\n'*}" >>"$OUT" 2>/dev/null || printf 'ERR\t0\t0\t0\n' >>"$OUT"
  done
}

SHARDS=$(workers_active)
log "advise latency: $N requests, $SAMPLES samples, $CONC client(s), deadline ${DEADLINE}ms; $SHARDS active worker(s) (= shards)"

per=$(((N + CONC - 1) / CONC))
for ((c = 0; c < CONC; c++)); do
  client "$c" "$per" &
done
wait

# pctl COLUMN Q   nearest-rank percentile of one column of the successful rows
# (done with sort because BSD awk on macOS has no asort()).
pctl() {
  local vals n idx
  vals=$(grep -v '^ERR' "$OUT" | cut -f"$1" | sort -n)
  n=$(grep -c . <<<"$vals" || true)
  if [ "$n" -lt 1 ]; then echo 0; return; fi
  idx=$(((n * $2 + 99) / 100))
  if [ "$idx" -lt 1 ]; then idx=1; fi
  sed -n "${idx}p" <<<"$vals"
}

ok=$(grep -vc '^ERR' "$OUT" || true)
err=$(grep -c '^ERR' "$OUT" || true)
if [ "$ok" -eq 0 ]; then
  die "no successful requests ($err errors); is $BASE_URL reachable and ADVISOR_SHARD_DNS set?"
fi
partial=$(grep -c 'true$' "$OUT" || true)
avg_shards=$(grep -v '^ERR' "$OUT" | cut -f3 | awk '{s += $1} END {printf "%.1f", s / NR}')
echo "requests ok=$ok errors=$err partial=$partial avg_shards=$avg_shards"
printf '%-22s %8s %8s %8s %8s\n' "latency (ms)" p50 p95 p99 max
for row in "client (curl total):1" "server (latency_ms):2"; do
  col="${row##*:}"
  printf '%-22s %8.0f %8.0f %8.0f %8.0f\n' "${row%:*}" \
    "$(pctl "$col" 50)" "$(pctl "$col" 95)" "$(pctl "$col" 99)" "$(pctl "$col" 100)"
done
