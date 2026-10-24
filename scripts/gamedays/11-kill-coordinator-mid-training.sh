#!/usr/bin/env bash
# Game day 11: kill the coordinator in the middle of a training generation.
#
#   scripts/gamedays/11-kill-coordinator-mid-training.sh [--yes]
#
# Hypothesis (docs/GAMEDAYS.md, drill 11): the trainer keeps ALL its state in Postgres
# (the CEM belief, the generation counter, every candidate and job row), and each step
# commits in one transaction. So killing the coordinator at any moment loses nothing:
# the new pod's trainer sees the same rows and carries on. The run finishes, there are
# no duplicate generations or jobs, and, because every step is deterministic, the
# champion is IDENTICAL to what `hearts-cli train` computes locally from the same config.
#
# Environment knobs: TRAIN_TIMEOUT (default 1200 s) for the whole run.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$DIR/lib.sh"

TRAIN_TIMEOUT="${TRAIN_TIMEOUT:-1200}"
# A small run: 6 generations x 8 candidates x 20k hands, then a 20k-hand validation.
POP=8; ELITES=2; GENS=6; HANDS=20000; UNIT=2000; VHANDS=20000; SEED=1; INIT=zero
TRAIN_BODY=$(jq -nc --argjson pop $POP --argjson el $ELITES --argjson g $GENS --argjson h $HANDS \
  --argjson u $UNIT --argjson v $VHANDS --argjson s $SEED --arg init $INIT \
  '{name:"gameday-11", population:$pop, elites:$el, generations:$g, hands_per_candidate:$h,
    unit_size:$u, validation_hands:$v, seed:$s, init:$init}')
CLI_CMD="hearts-cli train --init $INIT --population $POP --elites $ELITES --generations $GENS --hands $HANDS --validation-hands $VHANDS --seed $SEED"

DESC="Starts a small training run on the cluster, waits until it is part-way through generation 2
or later (some candidate jobs done, some not), then deletes the coordinator pod. Checks the run
resumes and finishes, creates no duplicate generations or jobs, and (if hearts-cli is on PATH)
that its champion equals the local 'hearts-cli train' result for the same config and seed."
gd_begin "11 Kill the coordinator mid-training" "$DESC" "$@"

run_id=""
cancel_run() {
  [ -n "$run_id" ] || return 0
  local st
  st=$(api_get "/api/training/$run_id" 2>/dev/null | jq -r '.status' 2>/dev/null || true)
  case "$st" in
    running | validating) log "cancelling training run $run_id"; api_post "/api/training/$run_id/cancel" '{}' >/dev/null 2>&1 || true ;;
  esac
}
gd_cleanup_add cancel_run

run_field() { api_get "/api/training/$run_id" | jq -r "$1"; }

old_pod=$(kubectl -n "$NS" get pod -l app.kubernetes.io/name=hearts-coordinator -o jsonpath='{.items[0].metadata.name}')
resp=$(api_post /api/training "$TRAIN_BODY") || die "POST /api/training failed (is this coordinator new enough?)"
run_id=$(jq -r '.id' <<<"$resp")
log "started training run $run_id; coordinator pod is $old_pod"
echo "  UI: $BASE_URL/training.html?id=$run_id"

watch_panel hearts-training "Hearts Training" "Fitness per generation" \
  'max(hearts_training_latest_fitness{namespace="hearts"})  # the curve continues after the restart'
watch_panel hearts-training "Hearts Training" "Generations per hour" \
  'sum(increase(hearts_training_generations_total{namespace="hearts"}[1h]))  # counter restarts from 0 in the new pod'
tips "kubectl -n $NS get pods -l app.kubernetes.io/name=hearts-coordinator -w" \
  "kubectl -n $NS logs deploy/hearts-coordinator | grep -i trainer   # the new pod's trainer picks the run up" \
  "curl -s $BASE_URL/api/training/$run_id | jq '{status,generation,current,last_error}'" \
  "kubectl -n $NS exec hearts-db-0 -- psql -U hearts -c 'select run_id, generation, count(*) from training_candidates group by 1,2 order by 2'"

# Wait until we are in the middle of a generation >= 2 (so there is real history to preserve).
mid_generation() {
  local g done total
  g=$(run_field .generation) || return 1
  [ "${g:-0}" -ge 2 ] || return 1
  done=$(run_field '.current.candidates_done // -1')
  total=$(run_field '.current.candidates_total // -1')
  [ "$done" -ge 1 ] && [ "$done" -lt "$total" ]
}
wait_until "run is mid-generation (generation >= 2, some candidates done)" "$TRAIN_TIMEOUT" mid_generation ||
  log "WARNING: never caught it mid-generation; killing anyway"
gen_before=$(run_field .generation)

log "deleting coordinator pod $old_pod (run is in generation $gen_before)"
kubectl -n "$NS" delete pod "$old_pod" --wait=false
wait_until "API answers /readyz again" 180 curl -fsS --max-time 3 "$BASE_URL/readyz" || true
new_pod=$(kubectl -n "$NS" get pod -l app.kubernetes.io/name=hearts-coordinator -o jsonpath='{.items[0].metadata.name}')
expect "a new coordinator pod is serving" "old=$old_pod new=$new_pod" [ "$new_pod" != "$old_pod" ]

terminal() { case "$(run_field .status)" in done | failed | cancelled) return 0 ;; *) return 1 ;; esac; }
wait_until "training run reaches a terminal status" "$TRAIN_TIMEOUT" terminal || true

status=$(run_field .status)
expect "run finished with status done" "status=$status" [ "$status" = done ]
if [ "$status" = done ]; then
  gens=$(run_field '.generations | length')
  expect "exactly $GENS generations recorded (no duplicates, none lost)" "got $gens" [ "$gens" = "$GENS" ]
  distinct=$(run_field '[.generations[].generation] | unique | length')
  expect "generation numbers are distinct" "distinct=$distinct" [ "$distinct" = "$GENS" ]
  jobs=$(api_get "/api/jobs?include_training=true&limit=500" | jq --argjson r "$run_id" '[.[] | select(.training_run_id == $r)] | length')
  want=$((GENS * POP + 2))
  expect "exactly $want tournament jobs (generations x population + 2 validation)" "got $jobs" [ "$jobs" = "$want" ]
  champion=$(run_field .champion.strategy)
  note "cluster champion" "$champion"
  note "validation" "$(run_field '.validation | "champion \(.champion_fitness) vs heuristic \(.baseline_fitness)"')"

  echo
  echo "  COMPARE with a local run (same config, same seed, must print the same champion):"
  echo "    $CLI_CMD"
  if command -v hearts-cli >/dev/null 2>&1; then
    log "hearts-cli found; running the local training (this may take a while)"
    local_out=$($CLI_CMD 2>/dev/null || true)
    local_champ=$(grep -o 'linear:[-0-9,]*' <<<"$local_out" | tail -1)
    expect "cluster champion equals hearts-cli train's champion" "local=$local_champ" [ "$local_champ" = "$champion" ]
  else
    note "hearts-cli not on PATH" "run the command above yourself and compare its final linear:... line with the cluster champion"
  fi
fi

gd_finish
