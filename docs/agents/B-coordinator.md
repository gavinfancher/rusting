# Agent B: Coordinator and data layer

**Owns:** `crates/coordinator/` and new migration files `migrations/0002_*.sql` and later (only if truly needed; never edit `0001`).

## Goal
The control plane: owns all state in Postgres, hands out leases over gRPC, serves the REST API, SSE, and the UI, fans out advisor requests, and exposes metrics. It must survive restarts with zero lost work, and must be correct with **multiple replicas** even though we run one (all coordination goes through Postgres row locks, never in-memory state).

## Build
- **Stack:** `tokio`, `axum` (HTTP), `tonic` (gRPC, using `hearts_proto::pb::worker_service_server`), `sqlx` (postgres, runtime-tokio, migrate, chrono or time), `clap` (derive + env), `tracing` (JSON or pretty per `LOG_FORMAT`), metrics via `prometheus` or `metrics` + `metrics-exporter-prometheus` (your choice; names must match CONTRACTS.md), `rust-embed` for `web/`, `tokio-stream`/axum SSE.
- **Layout suggestion:** `main.rs` (config, startup, shutdown), `config.rs`, `db.rs` (pool, migrations, and **all SQL** in one repository module with doc comments explaining each query, especially the lease query), `grpc.rs`, `http/` (`jobs.rs`, `advise.rs`, `workers.rs`, `benchmarks.rs`, `health.rs`, `static_files.rs`), `advisor_fanout.rs`, `metrics.rs`.
- **Startup:** connect to the DB with retry and backoff (Postgres may start after us in k8s), run `sqlx::migrate!("../../migrations")`, then start HTTP and gRPC on their addresses concurrently. Graceful shutdown on SIGTERM/SIGINT for both servers.
- **Queries:** use runtime-checked `sqlx::query`/`query_as` (not the `query!` macros), so builds don't need a live DB or offline data.
- **Lease semantics:** exactly as in CONTRACTS.md "Work unit lifecycle". Map the core types with `hearts_proto` conversions; build `pb::JobSpec` from the job's `spec` JSON (deserialize to `hearts_core::JobSpec`).
- **Background task** every `STATS_POLL_SECONDS`: refresh the `hearts_units{state}` gauges, `hearts_workers_active`, and the pool gauges; sweep expired leases with `attempts >= MAX_ATTEMPTS` to `failed`, finishing jobs that are now complete.
- **REST:** every endpoint in CONTRACTS.md, with input validation returning 400 plus `{"error"}`. Job creation inserts the job and all its units in one transaction using a set-based insert (`generate_series`), not N round trips, since a 1e9-hand job with small units could be millions of rows. Cap `num_hands / unit_size` at 1,000,000 units.
- **SSE:** `/api/jobs/:id/events` polls the DB every 1s per subscriber. That's fine at this scale; note it as an optimization candidate in a comment.
- **Advisor fan-out:** resolve `ADVISOR_SHARD_DNS` with `tokio::net::lookup_host` on every request (pods come and go), connect to each `http://ip:port` (cache channels by address in a `DashMap` or a `Mutex<HashMap>` and drop failed ones), split samples, call `Simulate` concurrently with `tokio::time::timeout(deadline + 500ms)`, merge `MoveTotals`, and build the response per CONTRACTS.md. View validation uses `hearts_core::rules::validate_view`. That's stubbed with `todo!()` until Agent A merges, so wrap it in `std::panic::catch_unwind` **only in tests**, or keep tests of advise to fan-out/merge logic with a mock shard. Don't let unimplemented engine code block you.
- **Static UI:** serve the embedded `web/` directory (create a placeholder `web/index.html` only if `web/` is empty in your worktree, and don't commit changes to other `web/` files; Agent D owns them). Fallback content types by extension.

## Tests
Integration tests in `crates/coordinator/tests/` using `testcontainers` (+ `testcontainers-modules` postgres) with **image `postgres` tag `18.4`**. Docker is available.
- Create a job: the unit count and boundaries are correct (including a last partial unit).
- Lease contention: 8 concurrent tasks leasing from 100 units, so every unit is leased exactly once.
- Expiry: lease with a 1s lease, wait, re-lease from another worker; attempts == 2 and the reclaim is counted.
- Idempotent completion: complete twice, so the second returns `accepted=false` and the standings count once.
- Job completion: the last unit completed flips the job to `done` with `finished_at`; an error at max attempts gives `failed`.
- Cancel: pending units are cancelled and later LeaseWork returns nothing.
- gRPC end to end: start the tonic server in-process on a random port and drive it with the generated client.
- HTTP: `POST /api/jobs` then `GET /api/jobs/:id` JSON shape (use `tower::ServiceExt::oneshot` on the router).
- Advisor merge with two mock `AdvisorShard` servers, one of which sleeps past the deadline, so `partial=true` and `shards_failed=1`.

## Acceptance
```
cargo test -p coordinator
docker run -d --name hearts-pg -e POSTGRES_USER=hearts -e POSTGRES_PASSWORD=hearts -e POSTGRES_DB=hearts -p 5432:5432 postgres:18.4
DATABASE_URL=postgres://hearts:hearts@localhost:5432/hearts LOG_FORMAT=pretty cargo run -p coordinator &
curl -s localhost:8080/readyz; curl -s -XPOST localhost:8080/api/jobs -H 'content-type: application/json' \
  -d '{"strategies":["heuristic","heuristic","random","random"],"num_hands":10000,"unit_size":1000}'
curl -s localhost:8080/metrics | grep hearts_
```
Clean up the container afterwards.
