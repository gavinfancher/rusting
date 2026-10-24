#!/usr/bin/env bash
# run-matrix.sh - run one scaling experiment over a list of worker configurations.
#
#   scripts/bench/run-matrix.sh EXPERIMENT CONFIG [CONFIG...]
#
# CONFIG is  replicas:cpu:threads:unit_size
#   replicas   number of worker pods
#   cpu        CPU request == limit per pod ("1", "2", "500m", ...)
#   threads    WORKER_THREADS; 0 = default (follow the CPU limit)
#   unit_size  hands per work unit for the job
#
# Example (experiment E2, horizontal scaling):
#   scripts/bench/run-matrix.sh E2-horizontal 1:2:0:2000 2:2:0:2000 3:2:0:2000 6:2:0:2000
#
# For each config the script:
#   1. patches the hearts-worker Deployment (replicas, cpu/memory requests == limits, WORKER_THREADS),
#   2. waits for `kubectl rollout status`,
#   3. waits until /api/workers shows exactly `replicas` active workers,
#   4. submits a fixed-seed job and waits for it,
#   5. POSTs the result to /api/benchmarks (the coordinator fills hands/wall_ms/hands_per_sec from the job).
# On exit (success, error or Ctrl-C) the original Deployment spec is restored.
#
# Environment (all optional):
#   BENCH_STRATEGIES  comma list of 4 strategies   default heuristic,heuristic,random,pimc:20
#   BENCH_HANDS       hands per run                 default 300000
#   BENCH_SEED        job seed                      default 42
#   BENCH_MEM         memory request == limit       default 1Gi
#   BENCH_NOTES       text stored in `notes`        default ""
#   BENCH_REPEAT      runs per config               default 1
#   WARMUP=1          run a small unrecorded job after each rollout (default 1)
#   ROLLOUT_TIMEOUT   seconds to wait for a rollout / workers to appear (default 240).
#                     A config that does not become ready in time (e.g. pods Pending:
#                     Insufficient cpu) is SKIPPED and recorded as a note in the summary.
#   WAIT_TIMEOUT      seconds to wait for one job (default 3600)
#   BASE_URL, NS      see scripts/lib.sh
#   DRY_RUN=1         print what would happen, change nothing
#
# Calibrate BENCH_HANDS so that a 3-pod x 2-CPU run takes about 60 s: run once with
# the default, look at the wall time, scale BENCH_HANDS proportionally.

set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
. "$DIR/../lib.sh"

usage() { sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
[ "$#" -ge 2 ] || usage 1
case "$1" in -h | --help) usage 0 ;; esac

EXPERIMENT="$1"
shift
CONFIGS=("$@")

BENCH_STRATEGIES="${BENCH_STRATEGIES:-heuristic,heuristic,random,pimc:20}"
BENCH_HANDS="${BENCH_HANDS:-300000}"
BENCH_SEED="${BENCH_SEED:-42}"
BENCH_MEM="${BENCH_MEM:-1Gi}"
BENCH_NOTES="${BENCH_NOTES:-}"
BENCH_REPEAT="${BENCH_REPEAT:-1}"
WARMUP="${WARMUP:-1}"
ROLLOUT_TIMEOUT="${ROLLOUT_TIMEOUT:-240}"
WAIT_TIMEOUT="${WAIT_TIMEOUT:-3600}"
DRY_RUN="${DRY_RUN:-0}"

require curl jq kubectl

# --- validate configs before touching the cluster ---------------------------
for c in "${CONFIGS[@]}"; do
  if ! [[ "$c" =~ ^[0-9]+:[0-9.]+m?:[0-9]+:[0-9]+$ ]]; then
    die "bad config '$c' (want replicas:cpu:threads:unit_size, e.g. 3:2:0:2000)"
  fi
done

# strategies as a JSON array, and the largest pimc:N (stored as pimc_samples)
STRAT_JSON=$(jq -cn --arg s "$BENCH_STRATEGIES" '$s | split(",")')
[ "$(jq 'length' <<<"$STRAT_JSON")" = 4 ] || die "BENCH_STRATEGIES must list exactly 4 strategies"
PIMC_SAMPLES=$(jq -n --argjson s "$STRAT_JSON" \
  '[$s[] | select(startswith("pimc:")) | ltrimstr("pimc:") | tonumber] | max // null')

log "experiment $EXPERIMENT: ${#CONFIGS[@]} configs x $BENCH_REPEAT run(s); strategies=$BENCH_STRATEGIES hands=$BENCH_HANDS seed=$BENCH_SEED mem=$BENCH_MEM"
if [ "$DRY_RUN" = 1 ]; then
  for c in "${CONFIGS[@]}"; do log "would run config $c"; done
  exit 0
