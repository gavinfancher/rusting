#!/usr/bin/env bash
# report.sh - print an aligned table and a CSV for one experiment.
#
#   scripts/bench/report.sh EXPERIMENT [--csv-only|--table-only]
#
# Fetches GET /api/benchmarks?experiment=EXPERIMENT, groups identical configs
# (replicas, cpu, threads, unit size, pimc samples; repeats are averaged), orders
# them by first appearance (oldest first) and computes, relative to the FIRST row:
#
#   speedup    = hands_per_sec / hands_per_sec(first)
#   cores      = replicas x cpu            (cpu "500m" = 0.5)
#   efficiency = speedup / (cores / cores(first))
#              1.0 = perfect linear scaling; < 1 means you pay for CPU you don't use.
#              (In experiments where total cores don't change, e.g. unit size,
#              efficiency equals speedup.)
#
# Environment: BASE_URL (see scripts/lib.sh), LIMIT=1000 (rows to fetch),
#              SINCE=2026-10-24T00:00:00Z  only use runs created at/after this time.

set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
. "$DIR/../lib.sh"

[ "$#" -ge 1 ] || { sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 1; }
case "$1" in -h | --help) sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;; esac
EXPERIMENT="$1"
MODE="${2:-both}"
require curl jq

EXP_ENC=$(jq -rn --arg e "$EXPERIMENT" '$e|@uri')
ROWS=$(api_get "/api/benchmarks?experiment=$EXP_ENC&limit=${LIMIT:-1000}")
if [ "$(jq 'length' <<<"$ROWS")" = 0 ]; then
  die "no benchmark rows for experiment '$EXPERIMENT' at $BASE_URL"
fi

# jq program: rows (newest first) -> array of result objects
PROG='
  def cores: ((.cpu_limit // "1") | if endswith("m") then (rtrimstr("m") | tonumber / 1000) else tonumber end)
             * (.worker_replicas // 1);
  [ .[] | select(.hands_per_sec != null) | select(($since == "") or (.created_at >= $since)) ]
  | sort_by(.created_at)
  | group_by([.worker_replicas, .cpu_limit, .threads_per_worker, .unit_size, .pimc_samples])
  | map({
      replicas: .[0].worker_replicas, cpu: .[0].cpu_limit, threads: .[0].threads_per_worker,
      unit_size: .[0].unit_size, pimc: .[0].pimc_samples, hands: .[0].hands,
      n: length,
      wall_s: ((map(.wall_ms) | add / length) / 1000),
      hps: (map(.hands_per_sec) | add / length),
      cores: (.[0] | cores),
      first: (.[0].created_at),
      sha: (.[0].git_sha // "")
    })
  | sort_by(.first)
  | (.[0]) as $b
  | map(. + {
      speedup: (.hps / $b.hps),
      efficiency: ((.hps / $b.hps) / (.cores / $b.cores))
    })
'
RESULT=$(jq --arg since "${SINCE:-}" "$PROG" <<<"$ROWS")

if [ "$MODE" != "--csv-only" ]; then
  echo "Experiment: $EXPERIMENT   (baseline = first row; efficiency = speedup / core ratio)"
  {
    printf 'replicas\tcpu\tthreads\tunit\tpimc\thands\twall_s\thands/s\tspeedup\teff\truns\tsha\n'
    jq -r '.[] | [.replicas, .cpu, .threads, .unit_size, (.pimc // "-"), .hands,
                  (.wall_s * 10 | round / 10), (.hps | round), (.speedup * 100 | round / 100),
                  (.efficiency * 100 | round / 100), .n, .sha] | @tsv' <<<"$RESULT"
  } | column -t -s "$(printf '\t')"
  echo
fi

if [ "$MODE" != "--table-only" ]; then
  if [ "$MODE" != "--csv-only" ]; then echo "CSV:"; fi
  jq -r '(["replicas","cpu","threads","unit_size","pimc_samples","hands","wall_s","hands_per_sec","speedup","efficiency","runs","git_sha"] | @csv),
         (.[] | [.replicas, .cpu, .threads, .unit_size, .pimc, .hands, .wall_s, .hps, .speedup, .efficiency, .n, .sha] | @csv)' <<<"$RESULT"
fi
