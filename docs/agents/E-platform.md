# Agent E: Platform (containers, CI, Kubernetes, monitoring)

**Owns:** `docker/`, `docker-compose.yml`, `.dockerignore`, `deploy/`, `.github/`, `Makefile`.

## Target environment (facts)
- k3s v1.36, 1 control plane (`k8s-cp`) and 3 workers (`k8s-w1..w3`, 6 cores / 32 GB each), **all amd64**, 1 GbE.
- Traefik is the ingress controller (IngressClass `traefik`). Existing apps use plain HTTP Ingresses (e.g. `hello.k8s.ad.gavinf.com`).
- Wildcard DNS `*.k8s.ad.gavinf.com` resolves on the tailnet. cert-manager exists with ClusterIssuers `letsencrypt-prod/staging`, but **don't use TLS** for now (match the existing HTTP Ingresses; note in the README how to add TLS later).
- Default StorageClass: `local-path`. Argo CD is installed but not used for this project yet.
- No Prometheus yet; we install kube-prometheus-stack.
- Images: `ghcr.io/gavinfancher/hearts-coordinator` and `ghcr.io/gavinfancher/hearts-worker`. The GHCR owner is set in ONE place (Kustomize `images:` in the overlay, plus a Makefile variable).
- The orchestrator does all `kubectl apply`, `helm install`, and `docker push`. **You must not touch the cluster** (no apply, no dry-run=server). Validate offline.

## Build
1. **`docker/Dockerfile`:** multi-stage with `cargo-chef` (planner, then cook deps, then build `--release -p coordinator -p worker`), base `rust:1.99-bookworm` (check the tag exists; otherwise use the closest 1.99 tag) with `protobuf-compiler` installed. Final stages `coordinator` and `worker` on `gcr.io/distroless/cc-debian12:nonroot`, each copying one binary and the coordinator also getting nothing extra (migrations and the web UI are embedded at compile time). `EXPOSE` the right ports and a `USER` of nonroot. Use `--mount=type=cache` for the cargo registry. `.dockerignore` excludes `target/`, `.git`, `docs/`.
2. **`docker-compose.yml`** (local end to end on the Mac): `postgres:18.4` with a healthcheck (`pg_isready`), the coordinator (depends_on healthy, ports 8080/50051), and `worker` scaled to 2 (`deploy.replicas: 2`, `WORKER_THREADS=2`, `COORDINATOR_GRPC_URL=http://coordinator:50051`). The advisor needs DNS fan-out: set `ADVISOR_SHARD_DNS=worker:50052` (compose DNS returns all replica IPs).
3. **`.github/workflows/ci.yml`:**
   - Job `check` on `ubuntu-24.04`: install protoc, `dtolnay/rust-toolchain@stable` with clippy and rustfmt, `Swatinem/rust-cache`, then `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (Docker is available on GH runners for testcontainers).
   - Job `images` (needs check, only on `main`): `docker/setup-buildx-action`, `docker/login-action` to ghcr.io with `GITHUB_TOKEN` (`permissions: packages: write`), `docker/build-push-action` for each target, `platforms: linux/amd64`, tags `sha-${short}` and `main`, GHA layer cache. Note in a comment how to add `linux/arm64` with an `ubuntu-24.04-arm` matrix later (EC2 Graviton).
4. **`deploy/base/`** (Kustomize; namespace `hearts`; standard labels `app.kubernetes.io/name`, `part-of: hearts`):
   - `namespace.yaml`
   - `postgres.yaml`: StatefulSet `hearts-db` (1 replica, image **`postgres:18.4`**, `volumeClaimTemplates` 10Gi `local-path`, `PGDATA` in a subdirectory of the mount, readiness/liveness `pg_isready`, resources requests 250m/512Mi limits 2/2Gi, a `postgres-exporter` sidecar (`quay.io/prometheuscommunity/postgres-exporter`, pinned version) on port 9187), Service `hearts-db` (ClusterIP 5432 + metrics 9187). Plus a Secret generator (`secretGenerator` in the overlay with literals from a git-ignored `deploy/overlays/homelab/db.env`; commit a `db.env.example` and document it).
   - `coordinator.yaml`: Deployment (1 replica, `strategy: Recreate` is fine for now, with a comment explaining why: one replica, schema migrations at startup), env from ConfigMap plus `DATABASE_URL` built from the Secret, ports 8080/50051, readiness `/readyz`, liveness `/healthz`, resources 500m/512Mi requests and 2/1Gi limits. Service `hearts-coordinator` (8080 http, 50051 grpc with `appProtocol: kubernetes.io/h2c`).
   - `worker.yaml`: Deployment `hearts-worker` (3 replicas; **requests == limits**, cpu 2 / memory 1Gi so benchmarks are predictable and QoS is Guaranteed, with a comment explaining that; `topologySpreadConstraints` maxSkew 1 on `kubernetes.io/hostname` with `whenUnsatisfiable: ScheduleAnyway`; `terminationGracePeriodSeconds: 60`; downward API env `POD_NAME`, `NODE_NAME`; `WORKER_THREADS` unset by default so it follows the CPU limit; ports 50052/9090; readiness and liveness on 9090). A headless Service `hearts-worker-headless` (`clusterIP: None`, port 50052, `publishNotReadyAddresses: false`) and a Service `hearts-worker-metrics` (9090) for monitoring. A PodDisruptionBudget `maxUnavailable: 1`.
   - `configmap.yaml`: all tunables from CONTRACTS.md with defaults (`LOG_FORMAT=json`, `ADVISOR_SHARD_DNS=hearts-worker-headless.hearts.svc.cluster.local:50052`, etc.).
   - `ingress.yaml`: `hearts.k8s.ad.gavinf.com` routing to coordinator:8080, ingressClassName traefik.
   - `monitoring.yaml`: ServiceMonitors for the coordinator, worker metrics, and postgres-exporter, labeled `release: kube-prometheus-stack`.
   - `prometheus-rules.yaml`: a PrometheusRule with the alerts below.
   - Every manifest gets a short header comment explaining what it is and the key k8s concepts it uses. The owner is learning k8s.
5. **`deploy/overlays/homelab/`:** `kustomization.yaml` (resources `../../base`, `images:` newName/newTag, secretGenerator, replica and resource patches commented as "the knobs you'll turn in experiments"). **`deploy/overlays/aws/`:** a README stub on what will differ (StorageClass gp3, LB, node groups, multiple coordinators, migrations Job).
6. **`deploy/monitoring/`:**
   - `kube-prometheus-stack-values.yaml`: Grafana Ingress `grafana.k8s.ad.gavinf.com` (traefik, HTTP), admin password from a Secret (document creating it), the sidecar picking up dashboards labeled `grafana_dashboard: "1"` in all namespaces, Prometheus retention 15d with a 20Gi `local-path` PVC, `serviceMonitorSelectorNilUsesHelmValues: false` and `ruleSelectorNilUsesHelmValues: false` so our monitors and rules are picked up, and k3s-specific fixes (disable `kubeEtcd`, `kubeControllerManager`, `kubeScheduler`, `kubeProxy` scraping, which k3s doesn't expose by default; add a comment).
   - `dashboards/`: Grafana dashboard JSON wrapped in ConfigMaps (kustomization with a `configMapGenerator`, label `grafana_dashboard: "1"`, namespace `monitoring`):
     - **Hearts Overview:** cluster hands/s (`sum(rate(hearts_hands_completed_total[1m]))`), units by state, lease/reclaim/duplicate/failed rates, active workers, DB query p95 by query, gRPC p95 by method.
     - **Worker Performance:** per-pod CPU cores used vs limit, **CPU throttling %** (`rate(container_cpu_cfs_throttled_periods_total[1m]) / rate(container_cpu_cfs_periods_total[1m])`), memory working set vs limit, per-pod and per-node hands/s (join on `kube_pod_info` for the node), unit duration p50/p95, busy fraction, wasted units, pod restarts, and the OOMKilled reason.
     - **Advisor:** latency p50/p95/p99, per-shard latency, shard errors by reason, partial answer rate.
     - **Postgres:** connections, transactions/s, locks, rows by state, DB size.
     - Use datasource variable `${datasource}`, a `namespace` variable, and 10s refresh.
7. **Alerts** (`PrometheusRule`, each with `summary`, `description`, and `runbook_url: https://github.com/gavinfancher/rusting/blob/main/docs/RUNBOOK.md#<anchor>`):
   - `HeartsCoordinatorDown`: the coordinator target is down for 1m
   - `HeartsWorkersDown`: `hearts_workers_active == 0` for 2m while units are pending
   - `HeartsJobStalled`: pending + leased > 0 and `rate(hearts_units_completed_total[5m]) == 0` for 5m
   - `HeartsHighReclaimRate`: reclaims are > 10% of leases over 10m
   - `HeartsUnitsFailed`: `increase(hearts_units_failed_total[10m]) > 0`
   - `HeartsCPUThrottlingHigh`: worker throttling > 25% for 5m
   - `HeartsPostgresDown`: `pg_up == 0` for 1m
   - `HeartsPodCrashLooping`: kube-state-metrics restarts in the hearts namespace
   - `HeartsAdvisorSlow`: p95 > 3s for 5m
