#!/usr/bin/env bash
# Game day 9: lease shorter than the unit runtime.
#
#   scripts/gamedays/09-short-lease.sh [--yes]
#
# Knobs:  LEASE=5          LEASE_SECONDS on the coordinator
#         SLOW_MS=15000    FAULT_SLOW_MS on the workers (extra sleep per unit)
#         KEEP_MAX_ATTEMPTS=1   do NOT raise MAX_ATTEMPTS (see below); expect failed units
#
# Hypothesis (docs/GAMEDAYS.md, drill 9): a lease is only a promise to be alive, renewed by a
# heartbeat every HEARTBEAT_SECONDS (10 s). With LEASE_SECONDS=5 a lease has already expired
# 5 s before the next heartbeat, and a unit takes 15 s. An expired lease is fair game: the
# next worker that calls LeaseWork (ORDER BY job_id, id picks expired units before fresh ones)
# reclaims it, so the same unit is computed by two workers at once. The slow original worker
# then either
#   * learns from its next heartbeat that the lease is lost (lost_unit_ids) and abandons the unit
#     at the next 50 ms slice (hearts_worker_wasted_units_total), or
#   * finishes first/late and its CompleteWork is rejected as `accepted=false`
#     (hearts_units_duplicate_total, hearts_worker_units_completed_total{result="duplicate"}).
# Either way only the first completion counts (idempotent UPDATE ... WHERE state <> 'done') and
# the seeds are deterministic, so the standings are STILL identical, only the cost goes up.
# The "Unit event rates" panel shows reclaims and duplicates; "Wasted units/s" is non-zero.
#
# Why we also raise MAX_ATTEMPTS: every reclaim is an attempt, and a unit that reaches
# MAX_ATTEMPTS=3 is marked failed. With such a short lease that can happen by bad luck, which would
# ruin the byte-for-byte comparison. Raising it to 10 isolates the lesson (wasted work, same
# result). Set KEEP_MAX_ATTEMPTS=1 to see units fail instead.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

LEASE="${LEASE:-5}"
SLOW_MS="${SLOW_MS:-15000}"
KEEP_MAX_ATTEMPTS="${KEEP_MAX_ATTEMPTS:-0}"
DESC="Sets LEASE_SECONDS=${LEASE} on the coordinator (it restarts: Recreate strategy) and FAULT_SLOW_MS=${SLOW_MS}
on the workers, then runs the reference job. It takes several minutes because every unit now takes
15 s. Expect reclaims, duplicate/wasted work and STILL identical standings.
Both overrides are removed afterwards."
gd_begin "09 Lease shorter than the unit runtime" "$DESC" "$@"

faults_off() {
  log "removing LEASE_SECONDS / MAX_ATTEMPTS from the coordinator and FAULT_SLOW_MS from the workers"
  kubectl -n "$NS" set env deploy/hearts-coordinator LEASE_SECONDS- MAX_ATTEMPTS- >/dev/null
  kubectl -n "$NS" set env deploy/hearts-worker FAULT_SLOW_MS- >/dev/null
  rollout_wait hearts-coordinator 300s
  rollout_wait hearts-worker 300s
}
gd_cleanup_add faults_off

coord_env=("LEASE_SECONDS=$LEASE")
if [ "$KEEP_MAX_ATTEMPTS" != 1 ]; then coord_env+=("MAX_ATTEMPTS=10"); fi
log "coordinator env: ${coord_env[*]}; workers: FAULT_SLOW_MS=$SLOW_MS"
kubectl -n "$NS" set env deploy/hearts-coordinator "${coord_env[@]}" >/dev/null
kubectl -n "$NS" set env deploy/hearts-worker "FAULT_SLOW_MS=$SLOW_MS" >/dev/null
rollout_wait hearts-coordinator 300s
rollout_wait hearts-worker 300s
wait_until "API ready" 120 curl -fsS --max-time 3 "$BASE_URL/readyz"
wait_workers_active "$GD_REPLICAS" 120 || true

watch_panel hearts-overview "Hearts Overview" "Unit event rates" \
  'sum(rate(hearts_units_reclaimed_total{namespace="hearts"}[1m])) and sum(rate(hearts_units_duplicate_total{namespace="hearts"}[1m]))'
watch_panel hearts-workers "Hearts Worker Performance" "Wasted units/s" \
  'sum(rate(hearts_worker_wasted_units_total{namespace="hearts"}[1m]))'
watch_panel hearts-workers "Hearts Worker Performance" "Worker results/s" \
  'sum by (result) (rate(hearts_worker_units_completed_total{namespace="hearts"}[1m]))  # result="duplicate"'
tips "kubectl -n $NS logs deploy/hearts-worker --since=2m | grep -E 'lease lost|abandoned|already done'" \
  "kubectl -n $NS get deploy hearts-coordinator -o jsonpath='{.spec.template.spec.containers[0].env}' | jq" \
  "make psql   then:  SELECT id, attempts, state FROM work_units WHERE attempts > 1 ORDER BY attempts DESC, id LIMIT 20;" \
  "curl -s $BASE_URL/api/jobs/<id> | jq .progress.reclaims"

sim_before=$(worker_metric hearts_worker_hands_simulated_total)
job=$(ref_submit) || exit 1
log "submitted reference job $job (expect several minutes)"
ref_check "$job"

reclaims_c=$(coord_metric hearts_units_reclaimed_total)
dups=$(coord_metric hearts_units_duplicate_total)
wasted=$(worker_metric hearts_worker_wasted_units_total)
sim_after=$(worker_metric hearts_worker_hands_simulated_total)
expect "units were reclaimed while their first worker was still alive" "job reclaims=$REF_RECLAIMS, counter=$reclaims_c" [ "$REF_RECLAIMS" -ge 1 ]
expect "duplicate or abandoned (wasted) work was observed" "duplicates=$dups, wasted units=$wasted" [ $((dups + wasted)) -ge 1 ]
amp=$(awk -v a="$sim_after" -v b="$sim_before" 'BEGIN { printf "%.2f", (a - b) / 100000 }')
note "hands simulated / hands needed" "x${amp} (close to 1: the slow sleep is cancelled before any hands are computed; a duplicate finishing a full unit pushes it above 1)"
note "alert HeartsHighReclaimRate" "state=$(alert_state HeartsHighReclaimRate)"

gd_finish
