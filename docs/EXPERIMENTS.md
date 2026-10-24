# Scaling experiments

Seven experiments that turn the cluster into a measuring instrument. Each has a hypothesis you write down **before** running it,
an exact command, panels to watch while it runs, a guide to interpreting the numbers, and an empty table to fill.

Tools: `scripts/bench/run-matrix.sh` (runs configs, records results in Postgres via `POST /api/benchmarks`), `scripts/bench/report.sh`
(table + CSV with speedup and efficiency), `scripts/bench/advise-latency.sh` (E6), `scripts/bench/noisy-neighbor.yaml` (E7). Details: `scripts/bench/README.md`.

- [Before you start](#before-you-start)
- [Concepts you will use](#concepts-you-will-use)
- [E1 Vertical scaling](#e1-vertical-scaling), [E2 Horizontal scaling](#e2-horizontal-scaling), [E3 Unit size](#e3-unit-size)
- [E4 Threads above the CPU limit](#e4-threads-above-the-cpu-limit), [E5 PIMC cost curve](#e5-pimc-cost-curve)
- [E6 Advisor fan-out latency](#e6-advisor-fan-out-latency-vs-shard-count), [E7 Noisy neighbour](#e7-noisy-neighbour)
- [How to present results](#how-to-present-results)

## Before you start

**Hygiene (a benchmark is only as good as its control of variables).**
- Nothing else running on the cluster: no other jobs, no drills, no leftover `FAULT_*` env (`kubectl -n hearts get deploy hearts-worker -o jsonpath='{.spec.template.spec.containers[0].env}'`).
- Fixed seed (42) and fixed strategies per experiment, so every config does **exactly the same work**. (You can even assert that: standings must be equal across configs, the same oracle as in the game days: `[481853, 479038, 1459749, 381744]` for the 100000-hand reference job; other hand counts give other numbers, but identical across configs.)
- Same image: `git_sha` is stored with each run; do not deploy in the middle of an experiment.
- Watch "CPU throttling % (per pod)" on `Hearts Worker Performance`: for E1, E2, E3, E5 and E7 it should stay near 0. If it does not, you are measuring the quota, not the code.
- Repeat each config at least 3 times (`BENCH_REPEAT=3`); `report.sh` averages repeats. Look at the spread: if runs differ by more than ~5 %, find out why before interpreting.

**Calibrate the workload.** The defaults are `BENCH_STRATEGIES=heuristic,heuristic,random,pimc:20` and `BENCH_HANDS=300000`. Run once
`scripts/bench/run-matrix.sh calibration 3:2:0:2000` and look at the wall time. Scale `BENCH_HANDS` so that 3 pods x 2 CPU take about 60 s (long enough to hide startup and tail effects, short enough to
repeat). The engine's per-thread speed: about 870k heuristic hands/s on an M4 Pro core (`docs/OPTIMIZATION_LOG.md`), and a `pimc:N` player costs about N x 30 heuristic hands per hand,
so `pimc:20` is roughly 600 heuristic hands ~ 0.7 ms per hand on that CPU; expect the amd64 VMs to be slower, perhaps 1.5 to 2 times. Wall time shrinks linearly with cores, so 1 pod x 1 CPU takes 6 times longer than 3 x 2.
(If you delete the `calibration` rows afterwards: `DELETE FROM benchmark_runs WHERE experiment = 'calibration';`.)

**What `run-matrix.sh` does to the cluster.** It patches the `hearts-worker` Deployment (replicas, requests == limits for CPU and memory, `WORKER_THREADS`), waits for the rollout and for `/api/workers` to show
exactly that many active workers, runs an unrecorded 2000-hand warm-up, runs the job, records the result, and finally restores the original spec (also on Ctrl-C). A config that cannot be scheduled (pods `Pending`,
for example 9 pods x 2 CPU) is **skipped** after `ROLLOUT_TIMEOUT` seconds, and the message `Insufficient cpu` is itself an experimental result: the cluster's allocatable CPU is finite.

**Cluster facts that shape the results.** Three worker nodes (k8s-w1..w3), 6 cores / 32 GB each, amd64, 1 GbE; the control-plane node is tainted and runs no workers. Allocatable CPU per worker node is a little under 6, because
system pods and the monitoring stack take requests, and Postgres (250m) and the coordinator (500m) live there too. So about **2 pods of 2 CPU per node, 6 pods in total**; larger configs may not fit.
Check what you have: `kubectl describe node k8s-w1 | sed -n '/Allocated resources/,/Events/p'`.

## Concepts you will use

**Speedup and efficiency.** Speedup S(n) = throughput(n) / throughput(1), where throughput is hands/s. Efficiency E(n) = S(n) / n for n times as many cores. `report.sh` computes both relative to the first row, using total cores (`replicas x cpu`).
Perfect scaling: S = n, E = 1.

**Amdahl's law.** If a fraction *s* of the work is serial (cannot be parallelised) and *p = 1 - s* is parallel, then S(n) = 1 / (s + p/n). The limit is 1/s however many cores you add.
From a measured speedup you can estimate the serial fraction (Karp-Flatt metric): **s ≈ (1/S - 1/n) / (1 - 1/n)**. If *s* is constant as *n* grows, the loss is a true serial part (Amdahl). If *s* **grows** with *n*, you are paying a cost that grows with the parallelism: coordination overhead, contention, or a shared bottleneck.
In this system the serial parts are: job creation and the tail (the last units finish while others idle), the per-job row lock that serialises unit completions in Postgres (`finish_job_if_complete`: `SELECT ... FOR UPDATE` on the job row), and the coordinator's single process.

**Coordination overhead.** Every unit costs a fixed round trip regardless of size: `LeaseWork` (an `upsert_worker` plus the `lease` CTE) and `CompleteWork` (a transaction: update unit, insert 4 `unit_results` rows, lock job row, check whether the job is finished). Per-unit overhead / per-unit compute decides efficiency: small units drown in overhead (E3), big units suffer tail effects.

**CFS throttling.** A CPU limit is a quota per 100 ms period. More runnable threads than quota means the kernel freezes the container for the rest of the period (E4). Throttled periods / periods is the metric.

**SMT vs physical cores.** A "6 CPU" VM may be 3 physical cores with 2 hyperthreads each. Two threads on one physical core share its execution units and caches: together they give maybe 1.2x to 1.3x one thread, not 2x. A CPU-bound simulation then scales almost linearly up to the physical core count and flattens beyond it. Check:
`kubectl debug node/k8s-w1 --image=busybox -it -- sh -c "grep -m1 'model name' /host/proc/cpuinfo; grep -m1 siblings /host/proc/cpuinfo; grep -m1 'cpu cores' /host/proc/cpuinfo"`
(`siblings` > `cpu cores` means SMT is on; inside a VM the hypervisor may hide this, in which case the E1 curve will show it). Delete the debug pod afterwards (`kubectl get pods`).

**1 GbE and fan-out tail latency.** The messages here are tiny (a unit result is a few hundred bytes; an advisor shard response a few kilobytes), so 1 GbE bandwidth (~110 MB/s) is never the limit; per-hop latency (~0.2 to 0.5 ms) and scheduling jitter are.
The advisor, however, waits for **all** shards: its latency is the **maximum** over *n* shard latencies. If one shard is slow with probability *q*, a request is slow with probability 1 - (1-q)^n, which grows with *n* (the "tail at scale" effect, E6).

---

## E1 Vertical scaling

**Hypothesis.** One worker pod, CPU limit 1 to 6 with the thread pool following the limit (`WORKER_THREADS` unset → `available_parallelism` reads the cgroup quota). Throughput should grow almost linearly while the pods' threads map to physical cores, with a small serial loss
inside a pod (each 2000-hand unit is cut into 64 chunks and each chunk is a rayon `par_iter` with a join at the end, so threads idle at the end of every chunk, and the pod does one gRPC round trip per unit). Predicted: efficiency ≥ 0.9 up to 3 CPUs, lower beyond if hyperthreads are in play.
Write down your prediction for 6 CPUs: ______ hands/s.

**Run** (about 15 minutes; 6 CPUs will probably stay `Pending` and be skipped. That is a finding: a 6-CPU pod cannot fit on a 6-core node with other pods):
```bash
scripts/bench/run-matrix.sh E1-vertical 1:1:0:2000 1:2:0:2000 1:3:0:2000 1:4:0:2000 1:5:0:2000 1:6:0:2000
scripts/bench/report.sh E1-vertical
```
With only one pod, the job runs on one node, so one node's CPUs and memory bandwidth are all that is used. Repeat with `BENCH_REPEAT=3` for tidy error bars.

**Watch.**
- `Hearts Worker Performance` -> "CPU cores used vs limit (per pod)": used should hug the limit line. "CPU throttling % (per pod)" ≈ 0. "Threads per worker" equals the limit.
- "Unit duration p50 / p95": should shrink as 1/CPU; if p95 stays far above p50, the units are uneven.
- `Hearts Overview` -> "Cluster hands/s" and "Pending units".

**Interpret.**
- Compute S(n) and the Karp-Flatt serial fraction per row. A flat *s* of a few percent means a true serial part inside the pod (chunk joins, per-unit RPC); rising *s* past 3 CPUs suggests SMT siblings or memory-bandwidth/cache sharing.
- Compare the shape with the pure engine benchmark (`docs/OPTIMIZATION_LOG.md`: 1 thread 824k hands/s, 8 threads 6.36M = 7.7x on the Mac): the cluster curve being lower than the engine curve measures the cost of everything outside `run_hands`.
- Try `BENCH_STRATEGIES=heuristic,heuristic,random,random BENCH_HANDS=3000000` as E1b: with ~1 µs hands, a 31-hand chunk is only ~30 µs of work, so the rayon join per chunk and the DB round trip per unit dominate and efficiency collapses sooner (that is the per-chunk overhead made visible).

**Results.**

| CPU | threads | hands/s | speedup | efficiency | serial fraction (Karp-Flatt) | throttling % | notes |
|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 1 | | 1.00 | 1.00 | - | | |
| 2 | 2 | | | | | | |
| 3 | 3 | | | | | | |
| 4 | 4 | | | | | | |
| 5 | 5 | | | | | | |
| 6 | 6 | | | | | | |

---

## E2 Horizontal scaling

**Hypothesis.** Adding pods (2 CPU each) scales almost linearly, better than E1 at the same total CPU, because pods share nothing in memory; they only share Postgres and the coordinator. Expected deviations: (a) the tail: after the last unit is leased, up to one unit's time is spent waiting for the slowest pod, which matters more as the job gets shorter at higher *n*; (b) with 150 units (300000 / 2000) over 6 pods, 25 units each, the imbalance is up to one unit in 25 = 4 %; (c) once two pods share a node, they share the memory bus and (if any) hyperthread siblings; (d) the job-row lock in `finish_job_if_complete`, hardly visible at these rates. Predict the efficiency at 6 pods: ______.

**Run** (about 15 minutes; 9 pods x 2 CPU = 18 CPUs asks for every allocatable CPU of the three worker nodes and will not schedule, so expect a `SKIP` there and `Insufficient cpu` in the scheduler events):
```bash
scripts/bench/run-matrix.sh E2-horizontal 1:2:0:2000 2:2:0:2000 3:2:0:2000 4:2:0:2000 6:2:0:2000 9:2:0:2000
scripts/bench/report.sh E2-horizontal
```
Optional E2b to fit more pods: `scripts/bench/run-matrix.sh E2b-horizontal-1cpu 1:1:0:2000 3:1:0:2000 6:1:0:2000 9:1:0:2000 12:1:0:2000`.

**Watch.**
- `Hearts Worker Performance` -> "Hands simulated/s per pod" (are all pods equal? a laggard is a noisy node or a hot spot) and "Hands simulated/s per node" (nodes with 3 pods vs 2 pods).
- "Busy fraction (per pod)": below 1.0 means a pod is waiting (for the coordinator, or at the tail of the job).
- `Hearts Overview` -> "DB query p95 by query", "gRPC handler p95 by method", "DB pool connections": do coordination latencies grow with the number of pods?
- "Pending units": how steeply it drains, and how long the tail is.
- Scheduling: `kubectl -n hearts get pods -o wide` to see how pods landed (topology spread prefers different nodes, `ScheduleAnyway`).

**Interpret.**
- Efficiency near 1.0 to ~4 pods and falling: compute the Karp-Flatt *s*. Constant *s* = Amdahl (tail and serial work); *s* growing with pods = contention (watch the DB panels).
- Tail estimate: wall time ≥ ideal + (one unit's duration). With unit time ~1.4 s (pimc:20 at 2 threads) and a 20 s job, the tail alone costs ~7 %. Shorter units shrink the tail but raise overhead (that is E3).
- Compare 3 pods x 2 CPU (E2) against 1 pod x 6 CPU (E1, if it fits) or 2 pods x 3 CPU: same cores, different packaging. Horizontal needs the coordinator; vertical needs a big node and is limited by the node. The comparison tells you what coordination costs.

**Results.**

| pods | CPU/pod | total cores | hands/s | speedup | efficiency | per-pod hands/s (min..max) | Karp-Flatt s | notes |
|---:|---:|---:|---:|---:|---:|---|---:|---|
| 1 | 2 | 2 | | 1.00 | 1.00 | | - | |
| 2 | 2 | 4 | | | | | | |
| 3 | 2 | 6 | | | | | | |
| 4 | 2 | 8 | | | | | | |
| 6 | 2 | 12 | | | | | | |
| 9 | 2 | 18 | | | | | | expect Pending |

---

## E3 Unit size

**Hypothesis.** Unit size trades overhead against granularity. Tiny units (10 hands) mean thousands of `LeaseWork`/`CompleteWork` round trips, each costing network + several SQL statements (a transaction with a job-row `FOR UPDATE`), so **Postgres and the coordinator become the bottleneck and hands/s collapses**; the cluster is mostly idle.
Huge units (10000 hands) mean few round trips but a long tail: the last unit runs alone while other pods idle, and a lost unit costs a lot of redo. There is a sweet spot in the middle (100 to 1000). Predict the unit size where efficiency peaks: ______.

**Run** (cluster size fixed at 3 pods x 2 CPU; a cheaper player makes the overhead stand out, so use `pimc:5`; about 20 minutes. Unit size 10 creates 30000 units: that is the point, but it can take minutes):
```bash
BENCH_STRATEGIES=heuristic,heuristic,random,pimc:5 BENCH_HANDS=300000 BENCH_REPEAT=2 \
  scripts/bench/run-matrix.sh E3-unit-size 3:2:0:10 3:2:0:100 3:2:0:1000 3:2:0:10000
scripts/bench/report.sh E3-unit-size
```
(The report's "speedup" is relative to unit size 10 here; read the `hands/s` column. Efficiency equals speedup because total cores are constant.)

**Watch while the 10-hand run is going** (this is the DB-bottleneck hunt):
- `Hearts Overview` -> **"DB query p95 by query"**: `histogram_quantile(0.95, sum by (le, query) (rate(hearts_db_query_duration_seconds_bucket{namespace="hearts"}[1m])))`.
  Per unit the workers cause `upsert_worker` + `lease` (LeaseWork) and `complete` (CompleteWork); every 10 s per worker, `touch_worker` + `heartbeat`. Watch `lease` and `complete`: when their p95 rises from ~1 ms to tens of ms while the CPU of the workers idles, the database is the bottleneck.
  Rate of queries: `sum by (query) (rate(hearts_db_query_duration_seconds_count{namespace="hearts"}[1m]))`. Average latency: `sum(rate(hearts_db_query_duration_seconds_sum[1m])) by (query) / sum(rate(hearts_db_query_duration_seconds_count[1m])) by (query)`.
- "DB pool connections": `in_use` near `DB_POOL_SIZE` (16) means requests queue for a connection (a symptom independent of query speed).
- "gRPC handler p95 by method" (`LeaseWork`, `CompleteWork`).
- `Hearts Postgres` -> "Transactions/s", "Connections by state", "Locks by mode" (lock waits on the job row show up as `RowShareLock`/`transactionid` waits), "Row changes/s".
- `Hearts Worker Performance` -> "Busy fraction (per pod)" (falls to e.g. 0.2: workers wait for the coordinator), "Hands simulated/s per pod", "Lease RPC errors/s and idle polls/s" (idle polls should be ~0).
- CPU of the coordinator and Postgres: `kubectl -n hearts top pods`. The coordinator limit is 2 CPUs; Postgres 2 CPUs; if either is pinned, that is your answer.

**Interpret.**
- Model: throughput ≈ min(compute capacity, coordination capacity). Compute capacity ≈ cores / (cost per hand). Coordination capacity ≈ (units/s the coordinator+DB can sustain) x unit size. At small unit size the second term is smaller, so throughput grows linearly with unit size, until the first term takes over (the knee). Estimate the units/s ceiling from the knee: ceiling ≈ hands/s at unit size 10 / 10.
- The per-job row lock serialises `CompleteWork` for one job, so completions per second per job are bounded by 1 / (transaction time under the lock): that is an Amdahl serial term, and *s* is large when units are tiny.
- At 10000 hands per unit: 30 units over 3 pods = 10 each. If throughput falls again, it is the tail and quantisation (30 units / 3 pods is fine; test also with 4 pods where 30 units don't divide evenly).
- Practical rule: choose a unit size so one unit takes ≥ 20 to 50 times the per-unit overhead, but ≪ the lease (30 s) and ≪ the job time (so the tail is small). Seconds, not milliseconds.
- Mitigations to try as follow-up experiments (all supported by config): `MAX_UNITS_PER_LEASE=8` and `PREFETCH=true` on the workers (one LeaseWork gets several units; the next lease is fetched while computing), `DB_POOL_SIZE`.
  Run `E3b` with `kubectl -n hearts set env deploy/hearts-worker MAX_UNITS_PER_LEASE=8 PREFETCH=true` and unit size 10 or 100, compare. Remove afterwards with `VAR-`.

**Results.**

| unit size | units | hands/s | wall s | `lease` p95 ms | `complete` p95 ms | pool in_use (max) | busy fraction | coordinator CPU | postgres CPU | notes |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 10 | 30000 | | | | | | | | | |
| 100 | 3000 | | | | | | | | | |
| 1000 | 300 | | | | | | | | | |
| 10000 | 30 | | | | | | | | | |

---

## E4 Threads above the CPU limit

**Hypothesis.** The pod has a 2-CPU quota (200 ms of CPU time per 100 ms period). With `WORKER_THREADS=2` the two threads fit the quota exactly. With 6 or 12 threads, all runnable at once, they exhaust the quota in a fraction of the period (6 threads: 33 ms; 12 threads: 17 ms) and the kernel **throttles** the whole container for the remaining ~67 to 83 ms.
Total CPU time used is still capped at 2 CPUs, so throughput should be roughly unchanged or slightly lower (context switches, cache thrash, wasted wake-ups), **but** latency gets lumpy: unit durations stretch and become more variable, heartbeats and probes can be delayed by up to a period, and the advisor's tail latency worsens. The "CPU throttling %" panel climbs from ~0 to 67 % (6 threads) and 83 % (12 threads); predicted fraction of periods throttled ≈ 1 - quota/(threads x period).
Write down the predicted throughput change: ______.

**Run** (about 10 minutes):
```bash
scripts/bench/run-matrix.sh E4-threads-over-limit 3:2:2:2000 3:2:4:2000 3:2:6:2000 3:2:12:2000
scripts/bench/report.sh E4-threads-over-limit
```
`threads` here is `WORKER_THREADS`; the first row (2 threads, 2 CPU) is the correctly sized control.

**Watch.**
- `Hearts Worker Performance` -> **"CPU throttling % (per pod)"** (the star of this experiment; you should see the alert threshold of 25 % crossed. `HeartsCPUThrottlingHigh` needs 5 sustained minutes to fire, but jobs are shorter than that, so look at the panel), "CPU cores used vs limit (per pod)" (usage capped at 2.0 despite 12 threads), "Threads per worker".
- "Unit duration p50 / p95" (the p95/p50 ratio grows), "Hands simulated/s per pod".
- Raw metric: `sum by (pod) (rate(container_cpu_cfs_throttled_seconds_total{namespace="hearts", pod=~"hearts-worker-.*"}[1m]))`: seconds per second spent frozen.
- Extra credit: run `scripts/bench/advise-latency.sh -n 100` while a tournament runs with 12 threads vs 2: compare advisor p99.

**Interpret.**
- If throughput is flat but throttling is high, you spent nothing and gained nothing in throughput; you only added jitter. So "more threads than limit" is never a free speedup.
- If throughput is *lower*, the cost is context switching and cache misses between more threads than cores, plus time lost when a thread holding the tail of a chunk is frozen while the others have finished.
- Why the default is right: `std::thread::available_parallelism()` reads the cgroup CPU quota, so with the limit as the only knob, threads follow it. This experiment shows what the default protects you from.
- Note the interplay with fractional limits: a limit of 1500m → `available_parallelism` rounds up to 2 threads, so even the default throttles slightly (try `1:1500m:0:2000`... the config syntax accepts `1.5` or `1500m`).

**Results.**

| threads | CPU limit | hands/s | vs control | throttling % (max) | unit p50 s | unit p95 s | notes |
|---:|---:|---:|---:|---:|---:|---:|---|
| 2 (control) | 2 | | 1.00 | | | | |
| 4 | 2 | | | | | | |
| 6 | 2 | | | | | | |
| 12 | 2 | | | | | | |

---

## E5 PIMC cost curve

**Hypothesis.** A `pimc:N` player does N Monte Carlo playouts for each of its legal moves on each of its decisions, so the cost per hand grows **linearly in N** on top of a small constant (the other three players and the dealing). `hands/s` should fall like `1 / (c0 + c1 x N)`, where c1 is the cost of one sample-decision. From the engine measurements: about 30 heuristic hands per sample, so `pimc:200` is roughly 6000 heuristic hands, ~7 ms per hand per thread on the Mac. Since PIMC per decision is serial inside a hand (rayon parallelises across hands), scaling with cores stays good: efficiency independent of N.
Predict hands/s at pimc:200 relative to pimc:10: ______ (linear says 20x slower).

**Run** (scale `BENCH_HANDS` so each run is about 60 s: cost ∝ N, so hands ∝ 1/N; about 15 minutes total):
```bash
for s in 10 50 200; do
  case $s in 10) h=600000 ;; 50) h=120000 ;; 200) h=30000 ;; esac
  BENCH_STRATEGIES="heuristic,heuristic,random,pimc:$s" BENCH_HANDS=$h BENCH_NOTES="pimc:$s" \
    scripts/bench/run-matrix.sh E5-pimc-cost 3:2:0:500
done
scripts/bench/report.sh E5-pimc-cost
```
(`pimc_samples` is stored from the strategy list; unit size 500 keeps units of reasonable duration for all three. Adjust `h` after a calibration run.) Add `pimc:1` and `pimc:2` for a better estimate of the intercept c0.

**Watch.**
- `Hearts Worker Performance` -> "Unit duration p50 / p95" (grows linearly in N), "Hands simulated/s per pod" (falls), "CPU throttling % (per pod)" (should stay 0), "Busy fraction (per pod)" (rises toward 1 as the work gets heavier relative to coordination).
- `Hearts Overview` -> "DB query p95 by query": as units get heavier, `lease`/`complete` rates fall by 20x, so the database becomes irrelevant (the opposite of E3).

**Interpret.**
- Fit `time per hand = c0 + c1 x N` from the three (or five) points: c1 is the slope (cost of one sample), c0 the intercept (non-PIMC work). A large c0 relative to c1 at small N means other players dominate.
- Use c1 to *plan*: how many hands/hour can you afford at N = 200? Which N gives a decent bot at an acceptable cost? This is the practical use of a cost curve.
- Strategy strength vs cost belongs to a separate experiment: run the same seed with `pimc:10` vs `pimc:200` in slot 3 and compare the `points` of slot 3 in the standings: the benefit side of the curve (diminishing returns). Record it in the notes.

**Results.**

| pimc samples | hands | wall s | hands/s | ms per hand per thread (hands/s ÷ threads, inverted) | relative cost | unit p50 s | slot-3 avg points | notes |
|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 10 | 600000 | | | | 1.0 | | | |
| 50 | 120000 | | | | | | | |
| 200 | 30000 | | | | | | | |

---

## E6 Advisor fan-out latency vs shard count

**Hypothesis.** `POST /api/advise` splits `samples` evenly over all worker pods (shard *s* gets `samples/n`), calls them concurrently and waits for all (or the deadline, 2 s by default). With a fixed number of samples:
- **median latency falls with more shards** (each does less work: roughly 1/n), until fixed costs dominate: DNS resolution of `hearts-worker-headless`, gRPC call, merge, ~1 ms network on 1 GbE;
- **the tail does not improve as much, and eventually worsens**: the request takes as long as the slowest shard, and the more shards, the more likely one of them is slow (a GC-free Rust process still has noisy neighbours, CPU throttling, a busy tournament on the same pool, a shard that just got a long unit). With per-shard slow probability *q* the request is slow with probability 1 - (1-q)^n. p99 should flatten or rise while p50 falls.
- Messages are tiny, so 1 GbE bandwidth does not matter; per-hop latency and stragglers do.
Predict the shard count where p99 stops improving: ______.

**Run** (workers idle, so the advisor has the pool to itself; about 20 minutes; the script prints percentiles for each shard count):
```bash
for n in 1 2 3 4 6; do
  kubectl -n hearts scale deploy/hearts-worker --replicas=$n
  kubectl -n hearts rollout status deploy/hearts-worker
  sleep 40      # old pods drop out of /api/workers; DNS settles
  echo "=== $n worker(s) ==="
  scripts/bench/advise-latency.sh -n 300 -s 20000
done
kubectl -n hearts scale deploy/hearts-worker --replicas=3          # or make deploy
```
(`-s 20000` samples per request is heavy enough to see compute time fall with shards; at the default 2000 you mostly measure fixed overhead.) Variants:
- **Constant work per shard**: scale `-s` with *n* (`-s $((n*5000))`) to isolate the tail effect.
- **Contended**: start a long tournament (UI, 3 pods busy) and repeat; the advisor then queues behind units on the same rayon pool.
- **Concurrency**: `CONC=8 scripts/bench/advise-latency.sh -n 400 -s 5000`.
- Check `partial` answers: if `partial>0` in the output, the 2 s deadline was hit and `samples_achieved < samples_requested`.

**Watch.**
- `Hearts Advisor` -> "Advise latency p50 / p95 / p99" (server view), "Per-shard latency p50 / p95 / p99" (the individual calls: when the end-to-end p99 is much higher than the per-shard p50, the tail comes from the max-of-n effect), "Shard errors/s by reason" (`timeout`, `error`, `connect`), "Partial answer rate", "Worker Simulate call p95", "Worker advise samples/s".
- `Hearts Worker Performance` -> "CPU throttling % (per pod)" and "Busy fraction (per pod)".
- Compare client-side (curl `time_total`) with server-side `latency_ms`: the difference is ingress + TLS + network from your laptop, i.e. what a user feels.

**Interpret.**
- Plot p50 and p99 against shard count (log-y). Expected: p50 ~ a/n + b; p99 ~ the max of n samples.
- Per-shard p50 vs end-to-end p50 should be close; per-shard p99 vs end-to-end p99 shows the max effect. The gap between "end-to-end p99" and "per-shard p99" grows with *n*.
- Mitigations to discuss (not implemented): hedged requests, returning at a quorum of shards (the deadline already gives partial answers), more samples per fast shard, dedicating a pool to the advisor.

**Results.**

| shards | samples | p50 ms | p95 ms | p99 ms | max ms | partial | per-shard p50 ms | per-shard p99 ms | notes |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 20000 | | | | | | | | |
| 2 | 20000 | | | | | | | | |
| 3 | 20000 | | | | | | | | |
| 4 | 20000 | | | | | | | | |
| 6 | 20000 | | | | | | | | |

---

## E7 Noisy neighbour

**Hypothesis.** Workers are Guaranteed QoS (requests == limits): the kernel gives their cgroup CPU shares proportional to a 2-CPU request and a hard quota of 2 CPUs. A burstable pod on the same node with a tiny request (50m) but unlimited bursting gets very little *weighted* CPU when the node is contended, so the workers should keep nearly all of their 2 CPUs. Yet they still share **last-level cache, memory bandwidth and (with SMT) physical cores** with the intruder, so expect a **small but measurable** loss (perhaps 5 to 25 %), concentrated on the node with the intruder. Because workers *pull* units, the affected pod simply takes fewer units, and the job slows by less than the affected pod's loss; the **advisor**, which waits for every shard, suffers the full straggler penalty (E6's max-of-n). Predict the job slowdown ___ % and the advisor p99 change ___ %.

**Run** (two experiment names so the baseline and the disturbed runs do not get averaged together; about 10 minutes):
```bash
# 1. baseline
BENCH_NOTES="no neighbour" scripts/bench/run-matrix.sh E7-baseline 3:2:0:2000
scripts/bench/advise-latency.sh -n 200 -s 20000 | tee /tmp/e7-advise-baseline.txt

# 2. unleash the neighbour on k8s-w1 (edit the nodeSelector inside the file for another node)
kubectl apply -f scripts/bench/noisy-neighbor.yaml
kubectl -n hearts get pod noisy-neighbor -o wide          # confirm Running on k8s-w1
kubectl -n hearts top pod noisy-neighbor                   # should be burning several cores

# 3. disturbed runs
BENCH_NOTES="noisy neighbour on k8s-w1" scripts/bench/run-matrix.sh E7-noisy 3:2:0:2000
scripts/bench/advise-latency.sh -n 200 -s 20000 | tee /tmp/e7-advise-noisy.txt

# 4. ALWAYS clean up
kubectl -n hearts delete pod noisy-neighbor
scripts/bench/report.sh E7-baseline --table-only ; scripts/bench/report.sh E7-noisy --table-only
```
Variants: raise `STRESS_WORKERS` in the YAML; pin to a node that hosts Postgres or the coordinator (`kubectl -n hearts get pods -o wide`) to see the effect on coordination instead of compute; give the intruder a larger CPU *request* (e.g. 4 CPUs) and watch it actually take CPU away (the pod may then not schedule: allocatable is finite).

**Watch.**
- `Hearts Worker Performance` -> "Hands simulated/s per node": the node with the neighbour drops below the others; "Hands simulated/s per pod" shows which pod; "CPU cores used vs limit (per pod)": the victim's usage falls under its 2.0 while *not* throttled. That distinguishes noisy neighbours (CPU available but slower) from throttling (quota exhausted).
- "Unit duration p50 / p95": the victim pod's units take longer.
- `Hearts Advisor` -> "Per-shard latency p50 / p95 / p99" and "Advise latency p50 / p95 / p99".
- `kubectl top nodes`: the node at 100 % CPU.

**Interpret.**
- Slowdown of the job = 1 - throughput_noisy / throughput_baseline. Compare to the victim pod's own loss (per-pod hands/s): with pull-based balancing the job loses roughly (victim loss) x (victim's share of the work) / (a mix), not the victim's full loss.
- If you see **no** effect, the kernel's weighting protected the guaranteed pods and the VMs' cores are not shared; if you see a big effect, suspect SMT siblings or an oversubscribed hypervisor. Compare E1's curve for hyperthread evidence.
- This is why benchmarks are only repeatable on quiet nodes, and why requests == limits is necessary but not sufficient.

**Results.**

| scenario | hands/s | wall s | victim-pod hands/s | other pods hands/s | advise p50 ms | advise p99 ms | notes |
|---|---:|---:|---:|---:|---:|---:|---|
| baseline | | | | | | | |
| neighbour on k8s-w1 (6 loops) | | | | | | | |
| neighbour, 12 loops | | | | | | | |

---

## How to present results

1. **State the question and the prediction first.** One sentence each: "Does throughput scale linearly with pods? I predicted 90 % efficiency at 6 pods." Predictions you wrote down in the tables above prove you learned something when they are wrong.
2. **Show the method in three lines**: workload (strategies, hands, seed), cluster (nodes, CPU/pod, image `git_sha`), and what varied. Mention the control and that standings were identical across configs (that is your correctness check).
3. **One chart per experiment.** Speedup vs cores with the ideal line y = x (E1, E2); hands/s vs unit size on a log x-axis (E3); throttling % and hands/s vs threads (E4); time per hand vs N with the fitted line (E5); p50 and p99 vs shard count (E6); grouped bars baseline vs noisy (E7). Label axes with units, start y-axes at zero, show the repeat range as error bars.
4. **Then the table**, from `scripts/bench/report.sh EXPERIMENT --csv-only`, with speedup, efficiency, and the Karp-Flatt serial fraction.
5. **Explain with a mechanism, not a shrug.** "Efficiency fell to 0.7 at 6 pods because ..." and name it: Amdahl tail, DB lock on the job row, SMT siblings, CFS throttling, a straggler pod. Back it with the Grafana panel (screenshot with the time range visible) or the PromQL.
6. **Say what you would change** ("raise unit size to 1000", "set MAX_UNITS_PER_LEASE=8") and, ideally, re-run to show it worked: before/after in the same table.
7. **Be honest about limits**: single cluster, three nodes, noisy home network, N runs, anything you did not control.

Quick chart without extra tooling (the owner's Python tooling rule is `uv`):
```bash
scripts/bench/report.sh E2-horizontal --csv-only > /tmp/e2.csv
uv run --with matplotlib --with pandas python - <<'PY'
import pandas as pd, matplotlib.pyplot as plt
df = pd.read_csv("/tmp/e2.csv")
df["cores"] = df["replicas"] * df["cpu"]            # works for whole-CPU limits like 2 (not "500m")
rel = df["cores"] / df["cores"].iloc[0]             # cores relative to the baseline row
plt.plot(rel, df["speedup"], "o-", label="measured")
plt.plot(rel, rel, "--", label="ideal (linear)")
plt.xlabel("cores relative to baseline"); plt.ylabel("speedup"); plt.legend()
plt.savefig("/tmp/e2.png", dpi=150)
PY
```

Save raw numbers in Postgres (they already are: `SELECT * FROM benchmark_runs WHERE experiment = 'E2-horizontal'`) and keep `/tmp/e2.csv` plus screenshots next to your write-up; the git sha tells you which code produced them.
