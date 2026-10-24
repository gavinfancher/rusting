#!/usr/bin/env bash
# Game day 1: kill a worker pod while it is computing a unit.
#
#   scripts/gamedays/01-kill-worker.sh [--yes]
#
# Knobs:  GRACEFUL=1   plain `kubectl delete pod` (SIGTERM, 45 s drain) instead of the
#                      default crash simulation (`--grace-period=0 --force`, i.e. SIGKILL).
#
# Hypothesis (docs/GAMEDAYS.md, drill 1): a SIGKILLed worker never reports its unit.
# Its lease (LEASE_SECONDS=30) expires, the next LeaseWork from ANY worker reclaims
# the unit (hearts_units_reclaimed_total +1, job.progress.reclaims >= 1), and the
# Deployment's ReplicaSet starts a replacement pod. Deterministic seeds make the
# redo identical, so the standings equal the reference.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

GRACEFUL="${GRACEFUL:-0}"
DESC="Submits the reference job, waits until a worker holds a lease, then kills that pod
($( [ "$GRACEFUL" = 1 ] && echo 'graceful SIGTERM: the worker finishes its unit first, expect 0 reclaims' || echo 'SIGKILL via --grace-period=0 --force: expect its lease to expire and the unit to be reclaimed' )).
Restores nothing: the ReplicaSet replaces the pod by itself."
gd_begin "01 Kill a worker mid-job" "$DESC" "$@"

before_reclaimed=$(coord_metric hearts_units_reclaimed_total)

job=$(ref_submit) || exit 1
log "submitted reference job $job"
watch_panel hearts-overview "Hearts Overview" "Unit event rates" \
  'sum(rate(hearts_units_reclaimed_total[1m]))  # spikes ~30 s after the kill'
watch_panel hearts-overview "Hearts Overview" "Active workers over time" \
  'max(hearts_workers_active{namespace="hearts"})  # drops by 1 about 30 s after the kill, then recovers'
watch_panel hearts-overview "Hearts Overview" "Units by state" \
  'sum by (state) (hearts_units{namespace="hearts"})  # the dead worker unit stays "leased" until its lease expires and someone reclaims it'
tips "kubectl -n $NS get pods -l app.kubernetes.io/name=hearts-worker -w" \
  "kubectl -n $NS get events --sort-by=.lastTimestamp | tail -20" \
  "curl -s $BASE_URL/api/workers | jq '.[] | {pod_name, active, leased_units}'" \
  "kubectl -n $NS logs deploy/hearts-coordinator --since=2m | grep -i lease"

victim=""
for _ in $(seq 1 60); do
  victim=$(busy_worker)
  [ -n "$victim" ] && break
  sleep 1
done
[ -n "$victim" ] || die "no worker ever held a lease (is the job running?)"
log "victim: $victim"

if [ "$GRACEFUL" = 1 ]; then
  kubectl -n "$NS" delete pod "$victim" --wait=false
else
  kubectl -n "$NS" delete pod "$victim" --grace-period=0 --force --wait=false 2>&1 | sed 's/^/    /'
fi
kill_epoch=$(date +%s)

ref_check "$job"
elapsed=$(($(date +%s) - kill_epoch))
note "time from kill to job done" "${elapsed}s"

if [ "$GRACEFUL" = 1 ]; then
  note "graceful kill" "reclaims=$REF_RECLAIMS (0 is expected: the unit finished during the drain)"
else
  expect "at least one unit was reclaimed (job.progress.reclaims)" "reclaims=$REF_RECLAIMS" [ "$REF_RECLAIMS" -ge 1 ]
  after_reclaimed=$(coord_metric hearts_units_reclaimed_total)
  expect "hearts_units_reclaimed_total increased" "$before_reclaimed -> $after_reclaimed" \
    [ "$after_reclaimed" -gt "$before_reclaimed" ]
fi

wait_until "replacement pod Ready (rollout complete)" 120 \
  kubectl -n "$NS" rollout status deploy/hearts-worker --timeout=5s
ready=$(kubectl -n "$NS" get deploy hearts-worker -o jsonpath='{.status.readyReplicas}')
expect "worker Deployment back to full strength" "ready=${ready:-0}/$GD_REPLICAS" [ "${ready:-0}" -eq "$GD_REPLICAS" ]

gd_finish
