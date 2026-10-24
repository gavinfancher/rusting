//! # worker
//!
//! The compute node of the distributed Hearts simulator. It does four jobs:
//!
//! 1. **Pull work** from the coordinator (`lease`): lease a unit, compute it,
//!    report the result. Pull-based scheduling means a slow or dead worker
//!    never blocks the others.
//! 2. **Burn CPU** on one shared rayon pool (`runner`).
//! 3. **Answer advisor requests** over gRPC (`advisor_shard`).
//! 4. **Expose health and metrics** over HTTP (`http`, `metrics`).
//!
//! ## Concurrency design in one picture
//!
//! ```text
//!  tokio runtime (async, few threads, must never block)
//!  +----------------------------------------------------------------+
//!  | lease loop task --> LeaseWork / CompleteWork RPCs              |
//!  | heartbeat task  --> Heartbeat RPC every N seconds              |
//!  | gRPC server     --> AdvisorShard::Simulate                     |
//!  | HTTP server     --> /metrics /healthz /readyz                  |
//!  +---------------+------------------------------------------------+
//!       spawn_blocking (hand the CPU work to a blocking thread)
//!  +---------------v------------------------------------------------+
//!  | blocking thread: pool.install(|| engine.run_hands(..))         |
//!  |      `-> rayon pool (WORKER_THREADS threads) does the maths    |
//!  +----------------------------------------------------------------+
//! ```
//!
//! The *why*: tokio's async threads are cooperative. A task only yields at
//! `.await` points, so a 30-second CPU loop on an async thread would freeze
//! every other task on that thread (heartbeats would be missed, the lease
//! would expire, health probes would time out). So CPU work is moved onto
//! separate threads: `spawn_blocking` gives us a thread that is allowed to
//! block, and `ThreadPool::install` runs the closure inside our sized rayon
//! pool so rayon's parallel iterators use exactly `WORKER_THREADS` threads
//! (this is what makes the Kubernetes CPU limit and vertical scaling work).

pub mod advisor_shard;
pub mod app;
pub mod config;
pub mod engine;
pub mod http;
pub mod lease;
pub mod metrics;
pub mod runner;
