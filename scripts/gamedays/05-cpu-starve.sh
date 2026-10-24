#!/usr/bin/env bash
# Game day 5: CPU-starve the workers (CFS throttling).
#
#   scripts/gamedays/05-cpu-starve.sh [--yes]
#
# Knobs:  CPU_LIMIT=500m   new CPU request == limit per worker
#         THREADS=4        WORKER_THREADS forced on the workers (more threads than CPU!)
#         BASELINE=1       run the reference job once BEFORE the change to compare speed
#
# Hypothesis (docs/GAMEDAYS.md, drill 5): the CPU limit is enforced by the kernel's CFS
# bandwidth controller: a container may use at most `limit x 100 ms` of CPU time per 100 ms
# period. With 4 busy rayon threads and a 500m limit, the threads burn the 50 ms quota in
# about 12 ms and are then frozen ("throttled") for the rest of the period. Consequences:
#   * container_cpu_cfs_throttled_periods_total / container_cpu_cfs_periods_total approaches 1,
#     so the "CPU throttling % (per pod)" panel goes to ~70-90% and HeartsCPUThrottlingHigh
#     fires after 5 minutes;
#   * hands/s per pod falls to about CPU_LIMIT/original-limit of normal;
#   * nothing is wrong or lost: the job still finishes with the reference standings, just slower.
# If you do NOT set WORKER_THREADS the worker sizes its pool from the cgroup quota
# (available_parallelism honours it) and there is almost no throttling. Choosing threads
# above the limit is what makes throttling visible.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

CPU_LIMIT="${CPU_LIMIT:-500m}"
THREADS="${THREADS:-4}"
BASELINE="${BASELINE:-1}"
DESC="Optionally runs the reference job as a baseline, then patches the worker Deployment to
cpu request=limit=${CPU_LIMIT} and WORKER_THREADS=${THREADS} (a rollout), runs the reference job
again and compares speed and throttling. The original spec is restored afterwards (another rollout)."
gd_begin "05 CPU-starve the workers" "$DESC" "$@"

worker_spec_save
gd_cleanup_add restore_workers
restore_workers() {
  worker_spec_restore
  rollout_wait hearts-worker 300s
}
mem=$(jq -r '.resources.limits.memory' "$WORKER_SPEC_FILE")

base_ms=0
if [ "$BASELINE" = 1 ]; then
  log "baseline run of the reference job..."
  bjob=$(ref_submit) || exit 1
  wait_job "$bjob" "$GD_JOB_TIMEOUT" || true
  base_ms=$(job_field "$bjob" '.elapsed_ms // 0')
  note "baseline elapsed" "${base_ms}ms"
fi

watch_panel hearts-workers "Hearts Worker Performance" "CPU throttling % (per pod)" \
  '100 * sum by (pod) (rate(container_cpu_cfs_throttled_periods_total{namespace="hearts", pod=~"hearts-worker-.*", container="worker"}[1m])) / sum by (pod) (rate(container_cpu_cfs_periods_total{namespace="hearts", pod=~"hearts-worker-.*", container="worker"}[1m]))'
watch_panel hearts-workers "Hearts Worker Performance" "CPU cores used vs limit (per pod)" \
  'sum by (pod) (rate(container_cpu_usage_seconds_total{namespace="hearts", pod=~"hearts-worker-.*", container="worker"}[1m]))'
watch_panel hearts-workers "Hearts Worker Performance" "Unit duration p50 / p95" \
  'histogram_quantile(0.95, sum by (le) (rate(hearts_worker_unit_duration_seconds_bucket{namespace="hearts"}[2m])))'
tips "kubectl -n $NS top pods -l app.kubernetes.io/name=hearts-worker" \
  "kubectl -n $NS get pod -l app.kubernetes.io/name=hearts-worker -o custom-columns=NAME:.metadata.name,QOS:.status.qosClass,CPU:.spec.containers[0].resources.limits.cpu" \
  "kubectl -n $NS logs deploy/hearts-worker | grep 'building rayon pool'   # threads= and requested= fields" \
  "kubectl -n $NS describe pod -l app.kubernetes.io/name=hearts-worker | grep -A3 -E 'Limits|QoS'"

log "patching workers: replicas=$GD_REPLICAS cpu=$CPU_LIMIT memory=$mem WORKER_THREADS=$THREADS"
patch_worker "$GD_REPLICAS" "$CPU_LIMIT" "$mem" "$THREADS"
rollout_wait hearts-worker 300s
wait_workers_active "$GD_REPLICAS" 120 || true

job=$(ref_submit) || exit 1
log "submitted reference job $job on starved workers"
ref_check "$job"

ratio=$(cfs_throttle_ratio)
note "CFS throttled periods / periods (all worker pods, since they started)" "$ratio"
expect "workers were throttled in more than 25% of periods" "ratio=$ratio" \
  awk -v r="$ratio" 'BEGIN { exit !(r > 0.25) }'
if [ "$base_ms" -gt 0 ] && [ "$REF_ELAPSED_MS" -gt 0 ]; then
  slowdown=$(awk -v a="$REF_ELAPSED_MS" -v b="$base_ms" 'BEGIN { printf "%.2f", a / b }')
  expect "the starved run was slower than the baseline" "x${slowdown} (${REF_ELAPSED_MS}ms vs ${base_ms}ms)" \
    awk -v s="$slowdown" 'BEGIN { exit !(s > 1.3) }'
fi
thr=$(worker_metric hearts_worker_threads)
note "sum of hearts_worker_threads over pods" "$thr (= replicas x THREADS=$((GD_REPLICAS * THREADS)))"
note "alert HeartsCPUThrottlingHigh" "state=$(alert_state HeartsCPUThrottlingHigh) (needs 5 minutes of >25% throttling to fire; restoring now will usually beat it)"

gd_finish
