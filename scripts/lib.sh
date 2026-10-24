#!/usr/bin/env bash
# scripts/lib.sh - shared helpers for the bench and game-day scripts.
#
# Source it, don't run it:   . "$(dirname "$0")/../lib.sh"
#
# Environment (all optional):
#   NS=hearts                      Kubernetes namespace of the app
#   BASE_URL=https://hearts.k8s.ad.gavinf.com
#                                  coordinator HTTP base URL. For the local
#                                  compose stack use BASE_URL=http://localhost:8080
#   GRAFANA_URL=https://grafana.k8s.ad.gavinf.com
#   POLL_SECONDS=2                 how often wait_job polls
#
# Conventions:
#   * Functions that "return" a value print it on stdout; all human-readable
#     messages go through `log`, which writes to STDERR, so `id=$(submit_job ...)`
#     captures only the id.
#   * Written for bash 3.2 (the macOS default): no associative arrays, no mapfile.

NS="${NS:-hearts}"
MON_NS="${MON_NS:-monitoring}"
BASE_URL="${BASE_URL:-https://hearts.k8s.ad.gavinf.com}"
BASE_URL="${BASE_URL%/}"
GRAFANA_URL="${GRAFANA_URL:-https://grafana.k8s.ad.gavinf.com}"
POLL_SECONDS="${POLL_SECONDS:-2}"
# Prometheus Service created by the kube-prometheus-stack Helm release.
PROM_SVC="${PROM_SVC:-kube-prometheus-stack-prometheus}"

# ---------------------------------------------------------------- logging ----

# log MESSAGE...   timestamped line on stderr
log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

die() { log "ERROR: $*"; exit 1; }

# require BIN...   fail early with a clear message if a tool is missing
require() {
  local missing=0 b
  for b in "$@"; do
    if ! command -v "$b" >/dev/null 2>&1; then
      log "ERROR: required program '$b' not found in PATH"
      missing=1
    fi
  done
  [ "$missing" -eq 0 ] || exit 1
}

# ------------------------------------------------------------- HTTP / API ----

# api_get /api/...   GET and print the body; non-zero exit on network error or HTTP >= 400
api_get() { curl -fsS --max-time 15 "$BASE_URL$1"; }

# api_post /api/... JSON   POST JSON, print the body; non-zero exit on error
api_post() {
  curl -fsS --max-time 30 -H 'Content-Type: application/json' -d "$2" "$BASE_URL$1"
}

# submit_job JSON   POST /api/jobs, print the new job id
submit_job() {
  local body="$1" resp code json
  resp=$(curl -sS --max-time 30 -H 'Content-Type: application/json' \
    -d "$body" -w '\n%{http_code}' "$BASE_URL/api/jobs") || {
    log "ERROR: POST $BASE_URL/api/jobs failed (network)"
    return 1
  }
  code="${resp##*$'\n'}"
  json="${resp%$'\n'*}"
  if [ "$code" != "201" ]; then
    log "ERROR: POST /api/jobs returned HTTP $code: $json"
    return 1
  fi
  jq -r '.id' <<<"$json"
}

# job_json ID   print the Job object
job_json() { api_get "/api/jobs/$1"; }

# standings_points ID|JSON  -> compact array of points by slot, e.g. [481853,479038,1459749,381744]
# Accepts either a job id or a Job JSON document.
standings_points() {
  local j="$1"
  case "$j" in
    '{'*) ;;
    *) j=$(job_json "$j") || return 1 ;;
  esac
  jq -c '[.standings | sort_by(.slot) | .[].points]' <<<"$j"
}

# progress_line JSON   one-line summary of a Job
progress_line() {
  jq -r '"job \(.id) \(.status): units \(.progress.done)/\(.progress.units_total) done, \(.progress.leased) leased, \(.progress.pending) pending, \(.progress.failed) failed | hands \(.progress.hands_done) | \((.hands_per_sec // 0) | floor) hands/s | reclaims \(.progress.reclaims)"' <<<"$1"
}

