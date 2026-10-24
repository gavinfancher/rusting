//! Plain data types returned by the database layer and serialized as JSON by
//! the REST API (shapes fixed by `docs/CONTRACTS.md`).

use chrono::{DateTime, Utc};
use hearts_core::{FEATURE_NAMES, JobSpec, Rules, Weights, learn::Init};
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
    /// Set on tournament jobs created by the trainer (hidden from the
    /// default jobs list).
    pub training_run_id: Option<i64>,
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

// ---------------------------------------------------------------------------
// Training (Level 1 learning). JSON shapes are fixed by CONTRACTS.md.
// ---------------------------------------------------------------------------

/// The stored `training_runs.config`: the `POST /api/training` body with every
/// default filled in. Serialized into Postgres as JSONB and echoed back in the
/// API, so the run is fully reproducible from this plus `seed`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrainingConfig {
    pub name: Option<String>,
    pub population: usize,
    pub elites: usize,
    pub generations: u32,
    pub hands_per_candidate: i64,
    pub unit_size: i64,
    /// Canonical strategy strings (already `champion:N`-resolved).
    pub opponents: Vec<String>,
    pub init: Init,
    pub sigma0: f64,
    pub smoothing: f64,
    pub min_sigma: f64,
    pub seed: u64,
    pub validation_hands: i64,
    pub rules: Rules,
}

/// Progress of the generation currently being evaluated.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CurrentGeneration {
    pub generation: i32,
    pub candidates_total: i64,
    pub candidates_done: i64,
    pub hands_done: i64,
    pub hands_total: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FeatureWeight {
    pub feature: &'static str,
    pub weight: i32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Champion {
    /// `linear:w0,...,w23`, usable directly as a strategy in `POST /api/jobs`.
    pub strategy: String,
    /// `null` if the measuring job failed (JSON can't carry infinity).
    pub fitness: Option<f64>,
    pub weights: Vec<FeatureWeight>,
}

impl Champion {
    pub fn new(w: Weights, fitness: Option<f64>) -> Champion {
        Champion {
            strategy: hearts_core::StrategyKind::Linear(w).to_string(),
            fitness,
            weights: FEATURE_NAMES
                .iter()
                .zip(w.0)
                .map(|(feature, weight)| FeatureWeight { feature, weight })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GenerationStats {
    pub generation: i32,
    pub seed: u64,
    pub mean_fitness: Option<f64>,
    pub best_fitness: Option<f64>,
    pub elite_mean_fitness: Option<f64>,
    pub population_mean_fitness: Option<f64>,
    pub sigma_mean: Option<f64>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub elapsed_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Validation {
    pub hands: i64,
    pub champion_fitness: Option<f64>,
    pub baseline_fitness: Option<f64>,
    pub champion_job_id: i64,
    pub baseline_job_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrainingRun {
    pub id: i64,
    pub name: Option<String>,
    /// running | validating | done | cancelled | failed
    pub status: String,
    pub config: TrainingConfig,
    pub seed: i64,
    pub generation: i32,
    pub generations_total: i32,
    pub current: Option<CurrentGeneration>,
    pub champion: Option<Champion>,
    /// Omitted from the list endpoint, present on `GET /api/training/:id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generations: Option<Vec<GenerationStats>>,
    pub validation: Option<Validation>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}
