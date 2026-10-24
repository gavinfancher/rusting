# Contracts

These are the interfaces shared between components. Every agent builds against them. **Do not change a contract on your own.** If you need a change, write a proposal under "Proposed changes" at the bottom and note it in your final report.

Source-of-truth files:
| Contract | File |
|---|---|
| Core types (Card, CardSet, JobSpec, UnitStats, PlayerView, MoveTotals, seeding, seating) | `crates/hearts-core/src/{card,types,seed}.rs` |
| Engine entry points | `hearts_core::tournament::run_hands`, `hearts_core::advisor::simulate`, `hearts_core::rules::{legal_moves, validate_view}` |
| gRPC | `proto/hearts.proto` (wire ↔ core conversions in `crates/hearts-proto`) |
| Database | `migrations/0001_init.sql` |
| HTTP API, env vars, ports, metrics | this file |

## Ports
| Component | Port | Purpose |
|---|---|---|
| coordinator | 8080 | HTTP: UI (`/`), REST (`/api/*`), SSE, `/metrics`, `/healthz`, `/readyz` |
| coordinator | 50051 | gRPC `WorkerService` |
| worker | 50052 | gRPC `AdvisorShard` |
| worker | 9090 | HTTP: `/metrics`, `/healthz`, `/readyz` |

## Environment variables
All binaries use `clap` with `env` support, so each flag can also be set as an env var. Defaults are shown.

**Shared**
- `RUST_LOG=info`: tracing filter
- `LOG_FORMAT=json`: `json` (k8s) or `pretty` (local)

**Coordinator**
- `DATABASE_URL` (required), e.g. `postgres://hearts:hearts@localhost:5432/hearts`
- `DB_POOL_SIZE=16`
- `HTTP_ADDR=0.0.0.0:8080`, `GRPC_ADDR=0.0.0.0:50051`
- `LEASE_SECONDS=30`: lease length; each heartbeat extends it to now + LEASE_SECONDS
- `MAX_ATTEMPTS=3`: a unit that fails or expires this many times becomes `failed`
- `NO_WORK_RETRY_MS=1000`: the `retry_after_ms` sent when there's no work
- `ADVISOR_SHARD_DNS=`, e.g. `hearts-worker-headless.hearts.svc.cluster.local:50052`. Resolve all A records and fan out to each. Empty means the advisor is disabled (HTTP 503).
- `ADVISOR_DEFAULT_DEADLINE_MS=2000`
- `STATS_POLL_SECONDS=5`: how often the `hearts_units{state}` gauges are refreshed and expired leases at MAX_ATTEMPTS are swept to `failed`

