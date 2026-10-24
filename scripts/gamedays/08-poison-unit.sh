#!/usr/bin/env bash
# Game day 8: poison unit (every unit panics) -> units fail -> HeartsUnitsFailed alert.
#
#   scripts/gamedays/08-poison-unit.sh [--yes]
#
# Hypothesis (docs/GAMEDAYS.md, drill 8): with FAULT_PANIC_EVERY_N=1 every worker panics inside
# the engine on every unit. The panic is caught (std::panic::catch_unwind around the rayon
# pool.install in crates/worker/src/runner.rs), so the process does NOT crash and does not
# restart; instead the worker reports CompleteWork(error). The coordinator puts the unit back to
# `pending` and a worker leases it again (attempts + 1) until attempts reach MAX_ATTEMPTS (3);
# then the unit becomes `failed` for good (poison-pill protection), and the job ends `failed`.
# hearts_units_failed_total increases, the "Failed units" stat turns red, and after the
# rule's evaluation interval the alert HeartsUnitsFailed fires (for: 0m). Healthy pods, no
# restarts: this is a DATA/logic fault, not an infrastructure one. After removing the fault,
# the reference job succeeds with the reference standings: the system is intact.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

POISON_JOB='{"name":"gameday-poison","strategies":["heuristic","heuristic","random","pimc:20"],"num_hands":10000,"seed":42,"unit_size":2000}'
DESC="Sets FAULT_PANIC_EVERY_N=1 on all workers (a rollout), submits a 5-unit job and expects it to
end 'failed' with every unit failed after MAX_ATTEMPTS. Checks the counters and (best effort) the
HeartsUnitsFailed alert. Then removes the fault and runs the reference job to prove recovery.
The failed job stays in the database as evidence (history only; it holds no work)."
gd_begin "08 Poison unit" "$DESC" "$@"

fault_off() {
  log "removing FAULT_PANIC_EVERY_N override"
  kubectl -n "$NS" set env deploy/hearts-worker FAULT_PANIC_EVERY_N- >/dev/null &&
    rollout_wait hearts-worker 300s
}
gd_cleanup_add fault_off

failed_before=$(coord_metric hearts_units_failed_total)
errored_before=$(coord_metric hearts_units_errored_total)
restarts_before=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o json | jq '[.items[].status.containerStatuses[]?.restartCount] | add // 0')

log "enabling FAULT_PANIC_EVERY_N=1 on all workers"
kubectl -n "$NS" set env deploy/hearts-worker FAULT_PANIC_EVERY_N=1 >/dev/null
rollout_wait hearts-worker 300s
wait_workers_active "$GD_REPLICAS" 120 || true

watch_panel hearts-overview "Hearts Overview" "Failed units" 'sum(hearts_units{namespace="hearts", state="failed"})'
watch_panel hearts-overview "Hearts Overview" "Unit event rates" \
  'sum(rate(hearts_units_errored_total{namespace="hearts"}[1m])) and sum(rate(hearts_units_failed_total{namespace="hearts"}[1m]))'
watch_panel hearts-workers "Hearts Worker Performance" "Worker results/s" \
  'sum by (result) (rate(hearts_worker_units_completed_total{namespace="hearts"}[1m]))  # result="error"'
tips "kubectl -n $NS logs deploy/hearts-worker --tail=30 | grep -i -E 'panic|injected|failed'" \
  "kubectl -n $NS get pods -l app.kubernetes.io/name=hearts-worker   # RESTARTS stays 0: the panic is caught" \
  "make psql   then:  SELECT id, state, attempts, last_error FROM work_units WHERE state = 'failed' ORDER BY id DESC LIMIT 10;" \
  "kubectl get --raw /api/v1/namespaces/$MON_NS/services/$PROM_SVC:9090/proxy/api/v1/alerts | jq '.data.alerts[] | {alert: .labels.alertname, state}'"

job=$(submit_job "$POISON_JOB") || exit 1
log "submitted poison job $job (5 units, MAX_ATTEMPTS=3 attempts each)"
wait_job "$job" 300 || true
expect "the job ended as failed" "status=$WAIT_STATUS" [ "$WAIT_STATUS" = failed ]
j=$(job_json "$job")
nfail=$(jq -r '.progress.failed' <<<"$j")
expect "all 5 units are in state failed" "failed=$nfail" [ "$nfail" -eq 5 ]

sleep 3
failed_after=$(coord_metric hearts_units_failed_total)
errored_after=$(coord_metric hearts_units_errored_total)
expect "hearts_units_failed_total increased by 5" "$failed_before -> $failed_after" [ $((failed_after - failed_before)) -ge 5 ]
expect "hearts_units_errored_total increased by 15 (5 units x 3 attempts)" "$errored_before -> $errored_after" \
  [ $((errored_after - errored_before)) -ge 15 ]
werr=$(worker_metric hearts_worker_units_completed_total 'result="error"')
note "worker-side hearts_worker_units_completed_total{result=\"error\"}" "$werr"
restarts_after=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o json | jq '[.items[].status.containerStatuses[]?.restartCount] | add // 0')
expect "no container restarted (panic was caught)" "restarts $restarts_before -> $restarts_after" [ "$restarts_after" -le "$restarts_before" ]

log "waiting up to 120 s for the HeartsUnitsFailed alert (Prometheus scrapes every 15 s, rules evaluate about every 30 s)..."
state=unknown
for _ in $(seq 1 24); do
  state=$(alert_state HeartsUnitsFailed)
  [ "$state" = firing ] || [ "$state" = unknown ] && break
  sleep 5
done
case "$state" in
  firing) expect "alert HeartsUnitsFailed is firing" "state=firing" true ;;
  unknown) note "alert HeartsUnitsFailed" "Prometheus API not reachable via kubectl service proxy; check the Alerts page in Grafana or Prometheus by hand" ;;
  *) expect "alert HeartsUnitsFailed is firing" "state=$state" false ;;
esac

fault_off
wait_workers_active "$GD_REPLICAS" 120 || true
rjob=$(ref_submit) || exit 1
log "recovery run: reference job $rjob"
ref_check "$rjob"
note "the alert stays firing for up to 10 minutes" "its window is increase(hearts_units_failed_total[10m]); this is expected"

gd_finish
