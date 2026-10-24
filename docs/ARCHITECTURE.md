# Architecture

## The 2-minute pitch
> I built a distributed Monte Carlo simulator for the card game Hearts in Rust and run it on my own 4-node k3s cluster. It does two kinds of work. **Tournaments** are batch jobs: millions of simulated hands that pit bot strategies against each other, split into work units that worker pods pull from a Postgres-backed queue. **The advisor** is interactive: given your hand, it fans thousands of simulations out to every worker in parallel and tells you which card minimizes your expected points.
>
> Its purpose is to study scaling and failure. Each worker runs a thread pool sized to its CPU limit (vertical scaling), and I add pods to scale horizontally. Every hand is seeded deterministically, so a job that survives a chaos drill (killed pods, a drained node, a dead database) must produce *exactly* the same standings as a clean run. That gives me an objective pass/fail test for resilience. Prometheus and Grafana show throughput, throttling, lease reclaims, and tail latency while I break things.

## Components
```
                    hearts.k8s.ad.gavinf.com (Traefik)
                                  │
                         ┌────────▼────────┐  HTTP :8080  UI, REST, SSE, /metrics
                         │   coordinator   │
                         │  (control plane)│  gRPC :50051 WorkerService
                         └──┬───────────▲──┘
            SQL (sqlx pool) │           │ LeaseWork / Heartbeat / CompleteWork
                  ┌─────────▼──┐        │   (workers PULL)
                  │  Postgres  │        │
                  │ StatefulSet│   ┌────┴──────────────── N worker pods ─────┐
                  │  + PVC     │   │ lease loop ─► rayon pool (T threads)    │
                  └────────────┘   │ AdvisorShard gRPC :50052  /metrics :9090│
                                   └─────────────────────────────────────────┘
                     ▲ advisor fan-out: coordinator ──► every worker pod IP
                       (found through the headless Service's DNS)
```

| Component | Responsibility | State |
|---|---|---|
| **hearts-core** | Rules, bots, tournament runner, PIMC advisor. Pure CPU, no I/O. | none |
| **coordinator** | Splits jobs into units, leases them, records results, serves the API and UI, fans out advisor calls | **none in memory**; everything lives in Postgres |
| **worker** | Pulls units, simulates them on a rayon pool, heartbeats, reports; serves advisor shards | only the unit in flight |
| **Postgres** | Jobs, work units (the queue), results, worker registry, benchmark history | durable (PVC) |
| **Prometheus/Grafana** | Metrics, dashboards, alerts | 15 days of metrics |

## Life of a tournament job
1. `POST /api/jobs` with 4 strategies, `num_hands`, `unit_size`, and `seed`. In one transaction the coordinator inserts the job and `ceil(num_hands / unit_size)` rows in `work_units`, each a hand range `[start, end)`.
2. Each worker loops on `LeaseWork`. The coordinator runs one atomic SQL statement that picks the next pending unit (or a leased unit whose lease has **expired**), marks it leased to that worker for 30s, and increments `attempts`. `FOR UPDATE SKIP LOCKED` means concurrent lease calls never get the same row and never wait on each other.
3. The worker runs `run_hands(spec, start..end)` on its rayon pool. Every 10s a heartbeat extends the lease. If the coordinator says the lease was lost, the worker stops between chunks.
4. `CompleteWork` stores per-strategy stats, and only the first completion counts (`WHERE state <> 'done'`). When no units are left, the job flips to `done`, all in the same transaction.
5. The UI streams progress over SSE; standings are a `SUM` over `unit_results`.

**What happens when things die:**
| Failure | What happens | Why it's safe |
|---|---|---|
| Worker pod killed | Its lease expires after ≤30s and the next `LeaseWork` from any worker reclaims the unit | Deterministic seeds mean the redo is identical |
| Worker slow or partitioned, then finishes late | Its completion is rejected (`accepted=false`) and counted as wasted work | Idempotent completion |
| Coordinator killed | Workers retry with exponential backoff; k8s restarts the pod; work continues | No in-memory state |
| Postgres killed | The coordinator goes not-ready (readiness probe); workers back off; data survives on the PVC | The DB is the single source of truth |
| Unit that always crashes | Each panic is caught and reported; after `MAX_ATTEMPTS` the unit is `failed` and an alert fires | Poison-pill protection |

## Life of an advisor request
1. `POST /api/advise` with your hand, the current trick, the played cards, and known voids.
2. The coordinator resolves `hearts-worker-headless`, which returns **one DNS record per ready worker pod**, splits the samples evenly, and calls every shard concurrently with a deadline.
3. Each shard runs PIMC: deal the unseen cards randomly (respecting what's known), play out the hand for every legal move with heuristic bots, and total your points.
4. The coordinator sums the totals and recommends the lowest average. If a shard times out, it returns a **partial** answer rather than an error.

The two modes stress the system differently. Tournaments measure **throughput** (hands/sec); the advisor measures **tail latency** (p99), and one slow pod drags down every request.

## Why these designs
- **Pull, not push.** Workers ask for work when they're free, so fast pods naturally take more units, new pods join simply by starting, and the coordinator never tracks who's alive in order to assign work.
- **Leases with expiry instead of failure detection.** "Is that worker dead?" is hard to answer in distributed systems. "Has it renewed its lease in the last 30s?" is easy. Expiry is the failure detector.
- **Postgres as the queue.** One less system to operate (no Kafka or NATS), transactional result writes, and `SKIP LOCKED` scales to thousands of leases/sec. The cost: at tiny unit sizes the DB becomes the bottleneck. Experiment E3 measures exactly that.
- **Determinism.** Seeds come from `(job_seed, hand_index)`, results are merged with commutative sums, and the advisor's randomness is owned by fixed-size sample chunks, not threads. Results therefore don't depend on thread count, pod count, or retries, which makes resilience testable.
- **requests == limits for workers.** Guaranteed QoS and no noisy-neighbor CPU borrowing, so benchmark numbers are repeatable. The thread pool defaults to the CPU limit to avoid CFS throttling (experiment E4 shows what happens when you get this wrong).
- **Rust.** No GC pauses (good for p99), small static binaries in distroless images, about 1 ms startup, and bitmask card sets that make a simulated hand cost microseconds.

## Trade-offs and known limits
- One coordinator replica for now. The design is multi-replica safe (all coordination is row locks), but migrations run at startup, which should move to a Job when replicas > 1.
- Postgres on a `local-path` PVC is pinned to one node. If that node dies, the DB is down until it returns (game day 4). Real HA would mean replicated storage (Longhorn) or an operator with replicas (CloudNativePG).
- SSE progress polls the DB per subscriber, which is fine for a few viewers.
- Advisor shards share CPU with tournament work on the same pods. That's intentional and makes for an interesting contention experiment.

## Where to look in the code
| Question | File |
|---|---|
| How are cards represented? | `crates/hearts-core/src/card.rs` |
| How is determinism guaranteed? | `crates/hearts-core/src/seed.rs`, `types.rs` (`seating`) |
| The lease query | `crates/coordinator/src/db.rs` |
| Worker loop, heartbeats, shutdown | `crates/worker/src/` |
| Wire format | `proto/hearts.proto` |
| All interfaces in one place | `docs/CONTRACTS.md` |
