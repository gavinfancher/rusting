# AWS overlay (stub)

Not implemented yet. What will differ from `../homelab`:

- **Storage:** Postgres PVC uses StorageClass `gp3` (EBS) instead of `local-path`; or move to RDS/Aurora and drop `postgres.yaml` (point `DATABASE_URL` at it via a Secret).
- **Ingress:** AWS Load Balancer Controller (ALB/NLB) instead of Traefik; TLS via ACM or cert-manager.
- **Nodes:** separate node groups (e.g. one for the coordinator/DB, a compute-optimized/Graviton group for workers) selected with `nodeSelector`/taints. Needs an arm64 image build (see the comment in `.github/workflows/ci.yml`).
- **Coordinator:** several replicas with `RollingUpdate`, which requires moving schema migrations out of coordinator startup into a one-off `Job` (or init step) so replicas do not race.
- **Images:** pulled from ECR or GHCR with an `imagePullSecret`.
- **Autoscaling:** cluster-autoscaler/Karpenter plus an HPA or KEDA on pending units (`hearts_units{state="pending"}`).