8. **`Makefile`** (with `make help` self-documenting targets): `dev` (compose up --build), `dev-down`, `test`, `image` (buildx `--platform linux/amd64 --load`, both targets), `push`, `deploy` (`kubectl apply -k deploy/overlays/homelab`), `undeploy`, `monitoring` (helm repo add, then `helm upgrade --install kube-prometheus-stack ... -n monitoring --create-namespace -f values`, then apply the dashboards), `status` (`get pods -o wide`, top), `logs-coordinator`, `logs-worker`, `psql` (`kubectl exec -it hearts-db-0 -- psql -U hearts`), `scale-workers N=5`, `port-forward-grafana`. A `TAG ?= $(shell git rev-parse --short HEAD)` variable.
9. **`deploy/README.md`:** a first-time setup guide covering prereqs, the secrets to create, `make monitoring`, `make deploy`, how to check it's healthy, and how to make the GHCR packages public or add an `imagePullSecret`.

## Validation (offline only)
```
kubectl kustomize deploy/overlays/homelab > /tmp/hearts-rendered.yaml   # must succeed
kubectl kustomize deploy/monitoring/dashboards > /dev/null
# if available (brew install kubeconform): kubeconform -summary -ignore-missing-schemas /tmp/hearts-rendered.yaml
docker compose config -q
docker buildx build --platform linux/amd64 -f docker/Dockerfile --target worker .   # must at least get through dependency cooking; the full build may fail on todo!() only at runtime, not at compile time, so it should succeed
python3 -c "import json,glob; [json.load(open(f)) for f in glob.glob('deploy/monitoring/dashboards/*.json')]"
```
Report the image sizes from `docker images`.
