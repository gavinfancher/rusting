# Deploying Hearts to Kubernetes

Layout:

| Path | What |
|---|---|
| `base/` | Kustomize base: namespace, Postgres, coordinator, workers, ingress, ServiceMonitors, alert rules. Every file has a header explaining the k8s concepts it uses. |
| `overlays/homelab/` | Your k3s cluster: image tags, DB secret, scaling knobs. |
| `overlays/aws/` | Stub describing what will differ on AWS. |
| `monitoring/` | Helm values for kube-prometheus-stack and Grafana dashboards. |

## Prerequisites

- `kubectl` pointed at the k3s cluster, plus `helm`.
- Images pushed to GHCR (CI does this on `main`, or `make image push TAG=...` locally after `docker login ghcr.io`).
- Wildcard DNS `*.k8s.ad.gavinf.com` reachable (it resolves on the tailnet).

## First-time setup

1. **DB secret (not committed).**
   ```
   cp deploy/overlays/homelab/db.env.example deploy/overlays/homelab/db.env
   $EDITOR deploy/overlays/homelab/db.env      # set POSTGRES_PASSWORD
   ```
   Kustomize turns it into Secret `hearts-db-<hash>`. Changing the file rolls the pods that use it.
   Note: Postgres only reads the password on first initialisation of its volume; changing it later
   means `ALTER USER` or deleting the PVC `data-hearts-db-0`.

2. **Grafana admin Secret.**
   ```
   kubectl create namespace monitoring
   kubectl -n monitoring create secret generic grafana-admin \
     --from-literal=admin-user=admin --from-literal=admin-password='<choose one>'
   ```

3. **Monitoring stack** (do this before the app: it installs the ServiceMonitor/PrometheusRule CRDs):
   ```
   make monitoring
   ```
   Grafana: http://grafana.k8s.ad.gavinf.com (or `make port-forward-grafana`).

4. **Image pull access.** Either make the two GHCR packages public (GitHub, your profile, Packages,
   `hearts-coordinator` and `hearts-worker`, Package settings, Change visibility), or keep them private and create a pull secret:
   ```
   kubectl -n hearts create secret docker-registry ghcr-pull \
     --docker-server=ghcr.io --docker-username=<github-user> --docker-password=<PAT with read:packages>
   ```
   and add `imagePullSecrets: [{name: ghcr-pull}]` to the coordinator and worker pod specs
   (e.g. via a patch in the overlay). Create the namespace first (`kubectl create ns hearts`) if you do this before `make deploy`.

5. **Deploy.**
   ```
   make deploy
   ```

## Is it healthy?

```
make status                       # pods Running/Ready, spread over k8s-w1..w3
kubectl -n hearts get ingress,svc,pvc
curl http://hearts.k8s.ad.gavinf.com/healthz
curl http://hearts.k8s.ad.gavinf.com/api/workers   # should list 3 active workers
make logs-coordinator
```
In Grafana, open "Hearts Overview": active workers should be 3. In Prometheus (Status, Targets) the
`hearts-coordinator`, `hearts-worker` and `hearts-db` targets should be UP.

## Day-to-day

- Roll out a new build: edit `newTag` in `overlays/homelab/kustomization.yaml`, then `make deploy`.
- Resize / rescale experiments: `make scale-workers N=6`, or the commented patches in the overlay.
- Database shell: `make psql`.
- Tear down: `make undeploy` (the Postgres PVC is retained; `kubectl -n hearts delete pvc data-hearts-db-0` to wipe).

## Adding TLS later

The Ingress is plain HTTP like the other apps. With cert-manager already installed, add to
`base/ingress.yaml` (and the Grafana ingress in the Helm values):
```yaml
metadata:
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt-prod
spec:
  tls:
    - hosts: [hearts.k8s.ad.gavinf.com]
      secretName: hearts-tls
```
Note Let's Encrypt HTTP-01 needs the host reachable from the internet; on a tailnet-only name use a DNS-01 issuer.

## Notes

- The coordinator runs migrations at startup, so it uses `strategy: Recreate` with a single replica.
- Workers have requests == limits (Guaranteed QoS) so benchmark numbers are repeatable.
- Alert runbook links point to `docs/RUNBOOK.md`, which another workstream is expected to write.