# wait_job ID [TIMEOUT_SECONDS=1800]
#   Poll until the job is terminal, printing a progress line whenever it changes.
#   Tolerates the API being unreachable (coordinator restarts are a normal part of
#   game days). Sets WAIT_STATUS to done|failed|cancelled|timeout.
#   Return code: 0 done, 2 failed/cancelled, 3 timeout.
WAIT_STATUS=""
wait_job() {
  local id="$1" timeout="${2:-1800}" start now j status line last=""
  start=$(date +%s)
  while :; do
    if j=$(job_json "$id" 2>/dev/null); then
      status=$(jq -r '.status' <<<"$j")
      line=$(progress_line "$j")
      if [ "$line" != "$last" ]; then log "$line"; last="$line"; fi
      case "$status" in
        done) WAIT_STATUS=done; return 0 ;;
        failed | cancelled) WAIT_STATUS="$status"; return 2 ;;
      esac
    else
      if [ "$last" != "unreachable" ]; then log "job $id: API unreachable, retrying..."; last="unreachable"; fi
    fi
    now=$(date +%s)
    if [ $((now - start)) -ge "$timeout" ]; then
      log "timeout after ${timeout}s waiting for job $id"
      WAIT_STATUS=timeout
      return 3
    fi
    sleep "$POLL_SECONDS"
  done
}

# workers_active   number of workers /api/workers reports as active (0 if unreachable)
workers_active() {
  api_get /api/workers 2>/dev/null | jq '[.[] | select(.active)] | length' 2>/dev/null || echo 0
}

# wait_workers_active N [TIMEOUT=180]   wait until exactly N workers are active
# (old pods stay "active" for up to 30s after they die, so this can take a while)
wait_workers_active() {
  local want="$1" timeout="${2:-180}" start now have
  start=$(date +%s)
  while :; do
    have=$(workers_active)
    [ "$have" = "$want" ] && return 0
    now=$(date +%s)
    if [ $((now - start)) -ge "$timeout" ]; then
      log "timeout: /api/workers shows $have active workers, wanted $want"
      return 1
    fi
    sleep "$POLL_SECONDS"
  done
}

# ---------------------------------------------------------------- metrics ----

# coord_metric NAME [LABEL_SUBSTRING]
#   Sum of all samples of NAME from the coordinator's /metrics (via BASE_URL).
#   Optional second arg: only lines containing that substring, e.g. 'state="pending"'.
#   Prints 0 if the endpoint is unreachable.
coord_metric() {
  local name="$1" filter="${2:-}"
  curl -fsS --max-time 10 "$BASE_URL/metrics" 2>/dev/null | awk -v n="$name" -v f="$filter" '
    (index($0, n "{") == 1 || index($0, n " ") == 1) && (f == "" || index($0, f) > 0) { s += $NF }
    END { printf "%.0f\n", s + 0 }'
}

# worker_pods   names of the worker pods (one per line)
worker_pods() {
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker \
    -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}'
}

# worker_metric NAME [LABEL_SUBSTRING]
#   Sum NAME over all worker pods. Workers are not exposed through the ingress, so
#   this goes through the Kubernetes API server's pod proxy (port 9090 /metrics).
worker_metric() {
  local name="$1" filter="${2:-}" pod
  for pod in $(worker_pods); do
    kubectl -n "$NS" get --raw "/api/v1/namespaces/$NS/pods/$pod:9090/proxy/metrics" 2>/dev/null
  done | awk -v n="$name" -v f="$filter" '
    (index($0, n "{") == 1 || index($0, n " ") == 1) && (f == "" || index($0, f) > 0) { s += $NF }
    END { printf "%.0f\n", s + 0 }'
}

# cfs_throttle_ratio   throttled_periods / total_periods summed over all worker pods
#   (container-lifetime totals, from the kubelet's cAdvisor endpoint). Prints e.g. 0.4213
cfs_throttle_ratio() {
  local node
  for node in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=hearts-worker \
    -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' | sort -u); do
    kubectl get --raw "/api/v1/nodes/$node/proxy/metrics/cadvisor" 2>/dev/null |
      grep -E "^container_cpu_cfs_(throttled_)?periods_total\{" |
      grep "namespace=\"$NS\"" | grep 'pod="hearts-worker-' | grep 'container="worker"'
  done | awk '
    /^container_cpu_cfs_throttled_periods_total/ { t += $NF }
    /^container_cpu_cfs_periods_total/ { p += $NF }
    END { if (p > 0) printf "%.4f\n", t / p; else print "0" }'
}

# prom_query PROMQL   instant query through the API-server service proxy; prints the
# first sample's value, or nothing if Prometheus is unreachable / has no data.
prom_query() {
  local q
  q=$(jq -rn --arg q "$1" '$q|@uri')
  kubectl get --raw "/api/v1/namespaces/$MON_NS/services/$PROM_SVC:9090/proxy/api/v1/query?query=$q" 2>/dev/null |
    jq -r '.data.result[0].value[1] // empty' 2>/dev/null
}