fi

# --- save the original spec; restore it no matter how we exit ----------------
worker_spec_save
restore() {
  local rc=$?
  trap - EXIT INT TERM
  worker_spec_restore
  log "waiting for the original rollout to finish..."
  rollout_wait hearts-worker 300s || log "WARNING: original spec did not become ready in 300s; check 'kubectl -n $NS get pods'"
  exit "$rc"
}
trap restore EXIT
trap 'exit 130' INT TERM

SHA=$(git_sha)
SKIPPED=()
RECORDED=0

run_config() {
  local cfg="$1" replicas cpu threads unit_size
  IFS=: read -r replicas cpu threads unit_size <<<"$cfg"
  log "=== config $cfg: replicas=$replicas cpu=$cpu threads=$threads unit_size=$unit_size ==="

  patch_worker "$replicas" "$cpu" "$BENCH_MEM" "$threads"
  # `rollout status` exits non-zero if the new pods never become ready (e.g. Pending).
  if ! rollout_wait hearts-worker "${ROLLOUT_TIMEOUT}s"; then
    log "SKIP $cfg: rollout did not complete in ${ROLLOUT_TIMEOUT}s. Pods:"
    kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o wide >&2 || true
    kubectl -n "$NS" get events --field-selector reason=FailedScheduling --sort-by=.lastTimestamp 2>/dev/null | tail -n 3 >&2 || true
    SKIPPED+=("$cfg")
    return 0
  fi
  if ! wait_workers_active "$replicas" "$ROLLOUT_TIMEOUT"; then
    log "SKIP $cfg: /api/workers never showed $replicas active workers"
    SKIPPED+=("$cfg")
    return 0
  fi

  if [ "$WARMUP" = 1 ]; then
    # Warm-up: lets the new pods open their gRPC channels and CPU caches. Not recorded.
    local wspec wid
    wspec=$(jq -cn --argjson s "$STRAT_JSON" --argjson seed "$BENCH_SEED" \
      '{name: "bench-warmup", strategies: $s, num_hands: 2000, seed: $seed, unit_size: 200}')
    wid=$(submit_job "$wspec")
    wait_job "$wid" 300 || true
  fi

  # Record the thread count the workers actually run (resolves threads=0).
  local real_threads
  real_threads=$(api_get /api/workers | jq '[.[] | select(.active) | .threads] | max // 0')
  log "workers report $real_threads thread(s) each"

  local run spec id body
  for ((run = 1; run <= BENCH_REPEAT; run++)); do
    spec=$(jq -cn --arg name "$EXPERIMENT $cfg" --argjson s "$STRAT_JSON" \
      --argjson hands "$BENCH_HANDS" --argjson seed "$BENCH_SEED" --argjson us "$unit_size" \
      '{name: $name, strategies: $s, num_hands: $hands, seed: $seed, unit_size: $us}')
    id=$(submit_job "$spec")
    log "submitted job $id (run $run/$BENCH_REPEAT)"
    wait_job "$id" "$WAIT_TIMEOUT" || true
    if [ "$WAIT_STATUS" != done ]; then
      log "SKIP recording for $cfg: job $id ended as $WAIT_STATUS"
      SKIPPED+=("$cfg(job $id $WAIT_STATUS)")
      continue
    fi
    body=$(jq -cn --arg exp "$EXPERIMENT" --argjson job "$id" --arg sha "$SHA" \
      --argjson replicas "$replicas" --argjson threads "$real_threads" --arg tcfg "$threads" --arg cpu "$cpu" \
      --argjson us "$unit_size" --argjson pimc "$PIMC_SAMPLES" --arg notes "$BENCH_NOTES" \
      '{experiment: $exp, job_id: $job, git_sha: $sha, worker_replicas: $replicas,
        threads_per_worker: $threads, cpu_limit: $cpu, unit_size: $us, pimc_samples: $pimc,
        notes: ($notes + (if $notes == "" then "" else " " end) + "WORKER_THREADS=" + $tcfg)}')
    api_post /api/benchmarks "$body" | jq -c '{id, experiment, worker_replicas, cpu_limit, threads_per_worker, unit_size, hands, wall_ms, hands_per_sec}' >&2
    RECORDED=$((RECORDED + 1))
  done
  return 0
}

for cfg in "${CONFIGS[@]}"; do
  run_config "$cfg"
done

log "done: recorded $RECORDED run(s); skipped: ${SKIPPED[*]:-none}"
log "report: scripts/bench/report.sh $EXPERIMENT"
