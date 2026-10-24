#!/usr/bin/env bash
# Game day 2: kill the coordinator pod in the middle of a job.
#
#   scripts/gamedays/02-kill-coordinator.sh [--yes]
#
# Hypothesis (docs/GAMEDAYS.md, drill 2): the coordinator keeps no state in memory
# (everything is in Postgres), so the Deployment simply starts a new pod. While it
# is gone, workers' gRPC calls fail; they retry with exponential backoff + jitter
# (hearts_worker_lease_errors_total grows) and keep their in-flight unit. The
# readiness probe keeps traffic away from the new pod until /readyz (a DB SELECT 1)
# passes. The job finishes with the reference standings.
# Note that the coordinator's own Prometheus counters restart from zero.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

DESC="Stretches the units (FAULT_SLOW_MS), submits the reference job, waits until a few units are
done, then deletes the coordinator pod. Measures the outage and checks the job still finishes
with identical standings. The FAULT_SLOW_MS override is removed afterwards."
gd_begin "02 Kill the coordinator" "$DESC" "$@"

stretch_on
old_pod=$(kubectl -n "$NS" get pod -l app.kubernetes.io/name=hearts-coordinator -o jsonpath='{.items[0].metadata.name}')
errs_before=$(worker_metric hearts_worker_lease_errors_total)

job=$(ref_submit) || exit 1
log "submitted reference job $job; coordinator pod is $old_pod"
watch_panel hearts-overview "Hearts Overview" "HTTP requests/s by route and status" \
  'sum by (route, status) (rate(hearts_http_requests_total{namespace="hearts"}[1m]))  # gap while the pod is gone'
watch_panel hearts-workers "Hearts Worker Performance" "Lease RPC errors/s and idle polls/s" \
  'sum by (rpc) (rate(hearts_worker_lease_errors_total{namespace="hearts"}[1m]))'
watch_panel hearts-overview "Hearts Overview" "Active workers over time" \
  'max(hearts_workers_active{namespace="hearts"})  # the gauge itself disappears while the coordinator is down'
tips "kubectl -n $NS get pods -l app.kubernetes.io/name=hearts-coordinator -w" \
  "kubectl -n $NS describe pod -l app.kubernetes.io/name=hearts-coordinator | tail -30   # Events: readiness probe failures" \
  "kubectl -n $NS logs deploy/hearts-worker --since=2m | grep -i 'backing off'" \
  "kubectl -n $NS logs -l app.kubernetes.io/name=hearts-coordinator --previous   # nothing: the old pod is gone" \
  "kubectl -n $NS get endpoints hearts-coordinator   # empty while not Ready"

wait_progress "$job" 3 180 || log "WARNING: fewer than 3 units finished in 180 s; killing anyway"

log "deleting coordinator pod $old_pod"
kubectl -n "$NS" delete pod "$old_pod" --wait=false
down_epoch=$(date +%s)

# observe the outage from the outside
wait_until "API stops answering /healthz" 60 bash -c "! curl -fsS --max-time 3 '$BASE_URL/healthz' >/dev/null" || true
wait_until "API answers /readyz again" 180 curl -fsS --max-time 3 "$BASE_URL/readyz" || true
outage=$(($(date +%s) - down_epoch))
note "outage seen from the ingress" "${outage}s"

new_pod=$(kubectl -n "$NS" get pod -l app.kubernetes.io/name=hearts-coordinator -o jsonpath='{.items[0].metadata.name}')
expect "a new coordinator pod is serving" "old=$old_pod new=$new_pod" [ "$new_pod" != "$old_pod" ]

ref_check "$job"

errs_after=$(worker_metric hearts_worker_lease_errors_total)
expect "workers saw RPC errors and retried (hearts_worker_lease_errors_total)" "$errs_before -> $errs_after" \
  [ "$errs_after" -gt "$errs_before" ]
note "coordinator counters restarted" "hearts_units_completed_total is now $(coord_metric hearts_units_completed_total) (it started from 0 in the new pod)"

gd_finish
