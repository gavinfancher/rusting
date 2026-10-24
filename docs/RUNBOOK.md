# Hearts runbook

How to run, scale, back up, and diagnose Hearts on the k3s cluster. The alert sections at the
bottom are linked from the Prometheus alerts (`deploy/base/prometheus-rules.yaml`, annotation
`runbook_url`). Each heading name below equals the alert name, so the GitHub anchors
(`#heartscoordinatordown` and so on) match.

Related documents: [ARCHITECTURE.md](ARCHITECTURE.md) (why it is built this way),
[CONTRACTS.md](CONTRACTS.md) (API, env vars, metrics), [GAMEDAYS.md](GAMEDAYS.md) (break it on
purpose), [EXPERIMENTS.md](EXPERIMENTS.md) (measure it), `deploy/README.md` (first-time setup).

Conventions: commands assume `kubectl` points at the k3s cluster. `-n hearts` is the app namespace
(`make` targets set it for you). `$URL` below means `https://hearts.k8s.ad.gavinf.com`.

- [Architecture at a glance](#architecture-at-a-glance)
- [First 5 commands when something is wrong](#first-5-commands-when-something-is-wrong)
- [Deploy](#deploy), [roll back](#roll-back), [scale](#scale)
- [Database access and useful SQL](#database-access-and-useful-sql)
- [Backup and restore](#backup-and-restore), [rotate the DB password](#rotating-the-db-password)
- [Alerts](#alerts) (one section per alert)
- [Glossary](#glossary)

## Architecture at a glance

```
 browser ──HTTPS──► Traefik (wildcard cert) ──► Service hearts-coordinator :8080
                                                      │
                          ┌───────────────────────────┴────────────────────────┐
                          │ Deployment hearts-coordinator (1 replica, Recreate)│
                          │  HTTP :8080  UI, /api/*, SSE, /metrics, probes      │
                          │  gRPC :50051 WorkerService (LeaseWork/Heartbeat/    │
                          │              CompleteWork)                          │
                          └───────┬───────────────────────────▲─────────────────┘
                 SQL (sqlx pool)  │                           │ workers PULL work
          ┌───────────────────────▼───┐          ┌────────────┴──────────────────┐
          │ StatefulSet hearts-db     │          │ Deployment hearts-worker (3)  │
          │  pod hearts-db-0          │          │  rayon pool, threads = CPU    │
          │  container postgres:18.4  │          │  gRPC :50052 AdvisorShard     │
          │  + postgres-exporter :9187│          │  HTTP :9090 /metrics /healthz │
          │  PVC data-hearts-db-0     │          │         /readyz              │
          │  (local-path, one node)   │          └───────────────────────────────┘
          └───────────────────────────┘      advisor fan-out: coordinator ─► every worker pod IP
                                              (DNS: Service hearts-worker-headless)
```

| Thing | Value |
|---|---|
| App namespace | `hearts` |
| Monitoring namespace | `monitoring` (Helm release `kube-prometheus-stack`) |
| Nodes | `k8s-cp` (control plane, tainted `CriticalAddonsOnly`), `k8s-w1`, `k8s-w2`, `k8s-w3` (6 cores, 32 GB, amd64, 1 GbE) |
| UI | https://hearts.k8s.ad.gavinf.com (Ingress `hearts`, class `traefik`) |
| Grafana | https://grafana.k8s.ad.gavinf.com (dashboards: `Hearts Overview`, `Hearts Worker Performance`, `Hearts Advisor`, `Hearts Postgres`; UIDs `hearts-overview`, `hearts-workers`, `hearts-advisor`, `hearts-postgres`) |
| Images | `ghcr.io/gavinfancher/hearts-coordinator` and `hearts-worker`, tags `sha-<7 chars>` and `main` |
| Workers | Deployment `hearts-worker`, label `app.kubernetes.io/name=hearts-worker`, requests == limits (2 CPU, 1Gi) = QoS Guaranteed, PodDisruptionBudget `hearts-worker` (maxUnavailable 1) |
| Services | `hearts-coordinator` (8080, 50051), `hearts-worker-headless` (50052, DNS only), `hearts-worker-metrics` (9090), `hearts-db` (5432, 9187) |
| Config | ConfigMaps `hearts-coordinator-config`, `hearts-worker-config` (env vars, see CONTRACTS.md); Secret `hearts-db-<hash>` (from `deploy/overlays/homelab/db.env`) |
| Probes | coordinator `/healthz` (liveness), `/readyz` (readiness: DB `SELECT 1` within 1 s); worker `/healthz`, `/readyz` (ready if the coordinator was reachable in the last max(30 s, 3 x heartbeat)) |
| Alerts | PrometheusRule `hearts-alerts` |

The one-paragraph mental model: **the coordinator is stateless, Postgres is the only state, workers pull
units under 30-second leases.** If a component dies, the lease clock (not a health check) decides when
its work is handed to someone else.

## First 5 commands when something is wrong

Run them in this order; stop when you have a lead.

```bash
# 1. What is running where, and is anything not Ready / restarting / Pending?
make status                     # kubectl get pods -o wide + kubectl top pods
#    (or: kubectl -n hearts get pods -o wide)

# 2. What did Kubernetes itself notice recently? (scheduling, probes, OOM, image pulls)
kubectl -n hearts get events --sort-by=.lastTimestamp | tail -30

# 3. Does the app think it is healthy? (needs DB) And who is registered?
curl -s https://hearts.k8s.ad.gavinf.com/readyz -w ' HTTP %{http_code}\n'
curl -s https://hearts.k8s.ad.gavinf.com/api/workers | jq '.[] | {pod_name, node_name, active, leased_units}'

# 4. What do the logs say? (JSON lines; grep for level)
kubectl -n hearts logs deploy/hearts-coordinator --tail=50
make logs-worker                # follows all workers, prefixed by pod
#    A crashed container: kubectl -n hearts logs <pod> --previous

# 5. Zoom in on the one bad object
kubectl -n hearts describe pod <pod>     # read the Events at the bottom and "Last State"
```

Then open Grafana, `Hearts Overview`: Active workers, Pending units, Failed units, Unit event rates.

Quick triage table:

| Symptom | Look at | Probably |
|---|---|---|
| UI does not load, 404/502 from Traefik | `kubectl -n hearts get pods`, `get endpoints hearts-coordinator` | coordinator not Ready: DB down, or pod restarting. See [HeartsCoordinatorDown](#heartscoordinatordown), [HeartsPostgresDown](#heartspostgresdown) |
| UI loads but "no workers" / job at 0% | `curl $URL/api/workers`, `kubectl -n hearts get pods -l app.kubernetes.io/name=hearts-worker` | workers Pending/CrashLoop, or cannot reach coordinator: [HeartsWorkersDown](#heartsworkersdown) |
| Job progresses then freezes | `hearts_units` by state, worker logs | [HeartsJobStalled](#heartsjobstalled) |
| Jobs slow, no errors | "CPU throttling % (per pod)", "Hands simulated/s per node" | [HeartsCPUThrottlingHigh](#heartscputhrottlinghigh), noisy neighbour, fewer workers than you think |
| Job status `failed` | `SELECT ... last_error` (see SQL below) | [HeartsUnitsFailed](#heartsunitsfailed) |
| Pod `Pending` | `kubectl describe pod` Events | not enough CPU/memory, or PV node affinity (Postgres) |
| `ImagePullBackOff` | `kubectl describe pod` | wrong `newTag`, GHCR package private without pull secret |
| `CrashLoopBackOff` / `OOMKilled` | `logs --previous`, `describe` "Last State" | [HeartsPodCrashLooping](#heartspodcrashlooping) |

## Deploy

First-time setup (secret, monitoring stack, image pull access) is in `deploy/README.md`.

### Roll out a new image

1. Merge to `main`. CI (`.github/workflows/ci.yml`) builds and pushes, for each of coordinator and worker,
   two tags: `ghcr.io/gavinfancher/hearts-<target>:sha-<first 7 chars of the commit>` and `:main`.
2. Prefer the immutable `sha-` tag over `main` (a moving tag can differ between nodes and does not change
   the Deployment spec, so nothing rolls). Edit `newTag` for **both** images in
   `deploy/overlays/homelab/kustomization.yaml`:
   ```yaml
   images:
     - name: ghcr.io/gavinfancher/hearts-coordinator
       newName: ghcr.io/gavinfancher/hearts-coordinator
       newTag: sha-1a2b3c4
     - name: ghcr.io/gavinfancher/hearts-worker
       newName: ghcr.io/gavinfancher/hearts-worker
       newTag: sha-1a2b3c4
   ```
3. Look before you leap, then apply:
   ```bash
   kubectl diff -k deploy/overlays/homelab | head -50    # what will change? (read-only)
   make deploy                                           # kubectl apply -k deploy/overlays/homelab
   kubectl -n hearts rollout status deploy/hearts-coordinator
   kubectl -n hearts rollout status deploy/hearts-worker
   ```
4. What to expect:
   - **Coordinator** uses `strategy: Recreate`: the old pod stops, then the new one starts. The API
     is down for roughly 10 to 20 seconds. Workers back off and retry; running jobs continue.
     (Migrations run at startup, which is why two versions must never overlap.)
   - **Workers** use the default RollingUpdate (surge 1, unavailable 0 with 3 replicas): a new pod
     starts, becomes Ready, then an old one is terminated. On SIGTERM a worker finishes its current
     unit (up to `SHUTDOWN_GRACE_SECONDS=45`, inside the pod's 60 s `terminationGracePeriodSeconds`),
     so a rollout normally causes **zero reclaims**.
   - **Postgres** is not touched unless you edit `postgres.yaml` or `db.env`.
5. Verify: `make status`, `curl $URL/api/workers | jq length`, Grafana `Hearts Overview`, and run a
   small job (the UI, or `hearts-cli`-sized: 20000 hands).

Preview without applying: `kubectl kustomize deploy/overlays/homelab`.

Config-only change (ConfigMap, for example `LEASE_SECONDS`): edit `deploy/base/configmap.yaml`,
`make deploy`, then restart the pods yourself, because Kubernetes does **not** restart pods when a
ConfigMap changes:
```bash
kubectl -n hearts rollout restart deploy/hearts-coordinator deploy/hearts-worker
```

### Roll back

Two ways. They are not equivalent.

| | `kubectl rollout undo` | Revert the tag in git |
|---|---|---|
| Command | `kubectl -n hearts rollout undo deploy/hearts-worker` (add `--to-revision=N`; list with `rollout history`) | edit `newTag` back (or `git revert`), `make deploy` |
| Speed | immediate, no git | one commit, one apply |
| Source of truth | **cluster only**: git still says the bad tag | git and cluster agree |
| Trap | the next `make deploy` re-applies the bad tag from git | none |

Use `rollout undo` as the emergency brake (fast, stops the bleeding), and then **immediately** make git
match (revert `newTag`), otherwise the next deploy reintroduces the bug. `rollout undo` only knows the
previous ReplicaSet revisions (10 are kept); `kubectl -n hearts rollout history deploy/hearts-worker`.
Practice: [game day 7](GAMEDAYS.md#drill-7-bad-deploy).

A bad **worker** image stalls harmlessly (the new pod `ImagePullBackOff`s or crash-loops while old pods
keep serving). A bad **coordinator** image is worse: with `Recreate` the old pod is already gone, so the
API is down until you undo.

## Scale

| What | How | Notes |
|---|---|---|
| Worker pods | `make scale-workers N=5` (`kubectl -n hearts scale deploy/hearts-worker --replicas=5`) | The next `make deploy` resets to the manifest value (3); put lasting values in the overlay `patches:` (see the comment in `deploy/overlays/homelab/kustomization.yaml`). |
| CPU per worker | overlay patch, or `kubectl -n hearts set resources deploy/hearts-worker --requests=cpu=4,memory=2Gi --limits=cpu=4,memory=2Gi` | Keep requests == limits (Guaranteed QoS, repeatable benchmarks). Threads follow the limit unless `WORKER_THREADS` is set. Triggers a rollout. |
| Threads | `kubectl -n hearts set env deploy/hearts-worker WORKER_THREADS=4` (remove: `WORKER_THREADS-`) | More threads than the CPU limit means CFS throttling (experiment E4). |
| Coordinator | leave at 1 replica | Migrations run at startup and the strategy is `Recreate`. Multi-replica needs a migration Job first (ARCHITECTURE.md, "Trade-offs"). |
| Postgres | leave at 1 | Single pod on a local PV. |

**Capacity maths.** Allocatable CPU per worker node is a bit under 6 (system pods and DaemonSets take
requests too). Workers request 2 CPU each, so a node fits about 2 workers, the three worker nodes about
6 in total. Postgres (250m), the coordinator (500m) and the monitoring stack also need room. More than
that and extra pods sit `Pending` with `0/4 nodes are available: ... Insufficient cpu` (that message
is the scheduler telling you exactly what is wrong). Check:
```bash
kubectl describe node k8s-w1 | sed -n '/Allocated resources/,/Events/p'
kubectl top nodes
```
Scheduling preference: `topologySpreadConstraints` (maxSkew 1 over `kubernetes.io/hostname`,
`ScheduleAnyway`) spreads workers across nodes but never blocks scheduling.

After scaling, confirm: `curl $URL/api/workers | jq '[.[] | select(.active)] | length'` (old pods stay
"active" for up to 30 s after they die).

## Database access and useful SQL

```bash
make psql        # kubectl exec -it hearts-db-0 -c postgres -- psql as $POSTGRES_USER on $POSTGRES_DB
```
One-off queries without an interactive shell:
```bash
kubectl -n hearts exec hearts-db-0 -c postgres -- sh -c 'psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c "SELECT count(*) FROM jobs"'
```
Schema: `migrations/0001_init.sql` (tables `jobs`, `work_units`, `unit_results`, `workers`,
`benchmark_runs`; enums `job_status`, `unit_state`). Seeds are `BIGINT` with the same bit pattern as the
Rust `u64`, so large seeds show up negative.

```sql
-- Units by state, per job (is anything stuck in 'leased'?)
SELECT job_id, state, count(*) FROM work_units GROUP BY job_id, state ORDER BY job_id, state;

-- Recent jobs with their progress
SELECT j.id, j.name, j.status, j.num_hands, j.unit_size, j.created_at,
       count(*) FILTER (WHERE u.state = 'done')   AS done,
       count(*) FILTER (WHERE u.state = 'failed') AS failed,
       count(*)                                  AS units
FROM jobs j JOIN work_units u ON u.job_id = j.id
GROUP BY j.id ORDER BY j.id DESC LIMIT 10;

-- Units that were reclaimed or retried the most (attempts - 1 = reclaims/retries)
SELECT id, job_id, state, attempts, lease_owner, completed_by, last_error
FROM work_units WHERE attempts > 1 ORDER BY attempts DESC, id LIMIT 20;

-- Failed units and why
SELECT id, job_id, attempts, last_error FROM work_units WHERE state = 'failed' ORDER BY id DESC LIMIT 20;

-- Expired leases that nobody has reclaimed yet (should be ~0 while workers are busy)
SELECT count(*) FROM work_units WHERE state = 'leased' AND lease_expires_at < now();

-- Worker registry: who has been seen, and how long ago?
SELECT id, node_name, threads, first_seen, last_seen, now() - last_seen AS ago
FROM workers ORDER BY last_seen DESC;

-- Slowest completed units (compute_ms is the worker-side compute time)
SELECT id, job_id, hand_end - hand_start AS hands, compute_ms, completed_by
FROM work_units WHERE state = 'done' ORDER BY compute_ms DESC NULLS LAST LIMIT 20;

-- Who did the work for a job (is load balanced?)
SELECT completed_by, count(*) AS units, sum(compute_ms) AS total_ms
FROM work_units WHERE job_id = 123 AND state = 'done' GROUP BY completed_by ORDER BY units DESC;

-- Standings of a job (what the API returns), by strategy slot
SELECT r.slot, sum(r.points) AS points, sum(r.hands) AS hands
FROM unit_results r JOIN work_units u ON u.id = r.unit_id
WHERE u.job_id = 123 GROUP BY r.slot ORDER BY r.slot;

-- Benchmark history
SELECT experiment, worker_replicas, cpu_limit, threads_per_worker, unit_size, hands_per_sec, created_at
FROM benchmark_runs ORDER BY created_at DESC LIMIT 20;

-- Connections and long queries
SELECT state, count(*) FROM pg_stat_activity WHERE datname = current_database() GROUP BY state;
SELECT pid, now() - query_start AS running, left(query, 80) FROM pg_stat_activity
WHERE state <> 'idle' AND datname = current_database() ORDER BY running DESC;

-- Housekeeping: delete old jobs (units and results cascade)
DELETE FROM jobs WHERE created_at < now() - interval '30 days';
```

## Backup and restore

Postgres runs on a node-local disk (`local-path`). **Nothing replicates it.** Take dumps before risky
work (drill 4 and 10, a Postgres upgrade, changing storage).

### Backup

```bash
kubectl -n hearts exec hearts-db-0 -c postgres -- \
  sh -c 'pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" --format=custom' > hearts-$(date +%F).dump
ls -lh hearts-*.dump
pg_restore --list hearts-$(date +%F).dump | head      # sanity check (needs a local pg_restore), or:
kubectl -n hearts exec -i hearts-db-0 -c postgres -- pg_restore --list < hearts-$(date +%F).dump | head
```
Do not use `-t` / `-it` with `kubectl exec` here: a TTY would mangle the binary stream.
Keep the file off the cluster (laptop, NAS). The custom format includes the `_sqlx_migrations` table,
so a restored database is consistent with the coordinator's migrations.

### Restore (into the existing database)

```bash
# 1. stop writers so nothing is inserted during the restore
kubectl -n hearts scale deploy/hearts-coordinator --replicas=0
kubectl -n hearts scale deploy/hearts-worker --replicas=0
# 2. restore (--clean --if-exists drops existing objects first)
kubectl -n hearts exec -i hearts-db-0 -c postgres -- \
  sh -c 'pg_restore -U "$POSTGRES_USER" -d "$POSTGRES_DB" --clean --if-exists --no-owner' < hearts-2026-10-24.dump
# 3. start everything again
kubectl -n hearts scale deploy/hearts-coordinator --replicas=1
kubectl -n hearts scale deploy/hearts-worker --replicas=3
make status
```
Expect a few harmless notices about objects that did not exist. Check with the SQL above that your jobs are
back.

### Disaster: the volume is gone (node lost for good)

1. `kubectl -n hearts delete pod hearts-db-0 --force --grace-period=0` is **not** enough: the PV is
   bound to the dead node. Delete the claim to get a fresh one: `kubectl -n hearts delete pvc data-hearts-db-0`
   then delete the stuck pod; the StatefulSet recreates both on a healthy node (empty database).
2. Wait for `hearts-db-0` Ready; the coordinator re-creates the schema on its next start
   (`kubectl -n hearts rollout restart deploy/hearts-coordinator`).
3. Scale coordinator and workers to 0 and run the restore above.

## Rotating the DB password

Postgres only reads `POSTGRES_PASSWORD` **when it first initialises an empty data directory**. Changing the
Secret later does not change the password inside the database, so you must change it in both places:

```bash
# 1. change it inside Postgres (use the CURRENT credentials; user is POSTGRES_USER, "hearts" by default)
make psql
#    ALTER USER hearts WITH PASSWORD 'the-new-password';
#    \q
# 2. change it for the clients: edit deploy/overlays/homelab/db.env (git-ignored)
$EDITOR deploy/overlays/homelab/db.env       # POSTGRES_PASSWORD=the-new-password
# 3. redeploy: Kustomize renames the Secret (new hash), which rolls every pod that references it
make deploy
kubectl -n hearts rollout status deploy/hearts-coordinator
kubectl -n hearts rollout status statefulset/hearts-db
curl -s https://hearts.k8s.ad.gavinf.com/readyz -w ' HTTP %{http_code}\n'
```
Notes:
- Between step 1 and the end of step 3, *new* DB connections that use the old password fail; existing ones keep
  working. Do it when no job is running and expect about a minute of downtime (coordinator `Recreate`,
  and the StatefulSet pod restarts too because its Secret reference changed).
- The postgres-exporter sidecar uses the same Secret, so `pg_up` returns once the pod is back.
- The alternative of deleting the PVC (`data-hearts-db-0`) wipes all data. Only do that after a backup.

## Alerts

All alerts are defined in `deploy/base/prometheus-rules.yaml` (PrometheusRule `hearts-alerts`). See them
firing in Grafana (Alerting) or in Prometheus: `make port-forward-grafana`, or
`kubectl -n monitoring port-forward svc/kube-prometheus-stack-prometheus 9090` then http://localhost:9090/alerts.

### HeartsCoordinatorDown

- **Meaning.** Prometheus cannot scrape the coordinator (or the target vanished) for 1 minute. No work is
  being handed out and the UI/API is gone. Severity: critical.
- **Likely causes.** Pod crash-looping or not Ready (database down: readiness checks `SELECT 1`); a rollout with `Recreate` that is
  stuck (bad image); the node is down; the ServiceMonitor or Service labels were changed (target missing).
- **PromQL.**
  ```
  sum(up{namespace="hearts", service="hearts-coordinator"}) == 0 or absent(up{namespace="hearts", service="hearts-coordinator"})
  ```
- **Diagnose.**
  ```bash
  kubectl -n hearts get pods -l app.kubernetes.io/name=hearts-coordinator -o wide
  kubectl -n hearts describe pod -l app.kubernetes.io/name=hearts-coordinator | sed -n '/Events:/,$p'
  kubectl -n hearts logs deploy/hearts-coordinator --tail=50
  kubectl -n hearts logs deploy/hearts-coordinator --previous          # if it restarted
  kubectl -n hearts get endpoints hearts-coordinator                   # empty = not Ready
  kubectl -n hearts get pods hearts-db-0                               # is the database up?
  ```
  The logs contain `database not ready, retrying` while it waits for Postgres.
- **Fix.** DB down: see [HeartsPostgresDown](#heartspostgresdown). Bad image: `kubectl -n hearts rollout undo deploy/hearts-coordinator`,
  then fix the tag in git. Node down: wait for it, or see game day 10. If only the target is missing
  but the app works, check `kubectl -n hearts get servicemonitor` and the `release: kube-prometheus-stack` label.

### HeartsWorkersDown

- **Meaning.** `hearts_workers_active` has been 0 for 2 minutes while units are pending: work exists but nobody is
  computing. ("Active" = a worker called the coordinator within the last 30 s.) Severity: critical.
- **Likely causes.** All worker pods Pending (no CPU left, a drained or down node), crash-looping (bad image, OOMKilled),
  workers cannot reach the coordinator (Service/DNS/NetworkPolicy, coordinator down), or `replicas` is 0.
- **PromQL.**
  ```
  hearts_workers_active == 0 and on() sum(hearts_units{state="pending"}) > 0
  ```
- **Diagnose.**
  ```bash
  kubectl -n hearts get deploy hearts-worker
  kubectl -n hearts get pods -l app.kubernetes.io/name=hearts-worker -o wide
  kubectl -n hearts describe pod <worker-pod> | sed -n '/Events:/,$p'
  kubectl -n hearts logs <worker-pod> --tail=30        # look for "LeaseWork failed; backing off"
  kubectl get nodes
  curl -s $URL/api/workers | jq
  ```
- **Fix.** Pending: free CPU (scale something else down) or lower the per-pod request. Crash loop: see
  [HeartsPodCrashLooping](#heartspodcrashlooping). `LeaseWork failed` with a reachable coordinator pod: check the Service
  `hearts-coordinator` port 50051 and that the coordinator is Ready. `replicas: 0`: `make scale-workers N=3`.

### HeartsJobStalled

- **Meaning.** Units are `pending` or `leased`, but no unit has completed in 5 minutes. Severity: warning.
- **Likely causes.** Workers dead or busy on enormous units (a `unit_size` far too big for the speed, for example PIMC with 200
  samples and 1e6 hands per unit: a single unit may legitimately take longer than 5 minutes); coordinator
  or database down; every worker is failing the unit (but then `HeartsUnitsFailed` fires first);
  `FAULT_SLOW_MS` left set from a drill.
- **PromQL.**
  ```
  sum(hearts_units{state=~"pending|leased"}) > 0 and on() sum(rate(hearts_units_completed_total[5m])) == 0
  ```
- **Diagnose.**
  ```bash
  curl -s $URL/api/jobs?limit=3 | jq '.[] | {id, status, progress}'
  curl -s $URL/api/workers | jq '.[] | {pod_name, active, leased_units}'
  kubectl -n hearts get deploy hearts-worker hearts-coordinator -o jsonpath='{range .items[*]}{.metadata.name}{": "}{.spec.template.spec.containers[0].env}{"\n"}{end}'   # leftover FAULT_* overrides?
  kubectl -n hearts logs <busy-worker> --tail=20       # "computing unit ... hands=N"
  ```
  SQL: expired leases and unit ages (see above). Grafana: `Hearts Worker Performance` -> "Busy fraction (per pod)" and
  "Unit duration p50 / p95".
- **Fix.** Slow units: cancel the job (`curl -X POST $URL/api/jobs/<id>/cancel`) and resubmit with a smaller `unit_size`.
  Workers idle but units leased forever: they expire after `LEASE_SECONDS` and are reclaimed by the next lease call; if
  not, the lease loop is not running, so restart the workers (`kubectl -n hearts rollout restart deploy/hearts-worker`).
  Remove stray `FAULT_*` env: `kubectl -n hearts set env deploy/hearts-worker FAULT_SLOW_MS- FAULT_PANIC_EVERY_N-`.

### HeartsHighReclaimRate

- **Meaning.** More than 10% of the units leased in the last 10 minutes were *reclaims* of an expired lease. Workers are
  dying, too slow to heartbeat, or the lease is shorter than the unit. Reclaims are normal after a kill (a few);
  a sustained rate is not. Severity: warning.
- **Likely causes.** Pods being killed (OOMKilled, evictions, node trouble, a rollout with the grace period exceeded); CPU
  starvation making heartbeats late; `LEASE_SECONDS` lowered below `HEARTBEAT_SECONDS` plus unit time (game day 9);
  network trouble between workers and coordinator; database slowness delaying `Heartbeat`.
- **PromQL.**
  ```
  sum(increase(hearts_units_reclaimed_total[10m])) / sum(increase(hearts_units_leased_total[10m])) > 0.1
  ```
- **Diagnose.**
  ```bash
  kubectl -n hearts get pods -l app.kubernetes.io/name=hearts-worker      # RESTARTS column
  kubectl -n hearts get events --sort-by=.lastTimestamp | grep -i -E 'kill|evict|oom|unhealthy'
  kubectl -n hearts logs deploy/hearts-worker --since=15m | grep -i -E 'lease lost|abandon|Heartbeat failed'
  kubectl -n hearts get deploy hearts-coordinator -o jsonpath='{.spec.template.spec.containers[0].env}'   # LEASE_SECONDS?
  ```
  Grafana: "Unit event rates" (reclaims vs duplicates), "Wasted units/s", "Pod restarts (last 1h)", "DB query p95 by query" (is `heartbeat` slow?).
  SQL: units with `attempts > 1` and their `lease_owner`.
- **Fix.** Fix the cause: raise limits (OOM), restore `LEASE_SECONDS` (30), remove fault env, relieve CPU pressure. Reclaims are
  *safe* (results stay identical, only work is wasted), so this alert is about cost and a symptom of something else.

### HeartsUnitsFailed

- **Meaning.** At least one unit exhausted `MAX_ATTEMPTS` (3) in the last 10 minutes and is permanently `failed`; its job ends
  as `failed`. Severity: warning. This is always a *logic or data* problem, not infrastructure.
- **Likely causes.** A panic in the engine for a specific input (a "poison unit"): a bug, or `FAULT_PANIC_EVERY_N` set (game day
  8); an invalid job spec that passes validation but fails in the worker (`bad job spec`); a unit that
  repeatedly loses its lease before it can finish (see HeartsHighReclaimRate: the attempts are used up by reclaims).
- **PromQL.**
  ```
  sum(increase(hearts_units_failed_total[10m])) > 0
  ```
- **Diagnose.**
  ```sql
  SELECT id, job_id, hand_start, hand_end, attempts, last_error FROM work_units WHERE state = 'failed' ORDER BY id DESC LIMIT 10;
  ```
  ```bash
  kubectl -n hearts logs deploy/hearts-worker --since=15m | grep -i -E 'panic|unit failed'
  kubectl -n hearts get deploy hearts-worker -o jsonpath='{.spec.template.spec.containers[0].env}'   # FAULT_PANIC_EVERY_N?
  ```
  Grafana: "Failed units", "Worker results/s" (result="error").
- **Fix.** Remove injected faults (`kubectl -n hearts set env deploy/hearts-worker FAULT_PANIC_EVERY_N-`). For a real bug: take
  `hand_start`..`hand_end` and the job `spec` from the database and reproduce locally with
  `hearts-cli tournament` or `hearts-cli play` (same seed, same hands), fix, redeploy, resubmit the job. A failed unit is never retried automatically; resubmit the job (deterministic, so a
  fixed build yields the right numbers). The alert clears 10 minutes after the last failure.

### HeartsCPUThrottlingHigh

- **Meaning.** A worker container spent more than 25% of its CFS periods throttled for 5 minutes. The kernel froze the container's threads
  because they used up the CPU quota (`limit x 100 ms` per 100 ms period). Throughput drops and benchmark numbers are skewed. Severity: warning.
- **Likely causes.** More threads than the CPU limit (`WORKER_THREADS` set higher than the limit, experiment E4); the limit was lowered (game day 5);
  a burst from the advisor sharing the pool; for fractional limits, rounding of `available_parallelism`.
- **PromQL.**
  ```
  sum by (pod) (rate(container_cpu_cfs_throttled_periods_total{namespace="hearts", pod=~"hearts-worker-.*", container="worker"}[5m]))
  /
  sum by (pod) (rate(container_cpu_cfs_periods_total{namespace="hearts", pod=~"hearts-worker-.*", container="worker"}[5m])) > 0.25
  ```
- **Diagnose.**
  ```bash
  kubectl -n hearts get deploy hearts-worker -o jsonpath='{.spec.template.spec.containers[0].resources}{"\n"}{.spec.template.spec.containers[0].env}{"\n"}'
  kubectl -n hearts logs <worker-pod> | grep 'building rayon pool'    # threads=N requested=M
  kubectl -n hearts top pods -l app.kubernetes.io/name=hearts-worker
  ```
  Grafana `Hearts Worker Performance`: "CPU throttling % (per pod)", "CPU cores used vs limit (per pod)" (usage pinned at the limit line), "Threads per worker".
- **Fix.** Remove the `WORKER_THREADS` override (`kubectl -n hearts set env deploy/hearts-worker WORKER_THREADS-`) so threads
  follow the limit, or raise the CPU limit (requests == limits). Do not "fix" it by removing limits: the benchmark guarantee is lost.

### HeartsPostgresDown

- **Meaning.** `pg_up` is 0: postgres-exporter cannot connect to the database for 1 minute. The coordinator's `/readyz` fails too, so the
  whole application is unavailable. Severity: critical.
- **Likely causes.** Pod `hearts-db-0` restarting or `Pending` (**node cordoned/down: the PV is pinned to that node**, game day 4/10); disk full on
  the node; OOMKilled (limit 2Gi); a wrong password after rotation (the *exporter* then reports down while Postgres is fine); PVC not bound.
- **PromQL.**
  ```
  pg_up{namespace="hearts"} == 0
  ```
- **Diagnose.**
  ```bash
  kubectl -n hearts get pod hearts-db-0 -o wide
  kubectl -n hearts describe pod hearts-db-0 | sed -n '/Events:/,$p'      # FailedScheduling / volume node affinity conflict?
  kubectl -n hearts logs hearts-db-0 -c postgres --tail=30
  kubectl -n hearts logs hearts-db-0 -c postgres-exporter --tail=10        # "password authentication failed"?
  kubectl -n hearts get pvc,pv
  kubectl get nodes
  kubectl describe node <db-node> | grep -i -E 'pressure|taint|unschedulable'
  ```
  Grafana `Hearts Postgres`: "Postgres up", "Database size".
- **Fix.** `Pending` with volume node affinity: `kubectl uncordon <node>` or bring the node back; the data lives only there. Crash loop with disk full:
  free space on the node (`/var/lib/rancher/k3s/storage`), delete old jobs (SQL above). Exporter auth failure: make the Secret and the in-DB password
  match (see "Rotating the DB password"). No recovery possible: restore a dump (see "Backup and restore").

### HeartsPodCrashLooping

- **Meaning.** A container in namespace `hearts` restarted more than twice in 15 minutes. Kubernetes keeps restarting it with exponential back-off
  (10 s, 20 s, 40 s ... up to 5 minutes), shown as `CrashLoopBackOff`. Severity: warning.
- **Likely causes.** `OOMKilled` (memory limit too low; exit code 137); the process exits at start (bad config such as an unparsable env var,
  `DATABASE_URL` wrong, port in use); liveness probe failing (`/healthz` not answering because the process is wedged); a bug panic outside `catch_unwind`.
- **PromQL.**
  ```
  increase(kube_pod_container_status_restarts_total{namespace="hearts"}[15m]) > 2
  ```
- **Diagnose.**
  ```bash
  kubectl -n hearts get pods                                   # STATUS and RESTARTS
  kubectl -n hearts describe pod <pod> | sed -n '/Last State/,/Ready:/p'   # Reason: OOMKilled / Error, Exit Code
  kubectl -n hearts logs <pod> --previous                      # the crashed instance's last lines
  kubectl -n hearts get events --sort-by=.lastTimestamp | tail -20
  kubectl -n hearts get pod <pod> -o jsonpath='{.status.containerStatuses[0].lastState}'
  ```
  Grafana `Hearts Worker Performance`: "OOMKilled (last terminated reason)", "Memory working set vs limit (per pod)", "Pod restarts (last 1h)".
- **Fix.** OOMKilled: raise the memory limit (requests == limits) or find the leak. Exit code 1 with a log message: fix the config. Liveness
  failures (`Liveness probe failed` in events): find out why `/healthz` is slow (CPU starvation, deadlock) before loosening the probe. Restore anything you changed during a drill (`kubectl -n hearts set env ... VAR-`).

### HeartsAdvisorSlow

- **Meaning.** The 95th percentile of `POST /api/advise` has been above 3 seconds for 5 minutes. The advisor fans out to every worker pod and
  waits for the slowest, so one slow shard slows every request. The default deadline is `ADVISOR_DEFAULT_DEADLINE_MS=2000`; slow means shards miss it
  and the answers become `partial`. Severity: warning.
- **Likely causes.** Workers busy with a tournament (advisor and tournament share one rayon pool per worker); too many `samples` per request; CPU
  throttling on workers; a noisy neighbour on one node; one worker unhealthy but still in the headless Service DNS; shard errors (timeouts/connect).
- **PromQL.**
  ```
  histogram_quantile(0.95, sum by (le) (rate(hearts_advise_duration_seconds_bucket[5m]))) > 3
  ```
- **Diagnose.**
  ```bash
  scripts/bench/advise-latency.sh -n 50          # p50/p95/p99 now, plus how many answers were partial
  # a single request, using the example body from docs/CONTRACTS.md ("POST /api/advise"):
  #   curl -s -H 'Content-Type: application/json' -d @body.json $URL/api/advise | jq '{partial, shards, shards_failed, latency_ms}'
  kubectl -n hearts get endpoints hearts-worker-headless     # which pod IPs are fanned out to
  ```
  Grafana `Hearts Advisor`: "Advise latency p50 / p95 / p99", "Per-shard latency p50 / p95 / p99" (a high *shard* p99 = a slow pod), "Shard errors/s by reason", "Partial answer rate", "Worker Simulate call p95".
  In `Hearts Worker Performance`: "CPU throttling % (per pod)" and "Busy fraction (per pod)".
- **Fix.** Pause long tournaments while using the advisor, or reduce `samples`; remove CPU throttling (see HeartsCPUThrottlingHigh); delete the
  misbehaving pod (`kubectl -n hearts delete pod <pod>`); check for an experiment pod left running (`kubectl -n hearts get pods`, `noisy-neighbor`).

## Glossary

| Term | Meaning here |
|---|---|
| **Namespace** | A named partition of the cluster. Everything of ours is in `hearts`; monitoring is in `monitoring`. |
| **Pod** | One or more containers sharing network and storage. The unit that is scheduled onto a node. |
| **Deployment / ReplicaSet** | A Deployment keeps N identical pods running by managing ReplicaSets (one per pod template version). A rollout is a new ReplicaSet scaling up while the old one scales down. `rollout undo` points back at an older ReplicaSet. |
| **StatefulSet** | Like a Deployment but pods have stable names (`hearts-db-0`) and their own PVC. Never runs two pods with the same identity. |
| **Strategy: RollingUpdate / Recreate** | RollingUpdate replaces pods gradually (maxSurge/maxUnavailable). Recreate stops all old pods first. The coordinator uses Recreate. |
| **Service** | A stable name and virtual IP in front of pods chosen by a label selector. **Headless** (`clusterIP: None`): no virtual IP, DNS returns every ready pod IP (used for advisor fan-out). |
| **Endpoints** | The list of ready pod IPs behind a Service. `kubectl get endpoints` is a great debugging view. |
| **Ingress / Traefik** | Routes outside HTTP(S) traffic by hostname to a Service. Traefik is the ingress controller in k3s and holds the wildcard certificate. |
| **ConfigMap / Secret** | Key-value config / sensitive key-value config, injected as env vars (`envFrom`). Pods do not restart when they change. |
| **PV / PVC / StorageClass** | PVC = a pod's request for storage; PV = the actual volume; StorageClass = how volumes are made. `local-path` makes a directory on one node's disk, and the PV carries a **node affinity** to that node, so the pod can only run there. |
| **Requests / limits** | Request = what the scheduler reserves. Limit = hard ceiling. CPU over the limit is *throttled*; memory over the limit is *OOM-killed*. |
| **QoS class** | Guaranteed (requests == limits for all containers), Burstable, BestEffort. Decides who is evicted first under node pressure. Workers are Guaranteed. |
| **CFS throttling** | The kernel scheduler gives a container `limit x 100 ms` of CPU time per 100 ms period; when the threads use it up they are frozen until the next period. Seen as `container_cpu_cfs_throttled_periods_total`. |
| **OOMKilled** | The container exceeded its memory limit and the kernel killed it (exit code 137). |
| **CrashLoopBackOff** | The container keeps exiting; the kubelet waits 10 s, 20 s, 40 s ... up to 5 min between restarts. |
| **ImagePullBackOff / ErrImagePull** | The node cannot pull the image (wrong tag, no auth, registry down); retries with growing delay. |
| **Liveness / readiness probe** | Liveness: "restart me if this fails". Readiness: "remove me from Service endpoints while this fails" (no restart). |
| **Taint / toleration** | A taint repels pods from a node unless they tolerate it. `k8s-cp` has `CriticalAddonsOnly`, so our pods stay off it. A down node gets `node.kubernetes.io/unreachable` and pods are evicted after `tolerationSeconds` (300 by default). |
| **Cordon / drain** | Cordon = mark a node unschedulable. Drain = cordon + evict its pods politely. `uncordon` reverses it. |
| **PodDisruptionBudget (PDB)** | Limits how many pods of a set may be down during *voluntary* disruptions (drains, not crashes). Ours: workers `maxUnavailable: 1`. |
| **Topology spread constraint** | A scheduling hint to spread pods across nodes (`maxSkew 1`, `ScheduleAnyway` = preference only). |
| **Downward API** | Injects pod metadata into env vars (`POD_NAME`, `NODE_NAME`). |
| **Kustomize / overlay** | Template-free manifest composition. `deploy/base` is the app; `deploy/overlays/homelab` patches images, secret and knobs. `kubectl apply -k` applies it. |
| **ServiceMonitor / PrometheusRule** | Prometheus Operator CRDs: "scrape these Services" / "evaluate these alert rules". |
| **Lease (application level)** | The coordinator's promise that a unit belongs to a worker until `lease_expires_at` (now + `LEASE_SECONDS`); heartbeats extend it. Not to be confused with Kubernetes' own Lease objects. |
| **Reclaim** | Leasing a unit whose previous lease expired. Counted by `hearts_units_reclaimed_total`; `job.progress.reclaims`. |
| **Wasted work** | A worker computed a unit that was already completed by someone else, or abandoned it after losing its lease (`hearts_worker_wasted_units_total`). |
| **Unit** | One range of hands `[hand_start, hand_end)` of a job; a row in `work_units`. |
