#!/usr/bin/env bash
# Game day 7: bad deploy (image tag that does not exist), then `kubectl rollout undo`.
#
#   scripts/gamedays/07-bad-deploy.sh [--yes]
#
# Hypothesis (docs/GAMEDAYS.md, drill 7): the worker Deployment uses the default
# RollingUpdate strategy (maxSurge 25% rounded up, maxUnavailable 25% rounded down: with 3
# replicas that is surge 1 / unavailable 0). So the controller first creates ONE new pod from the
# new ReplicaSet; it cannot pull the image (ErrImagePull, then ImagePullBackOff with growing
# delays) so it never becomes Ready, and the controller never scales the old ReplicaSet down.
# The old pods keep serving, the job finishes with the reference standings, and the rollout just
# sits there (it would be declared failed after progressDeadlineSeconds = 600 s). Nothing is
# automatically rolled back: `kubectl rollout undo` (or fixing the tag) is a human decision.
# Contrast: the coordinator uses Recreate; a bad coordinator image WOULD take the API down because
# the old pod is stopped first.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

DESC="Stretches the units, submits the reference job, then sets the worker image to a tag that does not
exist. Watches the new pod fail with ErrImagePull / ImagePullBackOff while the old pods keep
working, then runs 'kubectl rollout undo' and checks everything is healthy again."
gd_begin "07 Bad deploy" "$DESC" "$@"

worker_spec_save
good_image=$(jq -r '.image' "$WORKER_SPEC_FILE")
bad_image="${good_image%%:*}:does-not-exist-$(date +%s)"
restore_workers() {
  worker_spec_restore
  rollout_wait hearts-worker 300s
}
gd_cleanup_add restore_workers
stretch_on

job=$(ref_submit) || exit 1
log "submitted reference job $job"
watch_panel hearts-overview "Hearts Overview" "Active workers over time" \
  'max(hearts_workers_active{namespace="hearts"})  # stays at the old count: old pods are never scaled down'
watch_panel hearts-overview "Hearts Overview" "Hands completed per second" \
  'sum(rate(hearts_hands_completed_total{namespace="hearts"}[1m]))  # keeps flowing'
tips "kubectl -n $NS rollout status deploy/hearts-worker       # blocks: 'Waiting for deployment ... 1 out of 3 new replicas'" \
  "kubectl -n $NS get rs -l app.kubernetes.io/name=hearts-worker   # old RS keeps 3, new RS has 1 not ready" \
  "kubectl -n $NS get pods -l app.kubernetes.io/name=hearts-worker -w" \
  "kubectl -n $NS describe pod <new-pod> | sed -n '/Events:/,\$p'  # Failed to pull image ... not found" \
  "kubectl -n $NS rollout history deploy/hearts-worker" \
  "kubectl -n $NS get deploy hearts-worker -o jsonpath='{.spec.strategy}'"

wait_progress "$job" 2 120 || true
log "setting worker image to $bad_image"
kubectl -n "$NS" set image deploy/hearts-worker "worker=$bad_image" >/dev/null

bad_state() {
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o json |
    jq -r '[.items[].status.containerStatuses[]?.state.waiting.reason // empty
            | select(. == "ErrImagePull" or . == "ImagePullBackOff")] | first // empty'
}
reason=""
for _ in $(seq 1 60); do
  reason=$(bad_state)
  [ -n "$reason" ] && break
  sleep 2
done
expect "new pod failed to pull the image" "waiting reason=${reason:-none}" [ -n "$reason" ]

if kubectl -n "$NS" rollout status deploy/hearts-worker --timeout=20s >/dev/null 2>&1; then
  expect "rollout is stuck (does not complete)" "it completed?!" false
else
  expect "rollout is stuck (does not complete)" "rollout status timed out as expected" true
fi
ready=$(kubectl -n "$NS" get deploy hearts-worker -o jsonpath='{.status.readyReplicas}')
expect "old pods still serve (readyReplicas stays at the old count)" "ready=${ready:-0}/$GD_REPLICAS" [ "${ready:-0}" -ge "$GD_REPLICAS" ]
kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o wide >&2

ref_check "$job"

log "kubectl rollout undo"
kubectl -n "$NS" rollout undo deploy/hearts-worker
if rollout_wait hearts-worker 300s; then
  expect "rollout undo completed" "rollout status succeeded" true
else
  expect "rollout undo completed" "rollout status failed" false
fi
bad_left=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker -o json |
  jq --arg b "$bad_image" '[.items[] | select(.spec.containers[0].image == $b)] | length')
expect "no pod runs the bad image any more" "pods with bad image=$bad_left" [ "$bad_left" -eq 0 ]
note "rollout history" "$(kubectl -n "$NS" rollout history deploy/hearts-worker | tail -n 3 | tr '\n' ' ')"

gd_finish
