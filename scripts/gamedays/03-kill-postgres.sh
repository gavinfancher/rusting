#!/usr/bin/env bash
# Game day 3: kill the Postgres pod in the middle of a job.
#
#   scripts/gamedays/03-kill-postgres.sh [--yes]
#
# Knobs:  HARD=1   delete with --grace-period=0 --force (crash: Postgres must replay WAL)
#                  instead of a clean SIGTERM shutdown.
#
# Hypothesis (docs/GAMEDAYS.md, drill 3): hearts-db-0 is a StatefulSet pod with a
# local-path PVC. Deleting the pod makes the StatefulSet controller recreate a pod
# with the SAME name that re-attaches the SAME volume (so it goes back to the same
# node), and Postgres recovers its data. While it is down:
#   * the coordinator's /readyz fails (DB SELECT 1), so Traefik stops routing to it;
#   * workers' LeaseWork/CompleteWork fail and back off; results that cannot be
#     reported after 6 attempts are dropped (wasted work) and the unit is reclaimed
#     after its lease expires;
#   * sqlx's pool reconnects by itself, no coordinator restart is needed.
# No job or unit is lost, and the standings still match the reference.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

HARD="${HARD:-0}"
DESC="Stretches the units, submits the reference job, then deletes pod hearts-db-0
($( [ "$HARD" = 1 ] && echo 'forced, like a crash' || echo 'graceful shutdown' )). Watches readiness flip and recover, checks no job was lost and the
standings are still identical."
gd_begin "03 Kill the Postgres pod" "$DESC" "$@"

stretch_on
jobs_before=$(api_get '/api/jobs?limit=1000' | jq 'length')
db_uid_before=$(kubectl -n "$NS" get pod hearts-db-0 -o jsonpath='{.metadata.uid}')
node_before=$(db_node)
errs_before=$(worker_metric hearts_worker_lease_errors_total)

job=$(ref_submit) || exit 1
log "submitted reference job $job; hearts-db-0 runs on $node_before"
watch_panel hearts-postgres "Hearts Postgres" "Postgres up" 'min(pg_up{namespace="hearts"})'
watch_panel hearts-overview "Hearts Overview" "DB pool connections" \
  'sum by (state) (hearts_db_pool_connections{namespace="hearts"})'
watch_panel hearts-workers "Hearts Worker Performance" "Lease RPC errors/s and idle polls/s" \
  'sum by (rpc) (rate(hearts_worker_lease_errors_total{namespace="hearts"}[1m]))'
tips "kubectl -n $NS get pods -w" \
  "kubectl -n $NS describe pod hearts-db-0 | tail -25" \
  "kubectl -n $NS logs hearts-db-0 -c postgres --previous | tail -20   # the last words of the old container" \
  "kubectl -n $NS logs hearts-db-0 -c postgres | tail -20              # 'database system was interrupted' only with HARD=1" \
  "kubectl -n $NS logs deploy/hearts-coordinator --since=2m | grep -i -E 'error|database'" \
  "kubectl -n $NS get pvc,pv"

wait_progress "$job" 3 180 || log "WARNING: fewer than 3 units finished in 180 s; killing anyway"

if [ "$HARD" = 1 ]; then
  kubectl -n "$NS" delete pod hearts-db-0 --grace-period=0 --force --wait=false 2>&1 | sed 's/^/    /'
else
  kubectl -n "$NS" delete pod hearts-db-0 --wait=false
fi

readyz_failed=0
for _ in $(seq 1 45); do
  if ! curl -fsS --max-time 3 "$BASE_URL/readyz" >/dev/null 2>&1; then readyz_failed=1; break; fi
  sleep 1
done
expect "/readyz went unhealthy while the DB was gone" "observed=$readyz_failed" [ "$readyz_failed" = 1 ]

db_replaced() {
  local uid
  uid=$(kubectl -n "$NS" get pod hearts-db-0 -o jsonpath='{.metadata.uid}' 2>/dev/null) || return 1
  [ -n "$uid" ] && [ "$uid" != "$db_uid_before" ]
}
wait_until "StatefulSet created a new hearts-db-0 pod" 120 db_replaced
wait_until "hearts-db-0 Ready again" 240 \
  kubectl -n "$NS" wait --for=condition=Ready pod/hearts-db-0 --timeout=5s
wait_until "/readyz healthy again" 120 curl -fsS --max-time 3 "$BASE_URL/readyz"

db_uid_after=$(kubectl -n "$NS" get pod hearts-db-0 -o jsonpath='{.metadata.uid}')
node_after=$(db_node)
expect "a new pod (different UID) took over" "uid changed" [ "$db_uid_before" != "$db_uid_after" ]
expect "it came back on the same node (PV node affinity)" "$node_before -> $node_after" [ "$node_before" = "$node_after" ]

ref_check "$job"

jobs_after=$(api_get '/api/jobs?limit=1000' | jq 'length')
expect "no job rows were lost" "$jobs_before -> $jobs_after" [ "$jobs_after" -ge "$jobs_before" ]
errs_after=$(worker_metric hearts_worker_lease_errors_total)
note "worker RPC errors during the outage" "$errs_before -> $errs_after"

gd_finish
