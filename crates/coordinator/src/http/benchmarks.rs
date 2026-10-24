//! `GET/POST /api/benchmarks`: rows of the `benchmark_runs` table, used to
//! compare scaling experiments over time.

use super::{ApiError, parse_json};
use crate::{
    model::{BenchmarkRun, NewBenchmark},
    state::AppState,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
};
use std::collections::HashMap;

/// `POST /api/benchmarks`. If `job_id` is given, any of `hands`, `wall_ms`,
/// `hands_per_sec` (and `unit_size`) left out are filled from the job.
pub async fn create(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<BenchmarkRun>), ApiError> {
    let mut b: NewBenchmark = parse_json(&body)?;
    if b.experiment.trim().is_empty() {
        return Err(ApiError::bad_request("experiment is required"));
    }
    if let Some(job_id) = b.job_id {
        let job = state
            .db
            .get_job(job_id)
            .await?
            .ok_or_else(|| ApiError::bad_request(format!("job {job_id} not found")))?;
        // Hands actually simulated (equals num_hands for a finished job).
        b.hands.get_or_insert(job.progress.hands_done);
        b.unit_size.get_or_insert(job.unit_size);
        if let Some(ms) = job.elapsed_ms {
            b.wall_ms.get_or_insert(ms);
        }
        if let Some(hps) = job.hands_per_sec {
            b.hands_per_sec.get_or_insert(hps);
        }
    }
    let row = state.db.insert_benchmark(&b).await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `GET /api/benchmarks?experiment=E2-horizontal&limit=100`
pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<BenchmarkRun>>, ApiError> {
    let limit = match params.get("limit") {
        None => 200,
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=1000).contains(n))
            .ok_or_else(|| ApiError::bad_request("limit must be between 1 and 1000"))?,
    };
    let experiment = params.get("experiment").map(String::as_str);
    Ok(Json(state.db.list_benchmarks(experiment, limit).await?))
}
