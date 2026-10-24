//! # coordinator
//!
//! The control plane of the Hearts cluster. It is a library plus a thin
//! `main.rs` so that integration tests can start the same pieces in-process.
//!
//! * [`db`] owns **all** SQL. Postgres is the only shared state: every
//!   coordination decision (who holds which work unit, when a job is done)
//!   is made by row locks in the database, never by in-memory state. That is
//!   what makes running several coordinator replicas safe.
//! * [`grpc`] serves `WorkerService` (workers pull units of work).
//! * [`http`] serves the REST API, SSE progress streams, metrics, health
//!   checks and the embedded web UI.
//! * [`advisor_fanout`] splits `/api/advise` across worker pods and merges.
//! * [`metrics`] defines every Prometheus metric from `docs/CONTRACTS.md`.
//! * [`background`] is the periodic sweeper / gauge refresher.
//! * [`trainer`] drives cross-entropy-method training runs (Level 1 learning).

pub mod advisor_fanout;
pub mod background;
pub mod config;
pub mod db;
pub mod grpc;
pub mod http;
pub mod metrics;
pub mod model;
pub mod state;
pub mod trainer;
