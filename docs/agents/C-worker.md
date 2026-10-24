# Agent C: Worker

**Owns:** `crates/worker/`.

## Goal
The compute node: pulls work units, burns CPU with a rayon pool, heartbeats its leases, reports results, serves advisor shard requests, and behaves well under Kubernetes (honors CPU limits, shuts down gracefully, survives a coordinator outage, never dies from one bad unit).

## Build
- **Stack:** `tokio`, `tonic` (client `hearts_proto::pb::worker_service_client`, server `advisor_shard_server`), `rayon`, `clap` (derive + env), `tracing`, a metrics exporter (names per CONTRACTS.md), and `axum` for the small `:9090` HTTP server (`/metrics`, `/healthz`, `/readyz` = "connected to the coordinator recently, or no work yet").
- **Thread pool:** build ONE `rayon::ThreadPool` with `WORKER_THREADS` (0 means `available_parallelism()`; log the value and set the `hearts_worker_threads` gauge). Share it via `Arc`. Run compute with `tokio::task::spawn_blocking(move || pool.install(|| run_hands(&spec, range)))`, so the async runtime never blocks. Comment why: async threads must not do long CPU work.
- **Abstraction for testability:** define a trait (e.g. `UnitRunner`/`Engine`) with `run_hands` and `simulate`, and a real impl that calls `hearts_core`. Tests inject a fake (the real engine is `todo!()` until Agent A merges).
- **Lease loop** (own task):
  - LeaseWork(max_units = `MAX_UNITS_PER_LEASE`), then process the units sequentially (each unit already uses all threads), then CompleteWork.
  - On an empty response, sleep `retry_after_ms` and bump `hearts_worker_idle_polls_total`.
  - On gRPC errors, exponential backoff with full jitter (100ms doubling to a 10s cap; reset on success) and bump `hearts_worker_lease_errors_total{rpc}`. The coordinator being down must never crash the worker.
  - With `PREFETCH=true`, lease the next unit while the current one computes. Watch out for lease expiry on the prefetched unit; heartbeat it too.
- **Heartbeat task:** every `HEARTBEAT_SECONDS`, send the ids of in-flight units. If a unit comes back in `lost_unit_ids`, set a cancel flag. `run_hands` can't be interrupted mid-call, so either split the unit into sub-ranges (e.g. 64 chunks, `merge`d; still deterministic per the run_hands contract) and check the flag between chunks, or let it finish and count it as wasted. Chunking is preferred, so document the choice.
- **Panics:** wrap unit execution in `std::panic::catch_unwind` (inside `spawn_blocking`) and report `CompleteWork{error: <panic message>}`. The process keeps running.
- **Faults:** `FAULT_PANIC_EVERY_N` (panic inside the unit closure on every Nth unit) and `FAULT_SLOW_MS` (sleep inside the blocking task before computing), so game days can trigger them. Log loudly at startup when either is set.
- **Shutdown** on SIGTERM: stop leasing new units, let the current unit finish if it's done within `SHUTDOWN_GRACE_SECONDS` (default 45, under k8s's 60s), otherwise stop heartbeating and exit (the lease expires and is reclaimed). Log what happened. Also stop the gRPC server gracefully.
- **AdvisorShard server:** `Simulate` converts the view with `hearts_proto`, then runs `advisor::simulate(view, samples, seed, Some(now + deadline_ms))` on the **same rayon pool** via `spawn_blocking`, and responds. Record the metrics. Note in a comment that advisor and tournament work compete for the same CPUs, which is intentional and gives an interesting experiment.
- **Identity:** `worker_id = POD_NAME` (default: hostname), `node_name = NODE_NAME` (default `"local"`), sent in every `WorkerInfo`.

## Tests
- A mock coordinator (an in-process tonic `WorkerService` impl on a random port that hands out N units and records completions), plus a fake engine. The worker completes every unit, exactly once each.
- Lost lease: the mock marks a unit lost in a heartbeat, so the worker stops (with chunking) and counts it as wasted.
- Panic: a fake engine that panics gives an error completion, and the worker keeps going on the next unit.
- Coordinator down: start the worker with no coordinator, start the mock 1–2s later, and the worker recovers and completes the work.
- Graceful shutdown: trigger the shutdown signal (inject a `CancellationToken` rather than a real SIGTERM) mid-unit, so the unit finishes and no new lease is requested.
- AdvisorShard: call `Simulate` against the fake engine and check the response mapping.

## Acceptance
```
cargo test -p worker
COORDINATOR_GRPC_URL=http://localhost:50051 WORKER_THREADS=4 LOG_FORMAT=pretty cargo run -p worker   # with no coordinator: logs backoff, doesn't crash
curl -s localhost:9090/metrics | grep hearts_worker_
```
