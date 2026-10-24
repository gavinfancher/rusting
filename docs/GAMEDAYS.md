# Game days: break it on purpose

A game day is a rehearsal: you cause a failure you chose, with a hypothesis about what will happen,
and you check it against Grafana, `kubectl` and a pass/fail test. The point is to practise being the sysadmin
**before** something breaks at a bad time, and to learn the Kubernetes mechanisms by watching them work.

There are ten drills. Nine have scripts in `scripts/gamedays/`; the tenth (power off a node) is guided and manual.
Everything is written so that a failure does not damage anything: each script restores what it changed, even if you press Ctrl-C.

| # | Drill | Script | Mechanism you will see |
|---|---|---|---|
| 1 | [Kill a worker mid-job](#drill-1-kill-a-worker-mid-job) | `01-kill-worker.sh` | lease expiry, reclaim, ReplicaSet self-healing, SIGTERM vs SIGKILL |
| 2 | [Kill the coordinator](#drill-2-kill-the-coordinator) | `02-kill-coordinator.sh` | stateless service, readiness probe, client retry with backoff |
| 3 | [Kill the Postgres pod](#drill-3-kill-the-postgres-pod) | `03-kill-postgres.sh` | StatefulSet identity, PVC re-attach, readiness vs liveness |
| 4 | [Drain the node hosting Postgres](#drill-4-drain-the-node-hosting-postgres) | `04-drain-db-node.sh` | cordon/drain, PodDisruptionBudget, PV node affinity (pod stays Pending) |
| 5 | [CPU-starve the workers](#drill-5-cpu-starve-the-workers) | `05-cpu-starve.sh` | CFS throttling, requests vs limits |
| 6 | [OOM a worker](#drill-6-oom-a-worker) | `06-oom-worker.sh` | memory limit, OOMKilled, CrashLoopBackOff |
| 7 | [Bad deploy](#drill-7-bad-deploy) | `07-bad-deploy.sh` | RollingUpdate, ImagePullBackOff, `rollout undo` |
| 8 | [Poison unit](#drill-8-poison-unit) | `08-poison-unit.sh` | `catch_unwind`, MAX_ATTEMPTS, alerting |
| 9 | [Lease shorter than the unit runtime](#drill-9-lease-shorter-than-the-unit-runtime) | `09-short-lease.sh` | duplicate/wasted work, idempotent completion |
| 10 | [Whole node down](#drill-10-whole-node-down-manual) | `10-node-down.sh` (guided) | node-monitor grace, taint eviction timers |

## How it works

### The oracle: the reference job

Every drill that runs work uses the same job:

```json
{"strategies":["heuristic","heuristic","random","pimc:20"],"num_hands":100000,"seed":42,"unit_size":2000}
```

The engine is deterministic: hand *i* of seed 42 is always the same hand, whichever pod or thread plays it, and the
result is merged with plain integer addition. So **any correct run, however violent, must end with exactly these points per
strategy slot**:

```
[481853, 479038, 1459749, 381744]        (slot 0 heuristic, slot 1 heuristic, slot 2 random, slot 3 pimc:20)
```

That is also what `hearts-cli tournament --hands 100000 --seed 42 --strategies heuristic,heuristic,random,pimc:20`
prints on your laptop. Chaos engineering usually has to argue about "was the system still correct?"; here it is a string comparison.
Wrong standings mean either a unit was lost or a unit was counted twice, which would be a real bug.

### What a drill script does

1. Prints what it is about to do and asks `Proceed with this drill? [y/N]` (skip with `--yes`). Without a terminal it refuses to run unless `--yes` is given.
2. Preflight: cluster reachable, `/readyz` OK, at least one active worker, and a warning if leftover `FAULT_*`/`WORKER_THREADS`/`LEASE_SECONDS`/`MAX_ATTEMPTS` overrides exist.
3. Records the start time and prints Grafana links with `from=<start>` plus the panel and the PromQL to paste.
4. Submits the reference job, triggers the fault, waits, and checks the pass criteria.
5. Restores everything it touched (an EXIT trap, also on Ctrl-C), then prints `PASS`, `FAIL` or `ABORTED`.

Environment: `BASE_URL` (default `https://hearts.k8s.ad.gavinf.com`; `http://localhost:8080` for the compose stack, though drills
need Kubernetes), `NS` (default `hearts`), `GRAFANA_URL`. Scripts need `kubectl`, `curl`, `jq`.

Several drills add a per-unit sleep (`FAULT_SLOW_MS=1500`, "stretch") first so the 15 to 60 second reference job is still running
when the fault lands. It does not change results, and it is removed again at the end.

Run them one at a time, with nothing else using the cluster, and keep a terminal open on `kubectl -n hearts get pods -w`
and a browser on Grafana.

> If something goes wrong in a script's cleanup, the commands to restore by hand are in each drill's "Recover" below, and the generic ones are:
> `kubectl -n hearts set env deploy/hearts-worker FAULT_SLOW_MS- FAULT_PANIC_EVERY_N- WORKER_THREADS-`,
> `kubectl -n hearts set env deploy/hearts-coordinator LEASE_SECONDS- MAX_ATTEMPTS-`, `kubectl uncordon <node>`, `make deploy`.

### Dashboards used

| UID | Title | Panels referenced below |
|---|---|---|
| `hearts-overview` | Hearts Overview | Cluster hands/s, Active workers, Pending units, Failed units, Hands completed per second, Units by state, Unit event rates, Active workers over time, DB query p95 by query, gRPC handler p95 by method, DB pool connections, HTTP requests/s by route and status, Jobs created / finished per minute |
| `hearts-workers` | Hearts Worker Performance | CPU cores used vs limit (per pod), CPU throttling % (per pod), Memory working set vs limit (per pod), Hands simulated/s per pod, Hands simulated/s per node, Unit duration p50 / p95, Busy fraction (per pod), Threads per worker, Wasted units/s, Worker results/s, Lease RPC errors/s and idle polls/s, Pod restarts (last 1h), OOMKilled (last terminated reason) |
| `hearts-advisor` | Hearts Advisor | Advise latency p50 / p95 / p99, Per-shard latency p50 / p95 / p99, Shard errors/s by reason, Partial answer rate, Advise requests/s, Worker advise samples/s, Worker Simulate call p95 |
| `hearts-postgres` | Hearts Postgres | Postgres up, Database size, Connections, Transactions/s, Connections by state, Locks by mode, Rows by state, Tuples live vs dead, Row changes/s |

URL pattern: `https://grafana.k8s.ad.gavinf.com/d/<uid>?from=<epoch-ms>&to=now&refresh=10s`. Every dashboard has a `namespace` variable (default `hearts`).

### Diagnosis toolbox (practise these in every drill)

```bash
kubectl -n hearts get pods -o wide                     # state, restarts, node
kubectl -n hearts describe pod <pod>                   # Events at the bottom, "Last State" for crashes
kubectl -n hearts logs <pod> [-c container] [--previous] [--since=5m]
kubectl -n hearts get events --sort-by=.lastTimestamp | tail -30
kubectl -n hearts top pods ; kubectl top nodes         # needs metrics-server (k3s ships it)
kubectl -n hearts get endpoints <service>              # who receives traffic
kubectl -n hearts rollout status|history|undo deploy/<name>
kubectl get nodes ; kubectl describe node <node>       # Conditions, Taints, Allocated resources
```

---

## Drill 1: Kill a worker mid-job

**Hypothesis.** A worker killed with SIGKILL (a real crash) cannot tell anyone. Its unit stays `leased` in Postgres until
`lease_expires_at` (heartbeats extend it to now + `LEASE_SECONDS` = 30 s every 10 s, so 20 to 30 s remain). Any worker calling `LeaseWork` after
that, even an idle one polling every second, picks the expired unit up first (the lease query orders by `job_id, id` and accepts
`state = 'leased' AND lease_expires_at < now()`), increments `attempts` and counts a **reclaim**. Meanwhile the ReplicaSet notices the missing pod and starts a
replacement within seconds. The redo computes exactly the same unit (seeded by hand index), so standings are identical. A *graceful* delete behaves differently: SIGTERM makes the worker
stop leasing, finish its current unit (up to `SHUTDOWN_GRACE_SECONDS`, 45 s), report it, and exit: **zero reclaims**. The difference between SIGTERM and SIGKILL is the lesson.

**Command.**
```bash
scripts/gamedays/01-kill-worker.sh            # SIGKILL: kubectl delete pod <busy worker> --grace-period=0 --force
GRACEFUL=1 scripts/gamedays/01-kill-worker.sh # SIGTERM: plain kubectl delete pod
```

**Watch.**
- `Hearts Overview` -> "Unit event rates": the reclaimed line blips about 20 to 30 s after the kill.
  `sum(rate(hearts_units_reclaimed_total{namespace="hearts"}[1m]))`
- `Hearts Overview` -> "Active workers over time": drops by one ~30 s later (a worker is "active" for 30 s after its last call), then returns.
  `max(hearts_workers_active{namespace="hearts"})`
- `Hearts Overview` -> "Units by state": the dead worker's unit stays in `leased` until reclaimed.
- `Hearts Worker Performance` -> "Hands simulated/s per pod": the dead pod's line ends, the new pod's begins.

**Diagnose (practise).**
```bash
kubectl -n hearts get pods -l app.kubernetes.io/name=hearts-worker -w
kubectl -n hearts get events --sort-by=.lastTimestamp | tail
curl -s $URL/api/workers | jq '.[] | {pod_name, active, leased_units, last_seen}'
curl -s $URL/api/jobs/<id> | jq .progress          # reclaims
kubectl -n hearts logs deploy/hearts-coordinator | grep -i lease
```
SQL: `SELECT id, attempts, lease_owner, completed_by FROM work_units WHERE job_id = <id> AND attempts > 1;` (`lease_owner` is the dead pod, `completed_by` the rescuer).

**Recover.** Nothing: the Deployment replaces the pod. If it did not (Pending), `kubectl -n hearts describe pod` (usually Insufficient cpu).

**Pass criteria (automatic).** Job `done`; standings equal the reference; `progress.reclaims >= 1` and `hearts_units_reclaimed_total` increased (SIGKILL mode);
Deployment back to full ready replicas.

**Questions.**
1. Why did the reclaim take ~30 s rather than 1 s? What would a shorter `LEASE_SECONDS` buy, and what does drill 9 show it costs?
2. Why can the coordinator not simply ask Kubernetes whether the pod is dead? (What would be the failure modes of a coordinator that tracked liveness itself?)
3. With `GRACEFUL=1`, why were there no reclaims? Which line of code waits for the unit (`process_unit` in `crates/worker/src/lease.rs`)?
4. The terminated pod's counters (`hearts_worker_hands_simulated_total`) vanish from Prometheus. How would you notice lost work in the metrics?

---

## Drill 2: Kill the coordinator

**Hypothesis.** The coordinator holds no state in memory (`AppState` is a DB pool, config, metrics). Delete the pod and the Deployment
starts another. The `hearts-coordinator` Service has no ready endpoints until the new pod's readiness probe (`/readyz`, every 5 s, runs `SELECT 1` against Postgres) succeeds,
so the UI shows Traefik errors for ~10 to 20 s. Workers keep computing their in-flight unit; their `LeaseWork`, `Heartbeat` and `CompleteWork` calls fail with `Unavailable`,
and they retry with **exponential backoff with full jitter** (100 ms doubling to 10 s; `CompleteWork` tries up to 6 times over roughly 15 s) so a fleet does not stampede
the new pod. For an outage this short most workers manage to report their unit on retry and there are few or no reclaims; outages longer than the lease lead to reclaims, as in drill 1. Workers whose last
successful coordinator contact is older than max(30 s, 3 x heartbeat) turn NotReady (removed from advisor fan-out) but are **not** restarted, because the worker liveness probe does not depend on the coordinator.
The new coordinator's Prometheus counters start from zero.

**Command.** `scripts/gamedays/02-kill-coordinator.sh`

**Watch.**
- `Hearts Overview` -> "HTTP requests/s by route and status": a gap while the pod is down (the counters restart from zero in the new pod; this panel only counts HTTP, not the workers' gRPC calls).
- `Hearts Worker Performance` -> "Lease RPC errors/s and idle polls/s": the `lease`, `heartbeat` and `complete` error rates spike.
  `sum by (rpc) (rate(hearts_worker_lease_errors_total{namespace="hearts"}[1m]))`
- `Hearts Overview` -> "Active workers over time": the series itself disappears while there is nothing to scrape.
- Prometheus target `hearts-coordinator` goes DOWN, then UP (Status -> Targets).

**Diagnose (practise).**
```bash
kubectl -n hearts get pods -l app.kubernetes.io/name=hearts-coordinator -w
kubectl -n hearts get endpoints hearts-coordinator            # empty while not Ready
kubectl -n hearts describe pod -l app.kubernetes.io/name=hearts-coordinator | sed -n '/Events:/,$p'
kubectl -n hearts logs deploy/hearts-worker --since=2m | grep -i 'backing off'
kubectl -n hearts logs deploy/hearts-coordinator | head       # "database not ready" retries, migrations
```

**Recover.** Automatic. If the new pod never becomes Ready, look at its logs and at Postgres (drill 3).

**Pass criteria (automatic).** A different coordinator pod serves; job `done` with the reference standings; `hearts_worker_lease_errors_total` increased (workers saw and survived the outage); the stretch override is removed.

**Questions.**
1. Why `strategy: Recreate` for the coordinator, and what would a deploy cost you compared with `RollingUpdate`? (Hint: migrations at startup.)
2. Which probe decided that traffic stops during the outage, and why is it `/readyz` and not `/healthz` that checks the database?
3. What if the workers retried in lockstep without jitter? Where in `Backoff::next_delay` is the jitter?
4. The job's progress was kept; where? Why would a coordinator that kept a job queue in memory fail this drill?

---

## Drill 3: Kill the Postgres pod

**Hypothesis.** `hearts-db-0` belongs to a StatefulSet, so its replacement has the **same name** and mounts the **same PVC** (`data-hearts-db-0`), which forces it back
onto the same node. Postgres shuts down (clean) or is killed (HARD=1) and starts again, replaying its write-ahead log if needed. No committed data is lost. In between:
every coordinator DB call fails (sqlx `acquire_timeout` 5 s), HTTP handlers answer 500 `database error`, and `/readyz` fails (its `SELECT 1` must finish within 1 s); after 3 failed probes (~15 s)
the coordinator pod is removed from the Service endpoints, **but not restarted** (liveness is only `/healthz`). Workers back off. A result that could not be reported
after 6 tries is dropped (wasted work), and the unit is reclaimed after its lease expires. When Postgres is back the sqlx pool reconnects by itself; the readiness probe flips to ready on the next success. Depending on how the container runtime treats the Postgres image's stop signal (SIGINT = fast shutdown, or SIGTERM = "smart" shutdown that waits for the
pool's open connections), the stop can take a few seconds or up to the 30 s grace period before SIGKILL.

**Command.**
```bash
scripts/gamedays/03-kill-postgres.sh          # graceful delete
HARD=1 scripts/gamedays/03-kill-postgres.sh   # --grace-period=0 --force: crash recovery
```

**Watch.**
- `Hearts Postgres` -> "Postgres up": `min(pg_up{namespace="hearts"})` goes to 0 (or the series disappears while the exporter sidecar is down too).
- `Hearts Overview` -> "DB pool connections": in-use/idle drop and recover. "HTTP requests/s by route and status": a wall of 5xx, then normal.
- `Hearts Worker Performance` -> "Lease RPC errors/s and idle polls/s".
- `Hearts Overview` -> "Pending units"/"Units by state": frozen, then moving again.

**Diagnose (practise).**
```bash
kubectl -n hearts get pods -w
kubectl -n hearts describe pod hearts-db-0 | tail -25
kubectl -n hearts logs hearts-db-0 -c postgres --previous | tail     # last lines of the old instance
kubectl -n hearts logs hearts-db-0 -c postgres | tail                # "database system was not properly shut down" with HARD=1
kubectl -n hearts logs deploy/hearts-coordinator --since=2m | grep -i error
kubectl -n hearts get pvc,pv ; kubectl -n hearts get endpoints hearts-coordinator
```

**Recover.** Automatic. If `hearts-db-0` stays Pending, see drill 4; if it crash-loops, read the Postgres logs (`--previous`) and check disk space on its node.

**Pass criteria (automatic).** `/readyz` was observed failing; a new pod UID took over on the **same node**; job `done` with the reference standings; the number of jobs did not shrink.

**Questions.**
1. Why does the new pod come back on the same node? (Look at the PV: `kubectl get pv -o yaml | grep -A8 nodeAffinity`.)
2. Why is the Service for the coordinator unready but the coordinator not restarted? What would happen if liveness also checked the database?
3. What does the StatefulSet give you that a Deployment with one replica and a PVC would not?
4. Where would a unit completion sit if the DB died between the worker's compute and `CompleteWork`? Why is that safe?

---

## Drill 4: Drain the node hosting Postgres

**Hypothesis.** `kubectl drain` first **cordons** the node (no new pods), then **evicts** its pods through the Eviction API, which obeys PodDisruptionBudgets.
- Workers on that node are evicted one at a time because of the `hearts-worker` PDB (`maxUnavailable: 1`): the drain waits ("Cannot evict pod as it would violate the pod's disruption budget") until each replacement is Ready elsewhere.
  Since new workers only stay Ready while they can reach the coordinator, a database outage can make this wait long: drain retries until its timeout.
- `hearts-db-0` is evicted too and the StatefulSet recreates it, but the new pod **stays `Pending`**. Its PVC is bound to a `local-path` PersistentVolume whose
  `nodeAffinity` requires exactly this node, and this node is cordoned, so no node satisfies both. The scheduler event reads
  `0/4 nodes are available: 1 node(s) had volume node affinity conflict, 1 node(s) were unschedulable, ...`.
- With Postgres gone the coordinator becomes not-Ready, so the whole application is down for as long as the node is cordoned. Leases expire; after `uncordon`, Postgres starts on its node, workers reclaim the expired units, and the job completes with the reference standings.
This is the real cost of node-local storage; a replicated volume (Longhorn) or a database operator with replicas would remove the single point of failure.

**Command.**
```bash
scripts/gamedays/04-drain-db-node.sh                       # only pods with app.kubernetes.io/part-of=hearts are evicted
HOLD_SECONDS=120 FULL_DRAIN=1 scripts/gamedays/04-drain-db-node.sh   # everything on the node (Prometheus/Grafana too if they live there)
```
The script refuses to drain the control-plane node and always runs `kubectl uncordon` on exit.

**Watch.**
- `Hearts Postgres` -> "Postgres up"; `Hearts Overview` -> "Active workers over time".
- `Hearts Worker Performance` -> "Hands simulated/s per node": the drained node's line drops to zero.
- A shell: `kubectl get nodes` (the node shows `Ready,SchedulingDisabled`), `kubectl -n hearts get pods -o wide -w`.
- With `FULL_DRAIN=1`, if Prometheus lives on that node you will have a gap in all graphs: its own PVC is pinned the same way.

**Diagnose (practise).**
```bash
kubectl -n hearts describe pod hearts-db-0 | sed -n '/Events:/,$p'      # FailedScheduling ... volume node affinity conflict
kubectl get pv -o custom-columns=NAME:.metadata.name,CLAIM:.spec.claimRef.name,NODE:.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].values[0]
kubectl -n hearts get pdb                                               # ALLOWED DISRUPTIONS
kubectl describe node <node> | grep -E 'Unschedulable|Taints'
kubectl -n hearts get events --sort-by=.lastTimestamp | tail -20
```

**Recover.** `kubectl uncordon <node>` (the script does it). Never "fix" this by deleting the PVC unless you accept losing the database (restore from a dump: `docs/RUNBOOK.md`, "Backup and restore").

**Pass criteria (automatic).** `hearts-db-0` is `Pending` with a *volume node affinity* message while the node is drained; `/readyz` fails meanwhile; after uncordon Postgres comes back on the same node; job `done`, standings identical.

**Questions.**
1. Why is the pod Pending rather than rescheduled on a healthy node? Which object carries the constraint (PV, not Pod)?
2. Why does the PDB protect the workers but not the database? What PDB would protect the single DB pod (`maxUnavailable: 0`)? What would then happen to a drain?
3. What is the difference between `cordon`, `drain` and a node going NotReady?
4. Which alerts fire, in which order? (Expect `HeartsPostgresDown` after 1 minute; check Prometheus -> Alerts.)
5. How would you make this survive a node loss? Compare Longhorn, CloudNativePG, and running Postgres outside the cluster.

---

## Drill 5: CPU-starve the workers

**Hypothesis.** The CPU limit is a *quota* enforced by the kernel scheduler (CFS bandwidth control): in each 100 ms period a container may use `limit x 100 ms` of CPU time, across all its threads.
Shrink the limit to 500m and keep `WORKER_THREADS=4`: four busy rayon threads consume the 50 ms quota in about 12 ms of wall time and are then **throttled** for the rest of the period.
The "CPU throttling % (per pod)" panel rises toward 80 to 90 %, "CPU cores used vs limit" shows usage flat against the new 0.5 line, and per-pod throughput falls to roughly (0.5 / 2) = a quarter of normal. Nothing breaks: the job just takes about 4x as long and standings stay identical.
After 5 minutes of > 25 % throttling the `HeartsCPUThrottlingHigh` alert fires. If you had **not** forced the threads, the worker would size its pool from the cgroup limit (`std::thread::available_parallelism()` honours the quota) and throttle far less, which is why the default is the right one.
Throttling also hurts latency-sensitive work: advisor calls take longer, heartbeats and probes can be delayed by up to a period.

**Command.**
```bash
scripts/gamedays/05-cpu-starve.sh                          # baseline run, then 500m with 4 threads
CPU_LIMIT=250m THREADS=8 BASELINE=0 scripts/gamedays/05-cpu-starve.sh
```
Requests and limits change together (`requests == limits` keeps QoS Guaranteed; a limit below the request is invalid anyway).

**Watch.**
- `Hearts Worker Performance` -> "CPU throttling % (per pod)":
  `100 * sum by (pod) (rate(container_cpu_cfs_throttled_periods_total{namespace="hearts", pod=~"hearts-worker-.*", container="worker"}[1m])) / sum by (pod) (rate(container_cpu_cfs_periods_total{namespace="hearts", pod=~"hearts-worker-.*", container="worker"}[1m]))`
- "CPU cores used vs limit (per pod)", "Threads per worker" (4), "Unit duration p50 / p95" (grows ~4x), "Hands simulated/s per pod".
- `Hearts Overview` -> "Cluster hands/s".

**Diagnose (practise).**
```bash
kubectl -n hearts top pods -l app.kubernetes.io/name=hearts-worker          # CPU pinned near the limit
kubectl -n hearts get pod -l app.kubernetes.io/name=hearts-worker -o custom-columns=NAME:.metadata.name,QOS:.status.qosClass,CPU:.spec.containers[0].resources.limits.cpu
kubectl -n hearts logs deploy/hearts-worker | grep 'building rayon pool'     # threads=4
```
Contrast `top` (looks "busy but within limits") with the throttling metric (the real story).

**Recover.** The script restores the saved spec. By hand: `make deploy` (reapplies replicas/resources) and `kubectl -n hearts set env deploy/hearts-worker WORKER_THREADS-`.

**Pass criteria (automatic).** Job `done` with reference standings; CFS throttle ratio over worker lifetimes > 0.25; the starved run took > 1.3x the baseline.

**Questions.**
1. Why does `kubectl top` show the pod at "0.5 CPU, fine" while users wait 4x longer?
2. What is a CFS period? Why can a container be throttled while the node is mostly idle?
3. Why do requests matter (scheduling, CPU weight under contention) differently from limits (quota)? Why do we still set both equal?
4. Would removing the limit be a fix? What would you lose (QoS, repeatable benchmarks, noisy neighbours)?

---

## Drill 6: OOM a worker

**Hypothesis.** Memory cannot be throttled: if a process wants more than the container's memory limit, the kernel's OOM killer sends SIGKILL (exit code 137) and
the kubelet records `lastState.terminated.reason: OOMKilled`. With `restartPolicy: Always` it restarts the container, waiting 10 s, 20 s, 40 s, ... capped at 5 minutes between restarts:
`CrashLoopBackOff`. The pod is not rescheduled; it restarts in place, and `kube_pod_container_status_restarts_total` increases (alert `HeartsPodCrashLooping` after more than 2 restarts in 15 minutes).
A killed worker loses its unit like in drill 1 (lease expiry, reclaim), so once the limit is sane again the reference job gives identical standings.
Workers are small (tokio, rayon stacks, metrics; a unit holds only counters), so **64Mi may not be enough to trigger an OOM**: the script walks down `64Mi 32Mi 16Mi 8Mi` until it happens. The level at which it breaks is a measurement of the real memory footprint.

**Command.** `scripts/gamedays/06-oom-worker.sh` (override with `MEM_LADDER="48Mi 24Mi"`, `OOM_WAIT=120`). A heavy `pimc:200` job keeps the workers busy meanwhile and is cancelled afterwards.

**Watch.**
- `Hearts Worker Performance` -> "OOMKilled (last terminated reason)": `sum by (pod) (kube_pod_container_status_last_terminated_reason{namespace="hearts", pod=~"hearts-worker-.*", reason="OOMKilled"})`
- "Memory working set vs limit (per pod)": the working set climbs to the limit line just before the drop. "Pod restarts (last 1h)".
- `Hearts Overview` -> "Active workers over time" flaps.

**Diagnose (practise).**
```bash
kubectl -n hearts get pods -l app.kubernetes.io/name=hearts-worker -w        # OOMKilled -> CrashLoopBackOff
kubectl -n hearts describe pod <pod> | sed -n '/Last State/,/Ready:/p'       # Reason: OOMKilled, Exit Code: 137
kubectl -n hearts logs <pod> --previous                                      # last words (often just truncated)
kubectl -n hearts get pod <pod> -o jsonpath='{.status.containerStatuses[0].lastState}'
kubectl -n hearts get events --sort-by=.lastTimestamp | grep -i -E 'oom|kill|back-off'
```
Tell apart: `OOMKilled` (memory), `Error` with exit code 1 (the app exited), `Completed` (exit 0), liveness-probe kills (`Liveness probe failed` in events and "Killing" with exit 137 too).

**Recover.** `kubectl -n hearts rollout undo deploy/hearts-worker`, or `make deploy`, or restore the memory (the script does). A pod sitting in back-off waits out its timer; deleting the pod resets it.

**Pass criteria (automatic).** A worker container was `OOMKilled` and restarted; after restoring the limit all replicas are Ready and the reference job is `done` with identical standings.

**Questions.**
1. Why is CPU "compressible" (throttle) but memory not? What happens to a process that hits its memory limit but has no allocation failure handling?
2. Why does the pod show `CrashLoopBackOff` even though the cause was `OOMKilled`? What does the back-off protect?
3. Which QoS classes are evicted first under *node* memory pressure, and how is that different from this cgroup-level OOM?
4. How would you pick a memory limit? (Use the "Memory working set" panel over a PIMC-heavy run, add headroom.)

---

## Drill 7: Bad deploy

**Hypothesis.** The worker Deployment uses the default `RollingUpdate` with `maxSurge: 25%` (rounded up: 1) and `maxUnavailable: 25%` (rounded down: 0). Changing the image to a tag that does not exist
makes the controller create one new pod from a new ReplicaSet. The kubelet cannot pull it: `ErrImagePull`, then `ImagePullBackOff` with growing delays. The pod never becomes Ready,
so the controller never scales the old ReplicaSet down: **the old pods keep serving and the job finishes normally.** `kubectl rollout status` blocks, and after `progressDeadlineSeconds` (600 s)
the Deployment condition becomes `ProgressDeadlineExceeded`. Kubernetes does not roll back by itself; a human runs `kubectl rollout undo` (back to the previous ReplicaSet) or fixes the tag.
Contrast with the coordinator: `Recreate` kills the old pod first, so a bad coordinator image is an outage.

**Command.** `scripts/gamedays/07-bad-deploy.sh`

**Watch.**
- `Hearts Overview` -> "Active workers over time" (unchanged), "Hands completed per second" (keeps flowing).
- A shell: `kubectl -n hearts get pods -w`, `kubectl -n hearts rollout status deploy/hearts-worker`, `kubectl -n hearts get rs`.
- `Hearts Worker Performance` -> "Pod restarts (last 1h)" stays flat: nothing restarts; the pod never starts.

**Diagnose (practise).**
```bash
kubectl -n hearts rollout status deploy/hearts-worker --timeout=30s     # "Waiting for deployment ... 1 out of 3 new replicas have been updated"
kubectl -n hearts get rs -l app.kubernetes.io/name=hearts-worker         # old RS: 3, new RS: 1 not ready
kubectl -n hearts describe pod <new-pod> | sed -n '/Events:/,$p'        # Failed to pull image ... not found / ImagePullBackOff
kubectl -n hearts rollout history deploy/hearts-worker
kubectl -n hearts get deploy hearts-worker -o jsonpath='{.spec.strategy}{"\n"}{.spec.template.spec.containers[0].image}{"\n"}'
```

**Recover.**
```bash
kubectl -n hearts rollout undo deploy/hearts-worker            # emergency brake
# then make git match: edit newTag in deploy/overlays/homelab/kustomization.yaml, and make deploy
```
(See "Roll back" in the runbook for why `undo` alone is not durable.)

**Pass criteria (automatic).** A pod reached `ErrImagePull`/`ImagePullBackOff`; the rollout did not complete; ready replicas stayed at the old count; job `done` with identical standings; after `rollout undo`, rollout completes and no pod runs the bad image.

**Questions.**
1. Why did the old pods stay? Which two numbers (`maxSurge`, `maxUnavailable`) decide this, and what would `maxUnavailable: 1` have done to capacity?
2. Why is a readiness probe essential for rollouts being safe? What would change if the new image started but served errors and its probe was missing?
3. What did `rollout undo` change in the cluster, and why will `make deploy` bring the bad tag back unless you edit git?
4. How could CI catch this before the deploy (image existence check, `kubectl diff -k`, a smoke test)?

---

## Drill 8: Poison unit

**Hypothesis.** With `FAULT_PANIC_EVERY_N=1` every unit panics inside the engine. The worker does **not** crash: `Runner::run_unit` (`crates/worker/src/runner.rs`) wraps the work in
`catch_unwind(AssertUnwindSafe(|| pool.install(...)))` (rayon forwards panics from its threads to the caller of `install`), so the panic becomes `UnitOutcome::Failed("panic: injected fault ...")` and the worker sends `CompleteWork(error)`.
The coordinator sets `last_error` and puts the unit back to `pending` while `attempts < MAX_ATTEMPTS` (3), else `failed`. A 5-unit job therefore produces 15 errors (`hearts_units_errored_total` +15) and 5 permanently
failed units (`hearts_units_failed_total` +5); the job ends `failed`. No pod restarts, no `CrashLoopBackOff`. `HeartsUnitsFailed` has `for: 0m`, so it fires as soon as Prometheus evaluates the rule (scrape 15 s plus evaluation interval) and stays for the 10-minute window.
This is the "poison pill" problem of every queue: one unit that always fails must not block the queue forever.

**Command.** `scripts/gamedays/08-poison-unit.sh` (sets the fault on all workers, runs a 5-unit job, removes the fault, then runs the reference job to show recovery).

**Watch.**
- `Hearts Overview` -> "Failed units" (stat turns non-zero), "Unit event rates" (errored then failed).
  `sum(increase(hearts_units_failed_total[10m]))`
- `Hearts Worker Performance` -> "Worker results/s" (`result="error"`).
- Prometheus -> Alerts: `HeartsUnitsFailed` pending/firing. `kubectl get --raw /api/v1/namespaces/monitoring/services/kube-prometheus-stack-prometheus:9090/proxy/api/v1/alerts | jq`.
- Pods stay Running with 0 new restarts: `kubectl -n hearts get pods`.

**Diagnose (practise).**
```bash
kubectl -n hearts logs deploy/hearts-worker --tail=30 | grep -i -E 'panic|injected|failed'
make psql   # SELECT id, state, attempts, last_error FROM work_units WHERE state = 'failed' ORDER BY id DESC LIMIT 10;
kubectl -n hearts get deploy hearts-worker -o jsonpath='{.spec.template.spec.containers[0].env}' | jq   # the FAULT_* env
curl -s $URL/api/jobs/<id> | jq '{status, progress}'
```

**Recover.** `kubectl -n hearts set env deploy/hearts-worker FAULT_PANIC_EVERY_N-` (the script does). The failed job is history; resubmit if you wanted its result. The alert clears 10 minutes after the last failure.

**Pass criteria (automatic).** Job status `failed` with 5 failed units; failed counter +5, errored counter +15; no container restarts; (best effort) alert `HeartsUnitsFailed` firing; after removing the fault the reference job is `done` with identical standings.

**Questions.**
1. Why `catch_unwind` and not "let it crash and let Kubernetes restart"? What would a crash loop do to the *other* unit the worker holds and to the advisor shard?
2. What stops a poison unit from retrying forever? Where is `MAX_ATTEMPTS` enforced (`complete_err` in `crates/coordinator/src/db.rs`)?
3. A panic inside rayon: how does it reach `catch_unwind`? What state might be left inconsistent (`AssertUnwindSafe`)?
4. How would you tell this fault from an infrastructure one at 3 am? (Pods healthy, restarts zero, only `failed`/`errored` rising.)

---

## Drill 9: Lease shorter than the unit runtime

**Hypothesis.** A lease is a promise to be alive, renewed by a heartbeat every `HEARTBEAT_SECONDS` (10 s). Set `LEASE_SECONDS=5` and make each unit take 15 s (`FAULT_SLOW_MS=15000`):
the lease expires 5 s before the first heartbeat could renew it. While expired, the unit is up for grabs: whichever worker next calls `LeaseWork` takes it (expired units sort ahead of fresh ones), so two workers compute the same unit.
The first worker later either learns from its heartbeat reply (`lost_unit_ids`) that the lease is gone and abandons the unit at its next 50 ms slice (`hearts_worker_wasted_units_total`), or finishes and gets
`accepted=false` from `CompleteWork` (`hearts_units_duplicate_total`, `hearts_worker_units_completed_total{result="duplicate"}`). Only the first completion counts (the SQL is `UPDATE ... WHERE state NOT IN ('done','cancelled')`), and
seeds are deterministic, so **standings are still identical**: the cost is efficiency, not correctness. `hearts_units_reclaimed_total` and the reclaim rate soar (`HeartsHighReclaimRate`).
(The script also raises `MAX_ATTEMPTS` to 10, because each reclaim uses an attempt and 3 attempts could exhaust by bad luck, which would mark units `failed` and spoil the comparison. `KEEP_MAX_ATTEMPTS=1` lets you watch that failure.)

**Command.** `scripts/gamedays/09-short-lease.sh` (takes several minutes: 50 units x 15 s over 3 workers). Knobs: `LEASE=5 SLOW_MS=15000 KEEP_MAX_ATTEMPTS=0`.

**Watch.**
- `Hearts Overview` -> "Unit event rates": reclaimed and duplicate lines above zero.
  `sum(rate(hearts_units_reclaimed_total{namespace="hearts"}[1m]))`, `sum(rate(hearts_units_duplicate_total{namespace="hearts"}[1m]))`
- `Hearts Worker Performance` -> "Wasted units/s", "Worker results/s" (`duplicate`), "Busy fraction (per pod)" (busy but producing little).
- "Hands completed per second" vs "Hands simulated/s per pod": effective throughput below raw work.

**Diagnose (practise).**
```bash
kubectl -n hearts logs deploy/hearts-worker --since=3m | grep -E 'lease lost|abandoned|already done'
curl -s $URL/api/jobs/<id> | jq .progress.reclaims
make psql   # SELECT id, attempts, state, lease_owner, completed_by FROM work_units WHERE attempts > 1 ORDER BY attempts DESC LIMIT 20;
kubectl -n hearts get deploy hearts-coordinator -o jsonpath='{.spec.template.spec.containers[0].env}' | jq
```

**Recover.** `kubectl -n hearts set env deploy/hearts-coordinator LEASE_SECONDS- MAX_ATTEMPTS-` and `kubectl -n hearts set env deploy/hearts-worker FAULT_SLOW_MS-` (the script does).

**Pass criteria (automatic).** Job `done`, standings identical, `reclaims >= 1`, and at least one duplicate completion or wasted unit observed.

**Questions.**
1. What is the relation between `LEASE_SECONDS`, `HEARTBEAT_SECONDS` and the longest unit? Which rule of thumb keeps duplicates rare?
2. Why is the result still correct? Which two properties (idempotent completion, deterministic seeds) carry the proof?
3. What is the cost of a very *long* lease instead? (Dead workers hold units longer: drill 1.)
4. What does the heartbeat reply `lost_unit_ids` buy, versus workers discovering the loss only at `CompleteWork`?

---

## Drill 10: Whole node down (manual)

**Hypothesis.** The control plane does not know a node died; it only notices missing kubelet heartbeats.
- After `node-monitor-grace-period` (40 s by default) the node becomes `NotReady`, its pods turn not-Ready and leave the Service endpoints, and the node gets the taint `node.kubernetes.io/unreachable`.
- Pods tolerate that taint for `tolerationSeconds` = **300 s** by default, so Kubernetes does **not** delete them, and replacements are not created, for 5 minutes.
- **The job does not wait for Kubernetes.** Leases on the dead node's units expire after ≤ 30 s and the surviving workers reclaim them, so the job completes on the remaining pods long before any pod is rescheduled.
  Fewer workers means slower, not wrong: standings equal the reference.
- Part B (the node hosting Postgres): everything stops. The DB pod goes `Unknown`/`Terminating`; the StatefulSet will not start a second `hearts-db-0` while it cannot be sure the first is dead, and the local PV is on the dead disk anyway. Recovery requires the node to return, or a restore from a dump into a fresh PVC.

**Command.** `scripts/gamedays/10-node-down.sh` prints the full step-by-step plan. Run it in a terminal (without `--yes`) for the guided mode: it submits the reference job when you say go, waits for it, checks the standings, then waits for you to power the node on and checks the cluster converged. With `--yes` or no terminal it only prints the instructions.

Short version:
1. Part A: choose a worker node that does **not** host Postgres (`kubectl -n hearts get pod hearts-db-0 -o wide`). Never the control plane.
2. Start the job, hard power off the node (hypervisor "power off", not a clean shutdown).
3. Watch the timeline: +0 s pods look Running; +40 s node NotReady, pods not-Ready; ≥ 30 s units reclaimed; +5 min pods evicted and replaced (if resources allow).
4. Power on. Check `kubectl get nodes`, `kubectl -n hearts get pods -o wide`.

**Watch.**
- `Hearts Overview` -> "Active workers over time", "Units by state", "Unit event rates" (reclaims).
- `Hearts Worker Performance` -> "Hands simulated/s per node": one node's line stops.
- PromQL: `kube_node_status_condition{condition="Ready", status="true"}` (node goes to 0), `sum(increase(hearts_units_reclaimed_total{namespace="hearts"}[5m]))`.

**Diagnose (practise).**
```bash
kubectl get nodes
kubectl describe node <node> | sed -n '/Conditions:/,/Addresses:/p;/Taints:/p'
kubectl get events -A --sort-by=.lastTimestamp | tail -30
kubectl -n hearts get pods -o wide | grep <node>          # Running but not Ready, later Terminating/Unknown
kubectl -n hearts get endpoints hearts-worker-metrics hearts-worker-headless
```

**Recover.** Power the node on; stuck pods are cleaned up and the cluster converges. For a permanently dead node: `kubectl delete node <node>` makes Kubernetes forget it (pods are then deleted at once); for the DB, restore from backup.

**Pass criteria.** A: reference job `done`, standings identical, `reclaims >= 1` if the dead node held a leased unit, all pods Ready after power-on. B: all of that after power-on, and you can explain why the DB pod could not move.

**Questions.**
1. Which timers decided each step (40 s, 30 s, 300 s)? Which one would you shorten (`tolerationSeconds` on the workers) and what is the risk of too-short values (flapping nodes causing churn)?
2. Why did the job finish before Kubernetes replaced the pods? What does that say about application-level leases vs infrastructure failure detection?
3. Why does a StatefulSet refuse to start a replacement automatically? What does "at most one" protect (two Postgres on one disk)?
4. With 3 workers on 3 nodes and the PDB, can you afford a node loss for *capacity*, given each pod requests 2 of ~6 CPUs?

---

## Blind mode

Diagnosing a failure you caused and *know* about is easy. Real incidents arrive without a label. Blind mode trains the real skill: **work from symptoms to root cause.**

You need a second person (a friend, or yourself a week later after forgetting which one you queued).

**The helper (runs the drill).**
1. Pick a random drill without telling the diagnoser, for example `ls scripts/gamedays/0*.sh | sort -R | head -1` (for a harder game include `10`).
2. Give the diagnoser a head start rule: the helper runs it with output hidden, so the banner does not spoil it:
   ```bash
   HOLD_SECONDS=300 STRETCH_MS=3000 scripts/gamedays/0N-....sh --yes > /dev/null 2>&1
   ```
   (`HOLD_SECONDS` is only used by drill 4; `STRETCH_MS` lengthens drills 2, 3, 4 and 7 so there is time to diagnose.)
3. Say only: "something is wrong with Hearts; the UI/job was fine 5 minutes ago." Start a timer.

**The diagnoser.**
- Rules: no reading `scripts/gamedays/`, no looking at the helper's terminal. Allowed: `kubectl`, Grafana, `curl`, `docs/RUNBOOK.md`.
- Start with the runbook's "First 5 commands", write down for each command what it told you, form a hypothesis, test it with one more command, then name the **mechanism**
  (not just "a pod restarted" but "OOMKilled by the cgroup limit").
- Do not fix it by editing things: in blind mode the script restores everything itself when it finishes. Say what fix you *would* apply and the command. Optional: the helper aborts the script (Ctrl-C), which also restores.

**Scoring** (per drill, 0 to 2 each): found the right component; named the mechanism; said the correct recovery command; explained the blast radius (what was and was not affected); time to first correct hypothesis (< 5 min = 2, < 15 = 1).
Keep a log: date, drill, minutes, score, and what you would check first next time.

**Symptom cheat sheet (spoilers, for the helper or for review after).**

| Symptom as seen by the diagnoser | Drill | First giveaway |
|---|---|---|
| One worker disappeared, a new one appeared; job slower; `reclaims` ≥ 1 | 1 | `Killing` event; `progress.reclaims`; coordinator `hearts_units_reclaimed_total` |
| UI errors for a while then recovers; workers log `backing off`; coordinator pod age tiny | 2 | coordinator pod restarted; `hearts_worker_lease_errors_total` |
| `/readyz` 503, 5xx everywhere; `pg_up` 0; `hearts-db-0` restarted | 3 | `hearts-db-0` AGE small; coordinator Ready=false but RESTARTS=0 |
| Everything down, `hearts-db-0` `Pending`; a node `SchedulingDisabled` | 4 | `volume node affinity conflict` in the pod events |
| Jobs 4x slower, all pods Running, no errors | 5 | "CPU throttling % (per pod)" high; resources.limits.cpu small; `WORKER_THREADS` set |
| Worker pods `CrashLoopBackOff`, `RESTARTS` growing | 6 | `Last State: OOMKilled, exit code 137` |
| Deploy "stuck", one extra worker pod `ImagePullBackOff`, everything else fine | 7 | `rollout status` hangs; image tag not found |
| Job status `failed`, units failed, pods healthy, restarts zero | 8 | `last_error` mentions `injected fault`; `FAULT_PANIC_EVERY_N` in the env |
| Job completes but takes far longer than expected; huge reclaims; wasted units | 9 | `LEASE_SECONDS=5` on the coordinator, `FAULT_SLOW_MS` on workers |
| A node `NotReady`; its pods Running but not Ready; progress continues slowly | 10 | `kubectl get nodes`; taint `node.kubernetes.io/unreachable` |

---

## Appendix: reproducing the reference result locally

```bash
export PATH=/opt/homebrew/opt/rustup/bin:$PATH
cargo run --release -p hearts-cli -- tournament --hands 100000 --seed 42 --strategies heuristic,heuristic,random,pimc:20
# or with the compose stack:
make dev       # then:  BASE_URL=http://localhost:8080 curl ... (submit the reference job from the UI)
```
Expected points per slot: `481853 479038 1459749 381744`. Killing a worker mid-job locally (compose) gave `reclaims: 1` and the same standings, as in drill 1.
If this ever changes, someone altered the engine's random stream or seeding (`crates/hearts-core/src/seed.rs`, `rng.rs`, `tournament.rs`, `strategy.rs`), and all drill pass criteria need the new numbers
(update `REF_POINTS` in `scripts/gamedays/lib.sh` and this document).
