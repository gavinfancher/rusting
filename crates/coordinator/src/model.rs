//! Plain data types returned by the database layer and serialized as JSON by
//! the REST API (shapes fixed by `docs/CONTRACTS.md`).

use chrono::{DateTime, Utc};
use hearts_core::JobSpec;
use serde::{Deserialize, Serialize};

/// The `spec` object inside a Job: `{seed, strategies, rules}`. This is just
/// `hearts_core::JobSpec`, so the JSON stored in Postgres, the REST response
/// and the gRPC message all derive from one definition.
pub type Spec = JobSpec;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Progress {
    pub units_total: i64,
    pub pending: i64,
    pub leased: i64,
    pub done: i64,
    pub failed: i64,
    pub cancelled: i64,
    pub hands_done: i64,
    /// `SUM(GREATEST(attempts - 1, 0))`: how many times units were handed
    /// out again after a lease expiry or a reported error.
    pub reclaims: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Standing {
    pub slot: u8,
    pub strategy: String,
    pub hands: i64,
    pub points: i64,
    pub avg_points: f64,
    pub moons: i64,
    pub queens: i64,
    pub wins: i64,
    pub win_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub name: Option<String>,
    /// pending | running | done | failed | cancelled
    pub status: String,
    pub spec: Spec,
    pub num_hands: i64,
    pub unit_size: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub progress: Progress,
    pub standings: Vec<Standing>,
    pub elapsed_ms: Option<i64>,
    pub hands_per_sec: Option<f64>,
}

impl Job {
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "done" | "failed" | "cancelled")
    }
}

/// A unit handed to a worker by `Db::lease`.
#[derive(Debug, Clone)]
pub struct LeasedUnit {
    pub unit_id: i64,
    pub job_id: i64,
    pub hand_start: u64,
    pub hand_end: u64,
    pub spec: Spec,
    pub attempt: i32,
    pub lease_expires_at: DateTime<Utc>,
    /// True if this unit was previously leased and that lease had expired.
    pub reclaimed: bool,
}

/// Result of `Db::complete_ok` / `Db::complete_err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteOutcome {
    /// False when the report was ignored (unit already done / cancelled, or
    /// an error report from a worker that no longer holds the lease).
    pub accepted: bool,
    /// Set when this completion made the job terminal: `done` or `failed`.
    pub job_finished: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct WorkerRow {
    pub worker_id: String,
    pub pod_name: String,
    pub node_name: String,
    pub threads: i32,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub active: bool,
    pub leased_units: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BenchmarkRun {
    pub id: i64,
    pub experiment: String,
    pub job_id: Option<i64>,
    pub git_sha: Option<String>,
    pub worker_replicas: Option<i32>,
    pub threads_per_worker: Option<i32>,
    pub cpu_limit: Option<String>,
    pub unit_size: Option<i32>,
    pub pimc_samples: Option<i32>,
    pub hands: Option<i64>,
    pub wall_ms: Option<i64>,
    pub hands_per_sec: Option<f64>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Body of `POST /api/benchmarks` (the table's columns minus id/created_at).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct NewBenchmark {
    pub experiment: String,
    pub job_id: Option<i64>,
    pub git_sha: Option<String>,
    pub worker_replicas: Option<i32>,
    pub threads_per_worker: Option<i32>,
    pub cpu_limit: Option<String>,
    pub unit_size: Option<i32>,
    pub pimc_samples: Option<i32>,
    pub hands: Option<i64>,
    pub wall_ms: Option<i64>,
    pub hands_per_sec: Option<f64>,
    pub notes: Option<String>,
}
