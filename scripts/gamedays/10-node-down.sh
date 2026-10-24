#!/usr/bin/env bash
# Game day 10: a whole node goes down (MANUAL drill).
#
#   scripts/gamedays/10-node-down.sh [--yes]
#
# This drill is not automated: powering off a VM or a machine is done in your hypervisor / on
# the hardware, and no script should do it for you. This script
#   * prints the step-by-step instructions and what to watch;
#   * when run interactively (without --yes) walks you through it: it submits the reference job
#     when you say the node is off, waits for the job, checks the standings and PASS/FAIL;
#   * with --yes (or without a terminal) it only prints the instructions and exits.
#
# Hypothesis (docs/GAMEDAYS.md, drill 10): when a node dies the control plane learns it only
# through missing kubelet heartbeats. After node-monitor-grace-period (40 s by default) the node
# becomes NotReady and its pods are marked not-Ready (so Services stop routing to them), but
# Kubernetes does NOT delete them for another 300 s (the default tolerationSeconds of the
# node.kubernetes.io/unreachable:NoExecute taint). Replacement worker pods appear only after that.
# The job, however, does not wait for Kubernetes: leases on the dead node's units expire after
# 30 s and the surviving workers reclaim them, so the job finishes on the remaining pods with
# the reference standings, well before any pod is rescheduled.
# If the node hosts Postgres, everything stops (drill 4's reasoning, plus the pod can never be
# force-moved because its PV is on the dead disk and a StatefulSet never runs two copies).
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

DBN=$(db_node 2>/dev/null || echo "?")
cat <<EOF

================================================================================
 GAME DAY 10: Whole node down (manual)
================================================================================
 Postgres currently runs on: $DBN
 Worker pods and their nodes:
$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o wide --no-headers 2>/dev/null | awk '{print "   " $1 "  " $7}')

 PART A (recommended first): power off a node that does NOT host Postgres
 ------------------------------------------------------------------------
 1. Pick a worker node other than $DBN, e.g. k8s-w2. Never the control plane (k8s-cp).
 2. In a terminal, start watching:
      kubectl get nodes -w
      kubectl -n $NS get pods -o wide -w
 3. Open Grafana ($GRAFANA_URL):
      - "Hearts Overview" -> "Active workers over time"   (drops, then recovers)
      - "Hearts Overview" -> "Units by state" and "Unit event rates" (reclaims after about 30 s)
      - "Hearts Worker Performance" -> "Hands simulated/s per node" (the dead node's line stops)
    PromQL:  max(hearts_workers_active{namespace="hearts"})
             sum(increase(hearts_units_reclaimed_total{namespace="hearts"}[5m]))
             kube_node_status_condition{condition="Ready", status="true"}
 4. Submit the reference job (this script does it for you in interactive mode):
      curl -s -H 'Content-Type: application/json' -d '$REF_JOB' $BASE_URL/api/jobs | jq .id
 5. Power the node off (hypervisor "power off", not a graceful shutdown; or pull the power).
 6. Observe, with timestamps:
      t+0s     the pods on the node still show Running/Ready
      t+~40s   'kubectl get nodes' shows NotReady (node-monitor-grace-period)
      t+~40s   those pods flip to Ready=false and leave the Service endpoints
      t+30s..  leased units expire; 'job.progress.reclaims' > 0; the job continues on other pods
      t+~5min  the NoExecute taint's tolerationSeconds=300 elapses: pods go Terminating/Unknown
               and the Deployment creates replacements on the other nodes (if they fit: requests
               are 2 CPU each, check 'kubectl -n $NS get pods' for Pending / Insufficient cpu)
 7. Diagnose:
      kubectl describe node <node> | sed -n '/Conditions:/,/Addresses:/p;/Taints:/p'
      kubectl get events -A --sort-by=.lastTimestamp | tail -30
      kubectl -n $NS get pods -o wide | grep <node>
 8. Power the node back on. It rejoins; stuck Terminating pods are deleted; the cluster
    converges. Check:  kubectl get nodes ; kubectl -n $NS get pods -o wide

 PART B (later, bring snacks): power off the node that hosts Postgres ($DBN)
 ------------------------------------------------------------------------
 Same steps. Additional observations:
      - /readyz fails, the UI is down, workers back off (see drill 3 and 4);
      - hearts-db-0 goes Unknown/Terminating and its replacement can NOT start anywhere else
        (the StatefulSet will not run two pods with the same identity, and the PV is local);
      - there is no fix other than getting the node back (or restoring from a pg_dump backup
        to a new PVC: docs/RUNBOOK.md, "Backup and restore");
      - after power-on Postgres recovers its WAL and the job completes with identical standings.

 PASS CRITERIA
 ------------------------------------------------------------------------
   A: the reference job finishes 'done' with standings $REF_POINTS, using only the surviving pods;
      reclaims >= 1 if the dead node held a leased unit; all pods Ready after power-on.
   B: all of the above after power-on, and you can explain why the DB pod could not move.

 QUESTIONS: why did the job finish BEFORE Kubernetes replaced the pods? Which timer decided
 each step (40 s, 30 s, 300 s)? What would change with tolerationSeconds: 30 on the workers?
================================================================================
EOF

if [ "$GD_YES" = 1 ] || [ ! -t 0 ] || [ "${1:-}" = "--yes" ] || [ "${1:-}" = "-y" ]; then
  echo "(non-interactive: instructions only)"
  exit 0
fi

require curl jq kubectl
GD_TITLE="10 Whole node down (guided)"
read -r -p "Do you want the guided mode (submits the job and checks the result)? [y/N] " ans
case "$ans" in y | Y | yes) ;; *) echo "ok, follow the steps above by hand."; exit 0 ;; esac

trap gd_on_exit EXIT
trap 'log "interrupted"; exit 130' INT TERM
gd_preflight
GD_START_EPOCH=$(date +%s)
GD_START_MS=$((GD_START_EPOCH * 1000))
echo
echo "Grafana from this moment: $GRAFANA_URL/d/hearts-overview?from=$GD_START_MS&to=now&refresh=10s"
echo "Node to power off must be a worker node WITHOUT Postgres (Part A)."
read -r -p "Press Enter to submit the reference job, then power the node off immediately... " _
job=$(ref_submit) || exit 1
log "reference job $job is running: POWER OFF THE NODE NOW"
wait_job "$job" 1800 || true
ref_check_after() {
  expect "job reached done on the surviving pods" "status=$WAIT_STATUS" [ "$WAIT_STATUS" = done ]
  if [ "$WAIT_STATUS" = done ]; then
    local pts rec
    pts=$(standings_points "$job")
    rec=$(job_field "$job" .progress.reclaims)
    expect "standings identical to the reference" "got $pts" [ "$pts" = "$REF_POINTS" ]
    note "units reclaimed (leases of the dead node's units expired)" "$rec"
  fi
}
ref_check_after
echo
read -r -p "Power the node back on, then press Enter to check the cluster converged... " _
wait_until "all nodes Ready" 600 bash -c "! kubectl get nodes --no-headers | grep -q NotReady"
wait_until "worker Deployment fully ready" 600 kubectl -n "$NS" rollout status deploy/hearts-worker --timeout=5s
wait_until "API ready" 300 curl -fsS --max-time 3 "$BASE_URL/readyz"
GD_REPLICAS=$(kubectl -n "$NS" get deploy hearts-worker -o jsonpath='{.spec.replicas}')
wait_workers_active "$GD_REPLICAS" 180 || true
active=$(workers_active)
expect "all workers active again" "$active/$GD_REPLICAS" [ "$active" -eq "$GD_REPLICAS" ]
gd_finish
