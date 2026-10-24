#!/usr/bin/env bash
# scripts/gamedays/lib.sh - shared scaffolding for the game-day drills.
#
# Every drill script follows the same shape:
#
#   . "$DIR/lib.sh"
#   gd_begin "01 Kill a worker mid-job" "$DESCRIPTION" "$@"   # banner, confirm, preflight, traps
#   ... submit the reference job, trigger the fault ...
#   watch_panel DASHBOARD_UID "Dashboard" "Panel" 'PromQL'          # tell the human what to look at
#   ... wait ...
#   expect "label" "detail" [ test ]                          # pass criteria
#   gd_finish                                                 # cleanup, then PASS or FAIL
#
# Safety rules built in:
#   * nothing happens before you confirm (or pass --yes);
#   * every fault registers a cleanup with gd_cleanup_add, and the cleanups run
#     (in reverse order) on normal exit, on error and on Ctrl-C;
#   * the final line is always PASS, FAIL or ABORTED.
#
# Drills never use anything but kubectl and the public REST API, and only touch
# the `hearts` namespace.

GD_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
. "$GD_DIR/../lib.sh"

# The reference job. Its standings are the oracle for every drill: the engine is
# deterministic, so ANY correct run, however many pods died on the way, must end
# with exactly these points per strategy slot. Same result as
#   hearts-cli tournament --hands 100000 --seed 42 --strategies heuristic,heuristic,random,pimc:20
REF_JOB='{"name":"gameday-reference","strategies":["heuristic","heuristic","random","pimc:20"],"num_hands":100000,"seed":42,"unit_size":2000}'
REF_POINTS='[481853,479038,1459749,381744]'

STRETCH_MS="${STRETCH_MS:-1500}"   # per-unit sleep used by stretch_on to keep a job running long enough
GD_JOB_TIMEOUT="${GD_JOB_TIMEOUT:-900}"

GD_YES=0
GD_TITLE=""
GD_PASSED=0
GD_FAILED=0
GD_DONE=0
GD_CLEANED=0
GD_CLEANUPS=()
GD_START_EPOCH=0
GD_START_MS=0
GD_REPLICAS=0

# ------------------------------------------------------------ lifecycle ----

gd_usage() {
  echo "usage: $(basename "$0") [--yes|-y]"
  echo "  --yes  do not ask for confirmation"
  echo "environment: BASE_URL ($BASE_URL), NS ($NS), GRAFANA_URL, and drill-specific knobs (see the top of the script)"
}

# gd_begin TITLE DESCRIPTION [ARGS...]
gd_begin() {
  GD_TITLE="$1"
  local desc="$2"
  shift 2
  local a
  for a in "$@"; do
    case "$a" in
      --yes | -y) GD_YES=1 ;;
      -h | --help) gd_usage; GD_DONE=1; exit 0 ;;
      *) gd_usage; GD_DONE=1; exit 2 ;;
    esac
  done
  require curl jq kubectl
  echo
  echo "================================================================================"
  echo " GAME DAY: $GD_TITLE"
  echo "================================================================================"
  echo "$desc"
  echo
  echo "  target: namespace '$NS', API $BASE_URL"
  echo
  gd_confirm
  trap gd_on_exit EXIT
  trap 'log "interrupted"; exit 130' INT TERM
  gd_preflight
  GD_START_EPOCH=$(date +%s)
  GD_START_MS=$((GD_START_EPOCH * 1000))
  log "START $(date -u -r "$GD_START_EPOCH" +%FT%TZ 2>/dev/null || date -u +%FT%TZ): Grafana time range starts here (from=$GD_START_MS)"
}

gd_confirm() {
  if [ "$GD_YES" = 1 ]; then return 0; fi
  if [ ! -t 0 ]; then
    echo "stdin is not a terminal; re-run with --yes to skip the confirmation." >&2
    GD_DONE=1
    exit 2
  fi
  local ans
  read -r -p "Proceed with this drill? [y/N] " ans
  case "$ans" in
    y | Y | yes | YES) ;;
    *) echo "aborted, nothing was changed."; GD_DONE=1; exit 1 ;;
  esac
}

