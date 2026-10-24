# Benchmark scripts

Tools for the scaling experiments E1 to E7 described in `docs/EXPERIMENTS.md`.
They talk to the coordinator's REST API (`BASE_URL`, default
`https://hearts.k8s.ad.gavinf.com`) and to the cluster with `kubectl`
(namespace `hearts`). Needed on your PATH: `curl`, `jq`, `kubectl`.

| File | What it does |
|---|---|
| `run-matrix.sh` | Runs one experiment over a list of worker configurations and records each result via `POST /api/benchmarks`. |
| `report.sh` | Prints a table and CSV for an experiment, with speedup and efficiency relative to the first row. |
| `advise-latency.sh` | Fires `POST /api/advise` requests and prints p50/p95/p99 (experiment E6). |
| `noisy-neighbor.yaml` | A CPU-burning pod to co-locate with a worker (experiment E7). |

## run-matrix.sh

```
scripts/bench/run-matrix.sh EXPERIMENT replicas:cpu:threads:unit_size [...]

# horizontal scaling, 2 CPU per pod
scripts/bench/run-matrix.sh E2-horizontal 1:2:0:2000 2:2:0:2000 3:2:0:2000 6:2:0:2000
```

Config fields: pods, CPU per pod (request == limit), `WORKER_THREADS` (`0` = follow the CPU limit, i.e. the default), and the job's `unit_size`.

What happens per config:

1. The `hearts-worker` Deployment is patched (replicas, requests == limits for CPU and memory, `WORKER_THREADS`).
2. `kubectl rollout status` is awaited. If it does not complete in `ROLLOUT_TIMEOUT` seconds (for example pods stay `Pending` with `Insufficient cpu`), the config is skipped and listed in the summary.
3. `/api/workers` must show exactly that many active workers.
4. A short warm-up job runs (not recorded; `WARMUP=0` disables it).
5. A fixed-seed job runs and its result is POSTed to `/api/benchmarks` together with the git sha.
6. At exit (even on error or Ctrl-C) the original Deployment spec (replicas, image, resources, env) is restored.

Environment knobs:

| Variable | Default | Meaning |
|---|---|---|
| `BENCH_STRATEGIES` | `heuristic,heuristic,random,pimc:20` | four strategies |
| `BENCH_HANDS` | `300000` | hands per run. Calibrate so 3 pods x 2 CPU takes about 60 s |
| `BENCH_SEED` | `42` | job seed (the same seed gives identical standings in every config) |
| `BENCH_MEM` | `1Gi` | memory request == limit |
| `BENCH_NOTES` | empty | free text stored in `notes` |
| `BENCH_REPEAT` | `1` | runs per config (`report.sh` averages repeats) |
| `ROLLOUT_TIMEOUT` | `240` | seconds to wait for rollout and for workers to register |
| `WAIT_TIMEOUT` | `3600` | seconds to wait for one job |
| `DRY_RUN` | `0` | `1` prints the plan only |
| `BASE_URL`, `NS` | see `scripts/lib.sh` | |

Tips:

- Do not run two experiments at once, and do not use the UI for other jobs meanwhile: they share the workers.
- Check the "CPU throttling % (per pod)" panel in Grafana (`Hearts Worker Performance`) during a run. A clean scaling experiment has it near zero.
- `threads_per_worker` in the stored row is the number the workers actually reported (so `0` is resolved), and the original setting is in `notes` as `WORKER_THREADS=<n>`.
- If the script is killed with `kill -9` the trap cannot run. Restore by hand with `make deploy` and `kubectl -n hearts set env deploy/hearts-worker WORKER_THREADS-`.

## report.sh

```
scripts/bench/report.sh E2-horizontal            # table + CSV
scripts/bench/report.sh E2-horizontal --csv-only > e2.csv
SINCE=2026-10-24T00:00:00Z scripts/bench/report.sh E2-horizontal
```

- `speedup` = hands/s relative to the first row (the oldest config).
- `efficiency` = speedup divided by the ratio of total cores (`replicas x cpu`). 1.0 is perfect linear scaling.

## advise-latency.sh

```
scripts/bench/advise-latency.sh -n 300 -s 2000           # sequential
CONC=8 scripts/bench/advise-latency.sh -n 400            # 8 concurrent clients
```

## Local smoke test (no cluster)

`run-matrix.sh` needs a cluster, but `report.sh` and `advise-latency.sh` work against the compose stack:

```
make dev                                  # in another terminal
BASE_URL=http://localhost:8080 scripts/bench/advise-latency.sh -n 50
```
