# rusting: a distributed Hearts simulator

A Rust system that runs Monte Carlo simulations of the card game Hearts across a Kubernetes cluster. It exists to explore **vertical scaling** (threads per pod), **horizontal scaling** (pods per cluster), and **failure handling** (kill things and watch them recover) on real CPU-heavy work.

- **Tournament mode:** pit bot strategies against each other over millions of hands. Batch throughput.
- **Advisor mode:** "which card should I play?" answered by fanning simulations out to every worker. Latency-sensitive.

```
UI/API ──► coordinator ──► Postgres (jobs, leases, results)
               ▲   │ gRPC
     LeaseWork │   ▼ AdvisorShard fan-out
           worker pods (rayon thread pools)
```

| Path | What |
|---|---|
| `crates/hearts-core` | Pure game engine, bots, tournament runner, PIMC advisor |
| `crates/hearts-proto` | gRPC types (`proto/hearts.proto`) and conversions |
| `crates/coordinator` | Control plane: Postgres, gRPC leases, REST/SSE API, UI |
| `crates/worker` | Compute node: lease loop, rayon pool, advisor shard |
| `crates/hearts-cli` | Run everything locally without a cluster |
| `web/` | Static operator UI |
| `deploy/` | Kustomize manifests, monitoring |
| `docs/` | Contracts, architecture, runbook, game days, experiments |

Start with [`docs/CONTRACTS.md`](docs/CONTRACTS.md).