# gd_preflight: refuse to start on a cluster that is not healthy to begin with.
gd_preflight() {
  log "preflight checks..."
  kubectl -n "$NS" get deploy hearts-worker hearts-coordinator >/dev/null ||
    die "cannot read the hearts Deployments; is kubectl pointed at the right cluster?"
  api_get /healthz >/dev/null 2>&1 || die "$BASE_URL/healthz is not answering"
  api_get /readyz >/dev/null 2>&1 || die "$BASE_URL/readyz is failing (database down?); fix that first"
  GD_REPLICAS=$(kubectl -n "$NS" get deploy hearts-worker -o jsonpath='{.spec.replicas}')
  local active
  active=$(workers_active)
  [ "$active" -ge 1 ] || die "no active workers in /api/workers"
  log "ok: $active active worker(s), Deployment replicas=$GD_REPLICAS"
  # leftover overrides from an earlier drill would invalidate this one
  local leftovers
  leftovers=$(kubectl -n "$NS" get deploy hearts-worker hearts-coordinator -o json |
    jq -r '.items[] | .metadata.name as $d | .spec.template.spec.containers[0].env[]?
           | select(.name | test("^(FAULT_|WORKER_THREADS|LEASE_SECONDS|MAX_ATTEMPTS)"))
           | "\($d): \(.name)=\(.value)"')
  if [ -n "$leftovers" ]; then
    log "WARNING: leftover env overrides found (from an earlier run?):"
    printf '    %s\n' "$leftovers" >&2
    log "         remove with: kubectl -n $NS set env deploy/<name> VAR-"
  fi
}

# gd_cleanup_add FUNCTION   register a function to run at exit (LIFO)
gd_cleanup_add() { GD_CLEANUPS+=("$1"); }

gd_run_cleanups() {
  if [ "$GD_CLEANED" = 1 ]; then return 0; fi
  GD_CLEANED=1
  local i fn
  if [ "${#GD_CLEANUPS[@]}" -gt 0 ]; then
    log "--- cleanup: restoring cluster state ---"
    for ((i = ${#GD_CLEANUPS[@]} - 1; i >= 0; i--)); do
      fn="${GD_CLEANUPS[$i]}"
      "$fn" || log "WARNING: cleanup '$fn' failed; check the cluster by hand"
    done
  fi
}

gd_on_exit() {
  local rc=$?
  trap - EXIT INT TERM
  gd_run_cleanups
  if [ "$GD_DONE" != 1 ]; then
    echo
    echo "ABORTED: $GD_TITLE (exit code $rc). Cleanup has run; verify with: kubectl -n $NS get pods"
    exit "$rc"
  fi
}

# gd_finish   run cleanups, then print the verdict and exit 0 (PASS) or 1 (FAIL)
gd_finish() {
  trap - EXIT INT TERM
  gd_run_cleanups
  GD_DONE=1
  local secs=$(($(date +%s) - GD_START_EPOCH))
  echo
  echo "--------------------------------------------------------------------------------"
  echo " $GD_TITLE: $GD_PASSED check(s) passed, $GD_FAILED failed, ${secs}s elapsed"
  echo "--------------------------------------------------------------------------------"
  if [ "$GD_FAILED" -eq 0 ]; then
    echo "PASS"
    exit 0
  fi
  echo "FAIL"
  exit 1
}

# ----------------------------------------------------------- assertions ----

# expect LABEL DETAIL COMMAND...   passes when COMMAND exits 0, e.g.  expect "reclaims >= 1" "got $n" [ "$n" -ge 1 ]
expect() {
  local label="$1" detail="$2"
  shift 2
  if "$@"; then
    GD_PASSED=$((GD_PASSED + 1))
    printf '  [ ok ] %s (%s)\n' "$label" "$detail"
  else
    GD_FAILED=$((GD_FAILED + 1))
    printf '  [FAIL] %s (%s)\n' "$label" "$detail"
  fi
}

# note LABEL DETAIL   informational, never fails the drill
note() { printf '  [info] %s (%s)\n' "$1" "$2"; }

# wait_until DESCRIPTION TIMEOUT_SECONDS COMMAND...   poll every 2 s until COMMAND succeeds
wait_until() {
  local desc="$1" timeout="$2" start
  shift 2
  start=$(date +%s)
  while ! "$@" >/dev/null 2>&1; do
    if [ $(($(date +%s) - start)) -ge "$timeout" ]; then
      log "timeout (${timeout}s) waiting for: $desc"
      return 1
    fi
    sleep 2
  done
  log "ok: $desc"
}

# ------------------------------------------------------- observation help ----

# watch_panel DASHBOARD_UID DASHBOARD_TITLE PANEL_TITLE PROMQL
watch_panel() {
  echo
  echo "  WATCH  dashboard \"$2\" -> panel \"$3\""
  echo "         $GRAFANA_URL/d/$1?from=$GD_START_MS&to=now&refresh=10s"
  echo "         PromQL (paste into Grafana Explore or the Prometheus UI):"
  echo "           $4"
  echo
}

# tips TEXT...   print diagnosis commands worth practising during the drill
tips() {
  echo "  PRACTISE (run these in another terminal now):"
  local t
  for t in "$@"; do echo "    $t"; done
  echo
}

# ---------------------------------------------------------- job helpers ----

# ref_submit   submit the reference job, print its id
ref_submit() { submit_job "$REF_JOB"; }

# job_field ID JQ_FILTER   e.g. job_field 12 .progress.reclaims
job_field() { job_json "$1" | jq -r "$2"; }

# ref_check ID   wait for the job, then assert: done, standings == reference.
# Sets REF_RECLAIMS and REF_ELAPSED_MS for further checks.
REF_RECLAIMS=0
REF_ELAPSED_MS=0
ref_check() {
  local id="$1" pts j
  wait_job "$id" "$GD_JOB_TIMEOUT" || true
  expect "job $id reached status done" "status=$WAIT_STATUS" [ "$WAIT_STATUS" = done ]
  if [ "$WAIT_STATUS" = done ]; then
    j=$(job_json "$id")
    pts=$(standings_points "$j")
    expect "standings identical to the reference" "got $pts, want $REF_POINTS" [ "$pts" = "$REF_POINTS" ]
    REF_RECLAIMS=$(jq -r '.progress.reclaims' <<<"$j")
    REF_ELAPSED_MS=$(jq -r '.elapsed_ms // 0' <<<"$j")
    note "job summary" "reclaims=$REF_RECLAIMS, elapsed=${REF_ELAPSED_MS}ms"
  fi
}

# wait_progress ID MIN_DONE [TIMEOUT]   wait until the job has at least MIN_DONE units done
wait_progress() {
  local id="$1" want="$2" timeout="${3:-120}" start d
  start=$(date +%s)
  while :; do
    d=$(job_field "$id" .progress.done 2>/dev/null || echo 0)
    [ "${d:-0}" -ge "$want" ] 2>/dev/null && return 0
    [ $(($(date +%s) - start)) -lt "$timeout" ] || return 1
    sleep 1
  done
}

# ------------------------------------------------- stretch the job runtime ----
# The reference job is deliberately cheap (about 15 to 60 s on 3 workers), which
# is too short for faults that take a while to land. stretch_on adds FAULT_SLOW_MS
# (a sleep at the start of every unit) to all workers so the job runs longer. It
# does not change results. It is removed again by the registered cleanup.

stretch_off() {
  log "removing the FAULT_SLOW_MS override from the workers"
  kubectl -n "$NS" set env deploy/hearts-worker FAULT_SLOW_MS- >/dev/null &&
    rollout_wait hearts-worker 300s
}

stretch_on() {
  log "stretching units by ${STRETCH_MS}ms each (FAULT_SLOW_MS) so the job outlives the fault..."
  gd_cleanup_add stretch_off
  kubectl -n "$NS" set env deploy/hearts-worker "FAULT_SLOW_MS=$STRETCH_MS" >/dev/null
  rollout_wait hearts-worker 300s
  wait_workers_active "$GD_REPLICAS" 120 || true
}

# busy_worker   pod name of an active worker that currently holds a lease (empty if none)
busy_worker() {
  api_get /api/workers 2>/dev/null |
    jq -r '[.[] | select(.active and .leased_units > 0)][0].pod_name // empty' 2>/dev/null
}

pod_phase() { kubectl -n "$NS" get pod "$1" -o jsonpath='{.status.phase}' 2>/dev/null; }

# db_node   the node hosting hearts-db-0
db_node() { kubectl -n "$NS" get pod hearts-db-0 -o jsonpath='{.spec.nodeName}'; }
