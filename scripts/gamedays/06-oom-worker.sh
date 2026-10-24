#!/usr/bin/env bash
# Game day 6: OOM a worker (memory limit too small).
#
#   scripts/gamedays/06-oom-worker.sh [--yes]
#
# Knobs:  MEM_LADDER="64Mi 32Mi 16Mi 8Mi"   limits to try, in order, until a pod is OOMKilled
#         OOM_WAIT=90                       seconds to wait for an OOM at each step
#
# Hypothesis (docs/GAMEDAYS.md, drill 6): memory, unlike CPU, is not compressible. The
# kernel cannot "throttle" a process that wants more RAM than its cgroup limit; the OOM killer
# SIGKILLs it and the kubelet records `lastState.terminated.reason: OOMKilled` (exit code 137).
# The kubelet restarts the container (restartPolicy Always) with exponential back-off: 10 s,
# 20 s, 40 s ... capped at 5 min, shown as STATUS CrashLoopBackOff between attempts. The restart
# counter feeds the HeartsPodCrashLooping alert (more than 2 restarts in 15 minutes).
# A killed worker loses its in-flight unit exactly like drill 1: the lease expires and the unit is
# reclaimed, so after the limit is restored the reference job still gives identical standings.
#
# How much memory does a worker really need? It is small: a few tens of MB (tokio, rayon stacks,
# metrics, gRPC buffers); work units hold only counters, not hands. So 64Mi may not be enough to
# trigger an OOM; the script then tightens the limit step by step. The limit at which it
# breaks is itself a finding: compare with the "Memory working set vs limit (per pod)" panel.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

MEM_LADDER="${MEM_LADDER:-64Mi 32Mi 16Mi 8Mi}"
OOM_WAIT="${OOM_WAIT:-90}"
HEAVY_JOB='{"name":"gameday-oom-heavy","strategies":["pimc:200","pimc:200","heuristic","random"],"num_hands":1000000,"seed":7,"unit_size":200}'
DESC="Starts a heavy PIMC job (pimc:200 x2), then lowers the workers' memory request=limit through
the steps: ${MEM_LADDER} until a worker container is OOMKilled. Shows restarts and CrashLoopBackOff,
then restores the original spec, cancels the heavy job and runs the reference job to prove the
standings are still identical."
gd_begin "06 OOM a worker" "$DESC" "$@"

worker_spec_save
cpu=$(jq -r '.resources.limits.cpu' "$WORKER_SPEC_FILE")
HEAVY_ID=""
cancel_heavy() {
  if [ -n "$HEAVY_ID" ]; then
    api_post "/api/jobs/$HEAVY_ID/cancel" '{}' >/dev/null 2>&1 && log "cancelled heavy job $HEAVY_ID"
  fi
  return 0
}
restore_workers() {
  worker_spec_restore
  rollout_wait hearts-worker 300s
}
gd_cleanup_add restore_workers
gd_cleanup_add cancel_heavy

# oom_count: number of worker containers whose current or last termination was OOMKilled
oom_count() {
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o json |
    jq '[.items[].status.containerStatuses[]?
         | select(.lastState.terminated.reason == "OOMKilled" or .state.terminated.reason == "OOMKilled")] | length'
}
restarts_total() {
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o json |
    jq '[.items[].status.containerStatuses[]?.restartCount] | add // 0'
}

watch_panel hearts-workers "Hearts Worker Performance" "OOMKilled (last terminated reason)" \
  'sum by (pod) (kube_pod_container_status_last_terminated_reason{namespace="hearts", pod=~"hearts-worker-.*", reason="OOMKilled"})'
watch_panel hearts-workers "Hearts Worker Performance" "Memory working set vs limit (per pod)" \
  'sum by (pod) (container_memory_working_set_bytes{namespace="hearts", pod=~"hearts-worker-.*", container="worker"})'
watch_panel hearts-workers "Hearts Worker Performance" "Pod restarts (last 1h)" \
  'sum by (pod) (increase(kube_pod_container_status_restarts_total{namespace="hearts", pod=~"hearts-worker-.*"}[1h]))'
tips "kubectl -n $NS get pods -l app.kubernetes.io/name=hearts-worker -w        # STATUS: OOMKilled, CrashLoopBackOff" \
  "kubectl -n $NS describe pod <pod> | sed -n '/Last State/,/Ready:/p'          # Reason: OOMKilled, Exit Code: 137" \
  "kubectl -n $NS logs <pod> --previous                                         # logs of the killed container" \
  "kubectl -n $NS get pod <pod> -o jsonpath='{.status.containerStatuses[0].lastState}'" \
  "kubectl -n $NS top pods -l app.kubernetes.io/name=hearts-worker              # memory before the squeeze"

HEAVY_ID=$(submit_job "$HEAVY_JOB") || exit 1
log "heavy job $HEAVY_ID submitted"

found=""
for lim in $MEM_LADDER; do
  log "--- setting memory request=limit=$lim (cpu stays $cpu) ---"
  patch_worker "$GD_REPLICAS" "$cpu" "$lim" 0
  start=$(date +%s)
  while [ $(($(date +%s) - start)) -lt "$OOM_WAIT" ]; do
    if [ "$(oom_count)" -gt 0 ]; then found="$lim"; break; fi
    sleep 3
  done
  [ -n "$found" ] && break
  log "no OOM within ${OOM_WAIT}s at $lim; the workers fit in that. Tightening..."
done

expect "a worker container was OOMKilled" "limit=${found:-none}" [ -n "$found" ]
if [ -n "$found" ]; then
  log "OOMKilled at $found. Let it crash-loop for a moment to observe the back-off..."
  sleep 30
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o wide >&2
  r=$(restarts_total)
  expect "containers were restarted by the kubelet" "restartCount total=$r" [ "$r" -ge 1 ]
  note "alert HeartsPodCrashLooping" "state=$(alert_state HeartsPodCrashLooping) (fires when restarts > 2 in 15 min)"
fi

cancel_heavy
HEAVY_ID=""
log "restoring the original spec"
restore_workers
wait_workers_active "$GD_REPLICAS" 180 || true
ready=$(kubectl -n "$NS" get deploy hearts-worker -o jsonpath='{.status.readyReplicas}')
expect "workers healthy again after restoring the limit" "ready=${ready:-0}/$GD_REPLICAS" [ "${ready:-0}" -eq "$GD_REPLICAS" ]

job=$(ref_submit) || exit 1
log "recovery run: reference job $job"
ref_check "$job"

gd_finish
