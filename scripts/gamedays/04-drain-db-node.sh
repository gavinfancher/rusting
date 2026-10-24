#!/usr/bin/env bash
# Game day 4: drain the node that hosts Postgres.
#
#   scripts/gamedays/04-drain-db-node.sh [--yes]
#
# Knobs:  HOLD_SECONDS=45   how long to leave the node drained before uncordoning
#         FULL_DRAIN=1      drain EVERY pod on the node (also Prometheus/Grafana if they
#                           live there). Default: only pods labelled app.kubernetes.io/part-of=hearts
#                           (kubectl drain --pod-selector), which keeps the blast radius small.
#
# Hypothesis (docs/GAMEDAYS.md, drill 4): `kubectl drain` cordons the node (marks it
# unschedulable) and evicts the pods on it. Worker evictions respect the
# PodDisruptionBudget (maxUnavailable: 1), so the drain evicts them one at a time and
# waits for each replacement to become Ready on another node. hearts-db-0 is evicted too
# and the StatefulSet recreates it, but the new pod STAYS PENDING: its PVC
# (data-hearts-db-0, StorageClass local-path) is bound to a PersistentVolume with node
# affinity to THIS node, which is now cordoned. The scheduler reports
# "volume node affinity conflict". The database, and so the whole system, is down until
# you uncordon. Then the pod schedules, Postgres recovers, workers reclaim expired units,
# and the job finishes with the reference standings.
# This is the cost of local-path storage; the fix would be replicated storage or a DB operator.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

HOLD_SECONDS="${HOLD_SECONDS:-45}"
FULL_DRAIN="${FULL_DRAIN:-0}"
DESC="Finds the node running hearts-db-0, submits the reference job, then DRAINS that node
(cordon + evict). Expect hearts-db-0 to stay Pending (local-path PV is pinned to the node) and
the whole app to be unavailable for about ${HOLD_SECONDS}s. The node is uncordoned afterwards,
also if you press Ctrl-C."
gd_begin "04 Drain the node hosting Postgres" "$DESC" "$@"

NODE=$(db_node)
[ -n "$NODE" ] || die "cannot find the node of hearts-db-0"
if kubectl get node "$NODE" -o json | jq -e '.metadata.labels | has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master")' >/dev/null; then
  die "hearts-db-0 runs on control-plane node $NODE; refusing to drain it"
fi
log "Postgres runs on node $NODE"
echo "  Other pods on $NODE (a full drain would evict all of them):"
kubectl get pods -A --field-selector "spec.nodeName=$NODE" --no-headers -o custom-columns=NS:.metadata.namespace,NAME:.metadata.name 2>/dev/null | sed 's/^/    /'

uncordon_node() { kubectl uncordon "$NODE" >/dev/null 2>&1 || true; }
stop_drain() {
  if [ -n "${DRAIN_PID:-}" ]; then kill "$DRAIN_PID" 2>/dev/null || true; fi
}
gd_cleanup_add uncordon_node
gd_cleanup_add stop_drain

stretch_on
job=$(ref_submit) || exit 1
log "submitted reference job $job"
watch_panel hearts-postgres "Hearts Postgres" "Postgres up" 'min(pg_up{namespace="hearts"})  # drops to 0, then disappears'
watch_panel hearts-overview "Hearts Overview" "Active workers over time" 'max(hearts_workers_active{namespace="hearts"})'
watch_panel hearts-workers "Hearts Worker Performance" "Hands simulated/s per node" \
  'sum by (node) (rate(hearts_worker_hands_simulated_total{namespace="hearts"}[1m]) * on (namespace, pod) group_left (node) max by (namespace, pod, node) (kube_pod_info{namespace="hearts"}))'
tips "kubectl get nodes" \
  "kubectl -n $NS get pods -o wide -w" \
  "kubectl -n $NS describe pod hearts-db-0 | sed -n '/Events:/,\$p'    # FailedScheduling: volume node affinity conflict" \
  "kubectl get pv -o custom-columns=NAME:.metadata.name,CLAIM:.spec.claimRef.name,NODE:.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].values[0]" \
  "kubectl -n $NS get pdb   # ALLOWED DISRUPTIONS for the workers" \
  "kubectl -n $NS get events --sort-by=.lastTimestamp | tail -20"

wait_progress "$job" 3 180 || log "WARNING: fewer than 3 units finished in 180 s; draining anyway"

DRAIN_LOG=$(mktemp "${TMPDIR:-/tmp}/drain.XXXXXX")
drain_args=(--ignore-daemonsets --delete-emptydir-data --force --timeout=180s)
if [ "$FULL_DRAIN" != 1 ]; then drain_args+=(--pod-selector=app.kubernetes.io/part-of=hearts); fi
log "kubectl drain $NODE ${drain_args[*]}"
kubectl drain "$NODE" "${drain_args[@]}" >"$DRAIN_LOG" 2>&1 &
DRAIN_PID=$!

db_pending_on_affinity() {
  local phase msg
  phase=$(pod_phase hearts-db-0)
  [ "$phase" = Pending ] || return 1
  msg=$(kubectl -n "$NS" get pod hearts-db-0 -o jsonpath='{.status.conditions[?(@.type=="PodScheduled")].message}')
  case "$msg" in *"volume node affinity"*) log "scheduler says: $msg"; return 0 ;; esac
  return 1
}
if wait_until "hearts-db-0 is Pending with a volume node affinity conflict" 120 db_pending_on_affinity; then
  expect "Postgres pod is Pending because of PV node affinity" "see scheduler message above" true
else
  expect "Postgres pod is Pending because of PV node affinity" "pod was not Pending with that message within 120 s" false
fi
echo
echo "  WHY: the PV behind data-hearts-db-0 is a directory on $NODE's disk. Its nodeAffinity says"
echo "       'only $NODE'. The node is cordoned, so no node can satisfy both constraints."
echo

if curl -fsS --max-time 3 "$BASE_URL/readyz" >/dev/null 2>&1; then
  expect "coordinator is not ready while the DB is down" "/readyz still 200" false
else
  expect "coordinator is not ready while the DB is down" "/readyz fails" true
fi
note "job during the outage" "$(progress_line "$(job_json "$job" 2>/dev/null || echo '{"id":0,"status":"unreachable","progress":{"done":0,"units_total":0,"leased":0,"pending":0,"failed":0,"hands_done":0,"reclaims":0}}')")"

log "holding the drained state for ${HOLD_SECONDS}s (watch Grafana and kubectl get pods)..."
sleep "$HOLD_SECONDS"

wait "$DRAIN_PID" 2>/dev/null
drain_rc=$?
DRAIN_PID=""
note "kubectl drain exit code" "$drain_rc"
if grep -q "disruption budget" "$DRAIN_LOG"; then
  note "PodDisruptionBudget blocked at least one eviction (and drain retried)" "$(grep -c 'disruption budget' "$DRAIN_LOG") message(s)"
fi
sed 's/^/    drain: /' "$DRAIN_LOG" | tail -n 15
rm -f "$DRAIN_LOG"

log "uncordoning $NODE"
uncordon_node
wait_until "hearts-db-0 Running and Ready" 300 kubectl -n "$NS" wait --for=condition=Ready pod/hearts-db-0 --timeout=5s
wait_until "/readyz healthy again" 180 curl -fsS --max-time 3 "$BASE_URL/readyz"
expect "Postgres came back on the same node" "$(db_node) (was $NODE)" [ "$(db_node)" = "$NODE" ]

ref_check "$job"
note "units reclaimed because leases expired during the outage" "$REF_RECLAIMS"

gd_finish