**Worker**
- `COORDINATOR_GRPC_URL=http://localhost:50051`
- `WORKER_THREADS=0`: size of the rayon pool; 0 means `std::thread::available_parallelism()`, which respects the cgroup CPU limit in k8s
- `MAX_UNITS_PER_LEASE=1`
- `HEARTBEAT_SECONDS=10`
- `SHUTDOWN_GRACE_SECONDS=45`: on SIGTERM, how long to let the in-flight unit finish before abandoning it (keep it below the pod's `terminationGracePeriodSeconds: 60`)
- `PREFETCH=false`: lease the next unit while computing the current one
- `GRPC_ADDR=0.0.0.0:50052`, `METRICS_ADDR=0.0.0.0:9090`
- `POD_NAME` / `NODE_NAME`: from the k8s downward API; default to the hostname and `"local"`. `worker_id = POD_NAME`.
- `FAULT_PANIC_EVERY_N=0`: if > 0, panic on every Nth unit (game day: poison unit)
- `FAULT_SLOW_MS=0`: extra sleep per unit (game day: lease too short)

## Work unit lifecycle
```
pending ──LeaseWork──► leased ──CompleteWork(stats)──► done
   ▲                     │  ├─CompleteWork(error)──► pending (attempts < MAX) / failed
   └──lease expired──────┘  └─job cancelled────────► cancelled
         (reclaimed on the next LeaseWork; attempts >= MAX ⇒ failed by the sweeper)
```
- **Lease** (one statement, safe with many coordinators):
  ```sql
  UPDATE work_units SET state='leased', lease_owner=$w, lease_expires_at=now()+$lease, attempts=attempts+1
  WHERE id IN (SELECT id FROM work_units
               WHERE (state='pending' OR (state='leased' AND lease_expires_at < now()))
                 AND attempts < $max_attempts
               ORDER BY job_id, id
               FOR UPDATE SKIP LOCKED LIMIT $n)
  RETURNING ...
  ```
  Count `hearts_units_reclaimed_total` for rows that were `leased` (expired) before this update. Set the job to `running`/`started_at` if it was `pending`.
- **Heartbeat:** extend `lease_expires_at` for units where `lease_owner = worker AND state = 'leased'`. The rest are reported in `lost_unit_ids`.
- **Complete (stats):** in one transaction: `UPDATE … SET state='done' … WHERE id=$1 AND state NOT IN ('done','cancelled')`. If 0 rows, return `accepted=false`. Otherwise insert 4 `unit_results` rows; if the job has no `pending`/`leased` units left, set the job status (`done` or `failed`) and `finished_at`, and return `job_finished`.
- **Complete (error):** set `last_error`; `state = 'pending'` if `attempts < MAX_ATTEMPTS`, else `'failed'`.

## HTTP API (coordinator :8080)
All JSON. Errors are `{"error": "message"}` with a 4xx/5xx status. Times are RFC 3339 strings. Cards and strategies use the notation above.

### `POST /api/jobs` creates a tournament
Request:
```json
{ "name": "heuristic vs random", "strategies": ["heuristic","heuristic","random","pimc:200"],
  "num_hands": 100000, "seed": 42, "unit_size": 2000, "rules": {"no_points_first_trick": true} }
```
`name`, `seed` (default: random), `unit_size` (default 2000), and `rules` are optional. Limits: `num_hands` 1..=1e9, `unit_size` 1..=1e6, `seed` < 2^53 (JSON-safe). Units are `[k*unit_size, min((k+1)*unit_size, num_hands))`.
Response `201`: a **Job** object.

### `GET /api/jobs?limit=50` lists jobs, newest first
Returns an array of **Job** objects.

### `GET /api/jobs/:id` returns one **Job**

**Job object:**
```json
{
  "id": 1, "name": "heuristic vs random", "status": "running",
  "spec": {"seed": 42, "strategies": ["heuristic","heuristic","random","pimc:200"], "rules": {"no_points_first_trick": true}},
  "num_hands": 100000, "unit_size": 2000,
  "created_at": "...", "started_at": "...", "finished_at": null,
  "progress": {"units_total": 50, "pending": 10, "leased": 6, "done": 34, "failed": 0, "cancelled": 0,
               "hands_done": 68000, "reclaims": 2},
  "standings": [
    {"slot": 0, "strategy": "heuristic", "hands": 68000, "points": 401234, "avg_points": 5.9,
     "moons": 12, "queens": 15000, "wins": 30000, "win_pct": 44.1}
  ],
  "elapsed_ms": 12345, "hands_per_sec": 5510.2
}
```
- `standings` always has 4 entries (zeros before any unit completes), summed from `unit_results`.
- `reclaims` = `SUM(GREATEST(attempts - 1, 0))`.
- `elapsed_ms` runs from `started_at` to `finished_at` (or now). `hands_per_sec = hands_done / elapsed`; it is `null` before the job starts.

### `POST /api/jobs/:id/cancel`
Marks pending and leased units `cancelled` and the job `cancelled`. Returns the Job.

### `GET /api/jobs/:id/events` (Server-Sent Events)
Emits `event: progress` with `data: <Job JSON>` every 1s. When the job reaches a terminal status it sends one `event: done` with the final Job and closes the stream.

### `POST /api/advise`
Request:
```json
{ "seat": 0, "hand": ["3C","7C","JC","4D","6D","9D","QS","5S","TS","3H","8H","KH","AH"],
  "trick_leader": 3, "trick": ["2C"], "played": [],
  "voids": [[], [], [], []], "hearts_broken": false, "tricks_played": 0,
  "points": [0,0,0,0], "passed_to": 1, "passed_cards": ["AS","KS","9H"],
  "rules": {"no_points_first_trick": true}, "samples": 2000, "deadline_ms": 2000, "seed": 7 }
```
Defaults: `seat` 0, `trick_leader` = seat, `trick`/`played`/`passed_cards` empty, `voids` all empty, `points` zeros, `hearts_broken` false, `tricks_played` = `played.len()/4`, `passed_to` null, `samples` 1000 (max 1,000,000), `deadline_ms` = `ADVISOR_DEFAULT_DEADLINE_MS`, `seed` random. The view must be consistent: `hand.len() == 13 - tricks_played`, `played.len() == 4 * tricks_played`, and the first trick is led with 2C. The coordinator validates the view with `hearts_core::rules::validate_view` (400 on error), splits `samples` evenly across shards (shard `s` gets seed `child_seed(seed, s)`), merges, and responds:
```json
{ "best": "2C",
  "moves": [{"card": "2C", "avg_points": 1.12, "samples": 2000}, {"card": "QS", "avg_points": 9.8, "samples": 2000}],
  "samples_requested": 2000, "samples_achieved": 2000, "partial": false,
  "shards": 3, "shards_failed": 0, "latency_ms": 143 }
```
`partial` is true when `samples_achieved < samples_requested` (deadline hit or a shard failed). It returns 503 if no shard answered.

### `GET /api/workers`
```json
[{"worker_id": "hearts-worker-7d9f-abc", "pod_name": "...", "node_name": "k8s-w1", "threads": 2,
  "first_seen": "...", "last_seen": "...", "active": true, "leased_units": 1}]
```
`active` means `last_seen` is within 3 × HEARTBEAT (30s). Inactive workers seen within the last hour are included.

### `GET /api/benchmarks?experiment=E2-horizontal` and `POST /api/benchmarks`
POST body: the `benchmark_runs` columns except `id` and `created_at`. If `job_id` is given and `hands`, `wall_ms`, or `hands_per_sec` are omitted, the coordinator fills them from the job. Returns the row. GET returns rows newest first.

### Health
- `GET /healthz`: 200 if the process is up (liveness)
- `GET /readyz`: 200 if a DB `SELECT 1` succeeds within 1s, else 503 (readiness)

### UI
`GET /` and static assets come from `web/` (embedded in the binary with `rust-embed`). `index.html`, `tournament.html`, and `advisor.html` are served at `/`, `/tournament.html`, and `/advisor.html`.

## Metrics (Prometheus, at `/metrics`)
Use **no high-cardinality labels** (never a job ID, unit ID, or worker ID; the pod label comes from Prometheus target labels). Units are seconds. Histograms use buckets suited to their range.

**Coordinator**
| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `hearts_units_leased_total` | counter | | units handed out |
| `hearts_units_reclaimed_total` | counter | | of those, units whose previous lease had expired |
| `hearts_units_completed_total` | counter | | accepted completions |
| `hearts_units_duplicate_total` | counter | | completions rejected because the unit was already done |
| `hearts_units_errored_total` | counter | | CompleteWork with an error |
| `hearts_units_failed_total` | counter | | units moved to `failed` |
| `hearts_units` | gauge | `state` | current units per state (polled) |
| `hearts_hands_completed_total` | counter | | hands in accepted completions |
| `hearts_jobs_created_total` / `hearts_jobs_finished_total` | counter | `status` on finished | |
| `hearts_grpc_request_duration_seconds` | histogram | `method` | WorkerService handler time |
| `hearts_db_query_duration_seconds` | histogram | `query` (lease, heartbeat, complete, create_job, stats, ...) | |
| `hearts_db_pool_connections` | gauge | `state` (idle, in_use) | |
| `hearts_workers_active` | gauge | | workers seen within 30s |
| `hearts_advise_duration_seconds` | histogram | | end-to-end `/api/advise` latency |
| `hearts_advise_shard_duration_seconds` | histogram | | per-shard call latency |
| `hearts_advise_shard_errors_total` | counter | `reason` (timeout, error, connect) | |
| `hearts_advise_partial_total` | counter | | partial answers |
| `hearts_http_requests_total` | counter | `route`, `status` | route = the matched pattern, e.g. `/api/jobs/:id` |

**Worker**
| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `hearts_worker_threads` | gauge | | rayon pool size |
| `hearts_worker_busy` | gauge | | 1 while computing a unit |
| `hearts_worker_hands_simulated_total` | counter | | hands simulated (including wasted work) |
| `hearts_worker_units_completed_total` | counter | `result` (accepted, duplicate, error) | |
| `hearts_worker_wasted_units_total` | counter | | finished but `accepted=false`, or abandoned after a lost lease |
| `hearts_worker_unit_duration_seconds` | histogram | | compute time per unit |
| `hearts_worker_lease_errors_total` | counter | `rpc` | gRPC failures talking to the coordinator |
| `hearts_worker_idle_polls_total` | counter | | LeaseWork returned no units |
| `hearts_worker_advise_samples_total` | counter | | samples run for the advisor |
| `hearts_worker_advise_duration_seconds` | histogram | | per Simulate call |

Both binaries also export the default process metrics (CPU, RSS, open fds) if the chosen library supports it.

## Training (Level 1 learning)
Evolves the weights of the `linear:` strategy (24 integers, meanings in `hearts_core::types::FEATURE_NAMES`) with the cross-entropy method in `hearts_core::learn`. It needs no new gRPC and no worker changes: each candidate is scored by an ordinary tournament job whose slot 0 is `linear:<weights>`. Schema: `migrations/0002_training.sql`.

**Algorithm (the coordinator's trainer task, and `hearts-cli train`, identically):**
```
state = learn::initial_state(init, sigma0)
for g in state.generation .. generations:
    seed_g     = learn::generation_seed(run_seed, g)
    candidates = learn::sample(&state, population, seed_g)          # idx 0 = the mean
    for each candidate: job = learn::candidate_spec(c, opponents, seed_g, rules), num_hands = hands_per_candidate
    fitness_i  = learn::fitness(&job standings as UnitStats)           # slot 0 avg points per hand; failed/cancelled job => INFINITY
    state      = learn::update(&state, &cfg, &[(c_i, fitness_i)])
    champion   = learn::to_weights(&state.mean)
validation: champion vs opponents AND "heuristic" vs opponents, validation_hands each, same seed child_seed(run_seed, 1_000_000)
```
Because every step is deterministic, a cluster run and `hearts-cli train` with the same config and seed produce the **same champion weights**.

**Trainer task (coordinator):** every 2s it takes each `running`/`validating` run, inside a transaction holding `pg_try_advisory_xact_lock(42, run_id)` (so with several coordinator replicas exactly one advances a run; skip if not acquired), and moves it forward:
1. If generation `g = runs.generation` has no candidates yet: sample them; for each, create a tournament job (tagged `training_run_id`, name `train#<run> g<g> c<idx>`) and a `training_candidates` row; insert the `training_generations` row. All in one transaction, so a crash can't leave half a generation.
2. Else, if every candidate job is terminal: fill `fitness`, run `learn::update`, write the generation stats, and update `state`, `generation`, `champion` and `champion_fitness`. If `generation == generations`, create the two validation jobs and set status `validating`.
3. If `validating` and both validation jobs are terminal: status `done`, `finished_at`.

Restarting the coordinator at any point resumes correctly.

### `POST /api/training`
```json
{ "name": "first run", "population": 24, "elites": 6, "generations": 30,
  "hands_per_candidate": 40000, "unit_size": 5000,
  "opponents": ["heuristic","heuristic","heuristic"], "init": "heuristic",
  "sigma0": 20.0, "smoothing": 0.7, "min_sigma": 2.0, "seed": 1, "validation_hands": 200000,
  "rules": {"no_points_first_trick": true} }
```
Every field is optional; the defaults are shown (seed defaults to random). Limits: population 4..=256, elites 1..population, generations 1..=1000, hands_per_candidate 100..=10,000,000. `init` is `heuristic` or `zero`. The stored `config` is this body with the defaults filled in. Returns `201` and a **TrainingRun**.

### `GET /api/training` (newest first, without `generations`) · `GET /api/training/:id` · `POST /api/training/:id/cancel`
Cancel also cancels the run's non-terminal jobs.

**TrainingRun:**
```json
{ "id": 1, "name": "first run", "status": "running", "config": {...}, "seed": 1,
  "generation": 7, "generations_total": 30,
  "current": {"generation": 7, "candidates_total": 24, "candidates_done": 10, "hands_done": 400000, "hands_total": 960000},
  "champion": {"strategy": "linear:12,-3,...", "fitness": 5.91,
               "weights": [{"feature": "play.leading", "weight": 12}, ...]},
  "generations": [{"generation": 0, "seed": 123, "mean_fitness": 6.48, "best_fitness": 6.10, "elite_mean_fitness": 6.2,
                   "population_mean_fitness": 6.9, "sigma_mean": 20.0, "started_at": "...", "finished_at": "...", "elapsed_ms": 4100}],
  "validation": {"hands": 200000, "champion_fitness": 5.88, "baseline_fitness": 6.49,
                 "champion_job_id": 90, "baseline_job_id": 91},
  "created_at": "...", "finished_at": null, "last_error": null }
```
`champion` and `validation` are `null` until they're available. `current` is `null` when no generation is in flight.

### Changes to existing endpoints
- `POST /api/jobs`: a strategy may be `"champion:<run_id>"`. It's resolved at creation time to that run's current champion as `linear:...` (400 if the run has no champion yet). The stored spec contains the resolved weights.
- `GET /api/jobs`: excludes training jobs by default; `?include_training=true` includes them. The Job object gains `"training_run_id": null|N`.

### Metrics (coordinator)
| Metric | Type | Meaning |
|---|---|---|
| `hearts_training_runs_active` | gauge | runs in running/validating |
| `hearts_training_generations_total` | counter | completed generations |
| `hearts_training_generation_duration_seconds` | histogram | from generation start to update |
| `hearts_training_latest_fitness` | gauge | `champion_fitness` of the most recently completed generation (any run) |
| `hearts_training_latest_best_fitness` | gauge | `best_fitness` of that generation |

## Proposed changes
_(none yet)_