# alert_state NAME   prints firing|pending|inactive, or "unknown" if Prometheus can't be reached
alert_state() {
  local out
  out=$(kubectl get --raw "/api/v1/namespaces/$MON_NS/services/$PROM_SVC:9090/proxy/api/v1/alerts" 2>/dev/null) || {
    echo unknown
    return 0
  }
  jq -r --arg a "$1" '
    ([.data.alerts[] | select(.labels.alertname == $a) | .state]
      | if index("firing") then "firing" elif index("pending") then "pending" else "inactive" end)' \
    <<<"$out" 2>/dev/null || echo unknown
}

# ------------------------------------------------- worker Deployment helpers ----

WORKER_SPEC_FILE=""

# worker_spec_save   remember replicas, image, resources and env of the worker Deployment.
# Call once at the start; pair with worker_spec_restore in a trap.
worker_spec_save() {
  WORKER_SPEC_FILE=$(mktemp "${TMPDIR:-/tmp}/hearts-worker-spec.XXXXXX")
  kubectl -n "$NS" get deploy hearts-worker -o json |
    jq '{replicas: .spec.replicas,
         image: .spec.template.spec.containers[0].image,
         resources: .spec.template.spec.containers[0].resources,
         env: (.spec.template.spec.containers[0].env // [])}' >"$WORKER_SPEC_FILE"
  log "saved worker spec to $WORKER_SPEC_FILE: $(jq -c '{replicas, image, cpu: .resources.limits.cpu, memory: .resources.limits.memory}' "$WORKER_SPEC_FILE")"
}

# worker_spec_restore   put the saved spec back (one JSON patch => at most one rollout)
worker_spec_restore() {
  if [ -z "$WORKER_SPEC_FILE" ] || [ ! -s "$WORKER_SPEC_FILE" ]; then return 0; fi
  log "restoring the worker Deployment to its saved spec..."
  local patch
  patch=$(jq -c '[
      {op: "replace", path: "/spec/replicas", value: .replicas},
      {op: "replace", path: "/spec/template/spec/containers/0/image", value: .image},
      {op: "replace", path: "/spec/template/spec/containers/0/resources", value: .resources},
      {op: "replace", path: "/spec/template/spec/containers/0/env", value: .env}
    ]' "$WORKER_SPEC_FILE")
  kubectl -n "$NS" patch deploy hearts-worker --type=json -p "$patch" >/dev/null ||
    log "WARNING: restore patch failed; fix by hand: 'make deploy' and remove env overrides (see docs/RUNBOOK.md)"
}

# patch_worker REPLICAS CPU MEM THREADS
#   Set replicas, requests == limits for cpu and memory, and WORKER_THREADS
#   (0 = remove the override, i.e. follow the CPU limit). One patch, one rollout.
patch_worker() {
  local replicas="$1" cpu="$2" mem="$3" threads="$4" cur patch
  cur=$(kubectl -n "$NS" get deploy hearts-worker -o json)
  patch=$(jq -c --argjson r "$replicas" --arg cpu "$cpu" --arg mem "$mem" --argjson t "$threads" '
      .spec.template.spec.containers[0] as $c
      | ([($c.env // [])[] | select(.name != "WORKER_THREADS")]
         + (if $t > 0 then [{name: "WORKER_THREADS", value: ($t | tostring)}] else [] end)) as $env
      | [
          {op: "replace", path: "/spec/replicas", value: $r},
          {op: "replace", path: "/spec/template/spec/containers/0/resources",
           value: {requests: {cpu: $cpu, memory: $mem}, limits: {cpu: $cpu, memory: $mem}}},
          {op: "replace", path: "/spec/template/spec/containers/0/env", value: $env}
        ]' <<<"$cur")
  kubectl -n "$NS" patch deploy hearts-worker --type=json -p "$patch" >/dev/null
}

# rollout_wait DEPLOY [TIMEOUT=300s]
rollout_wait() {
  kubectl -n "$NS" rollout status "deploy/$1" --timeout="${2:-300s}" >&2
}

# git_sha   short sha of the images running in the cluster if they are tagged sha-xxxxxxx,
# otherwise the local repo's HEAD.
git_sha() {
  local img
  img=$(kubectl -n "$NS" get deploy hearts-worker -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true)
  case "$img" in
    *:sha-*) echo "${img##*:sha-}" ;;
    *) git rev-parse --short HEAD 2>/dev/null || echo unknown ;;
  esac
}
