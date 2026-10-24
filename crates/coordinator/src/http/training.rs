//! Training endpoints: `POST/GET /api/training`, `GET /api/training/:id`,
//! `POST /api/training/:id/cancel`, plus [`resolve_strategy`], which turns the
//! `champion:<run_id>` alias into a concrete `linear:...` strategy.
//!
//! These handlers only validate input and read/write rows. The actual
//! training is advanced by the background [`crate::trainer`].

use super::{ApiError, parse_id, parse_json};
use crate::{
    db::Db,
    model::{TrainingConfig, TrainingRun},
    state::AppState,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hearts_core::{Rules, StrategyKind, learn::Init};
use serde::Deserialize;
use std::collections::HashMap;

const MAX_HANDS: i64 = 1_000_000_000;
const MAX_UNIT_SIZE: i64 = 1_000_000;
/// Per generation: `population` jobs, each split into units. Keeps one
/// generation's unit INSERTs sane (same spirit as the job limit).
const MAX_UNITS_PER_GENERATION: i64 = 1_000_000;
const MAX_SEED: u64 = 1 << 53;
const MAX_NAME_LEN: usize = 200;

/// `POST /api/training` body. Every field is optional; see CONTRACTS.md.
#[derive(Debug, Deserialize, Default)]
struct CreateTraining {
    name: Option<String>,
    population: Option<usize>,
    elites: Option<usize>,
    generations: Option<u32>,
    hands_per_candidate: Option<i64>,
    unit_size: Option<i64>,
    opponents: Option<Vec<String>>,
    init: Option<Init>,
    sigma0: Option<f64>,
    smoothing: Option<f64>,
    min_sigma: Option<f64>,
    seed: Option<u64>,
    validation_hands: Option<i64>,
    rules: Option<Rules>,
}

/// Parse one strategy string, resolving `champion:<run_id>` first.
///
/// `champion:N` becomes the run's *current* champion as `linear:...`
/// (so the job's stored spec is self-contained and never changes later).
/// 400 if the run doesn't exist or has no champion yet.
pub async fn resolve_strategy(db: &Db, raw: &str) -> Result<StrategyKind, ApiError> {
    let lower = raw.trim().to_ascii_lowercase();
    if let Some(id) = lower.strip_prefix("champion:") {
        let run_id = id
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| ApiError::bad_request(format!("invalid champion reference {raw:?}")))?;
        return match db.training_champion(run_id).await? {
            None => Err(ApiError::bad_request(format!(
                "training run {run_id} not found"
            ))),
            Some(None) => Err(ApiError::bad_request(format!(
                "training run {run_id} has no champion yet"
            ))),
            Some(Some(w)) => Ok(StrategyKind::Linear(w)),
        };
    }
    raw.parse::<StrategyKind>().map_err(ApiError::bad_request)
}

/// `POST /api/training`
pub async fn create(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<TrainingRun>), ApiError> {
    let req: CreateTraining = parse_json(&body)?;

    let population = req.population.unwrap_or(24);
    if !(4..=256).contains(&population) {
        return Err(ApiError::bad_request(
            "population must be between 4 and 256",
        ));
    }
    // Default 6, but never more than population-1 so `population: 4` alone is valid.
    let elites = req.elites.unwrap_or_else(|| 6.min(population - 1));
    if !(1..population).contains(&elites) {
        return Err(ApiError::bad_request(format!(
            "elites must be between 1 and population-1 ({})",
            population - 1
        )));
    }
    let generations = req.generations.unwrap_or(30);
    if !(1..=1000).contains(&generations) {
        return Err(ApiError::bad_request(
            "generations must be between 1 and 1000",
        ));
    }
    let hands_per_candidate = req.hands_per_candidate.unwrap_or(40_000);
    if !(100..=10_000_000).contains(&hands_per_candidate) {
        return Err(ApiError::bad_request(
            "hands_per_candidate must be between 100 and 10000000",
        ));
    }
    let unit_size = req.unit_size.unwrap_or(5000);
    if !(1..=MAX_UNIT_SIZE).contains(&unit_size) {
        return Err(ApiError::bad_request(format!(
            "unit_size must be between 1 and {MAX_UNIT_SIZE}"
        )));
    }
    let validation_hands = req.validation_hands.unwrap_or(200_000);
    if !(1..=MAX_HANDS).contains(&validation_hands) {
        return Err(ApiError::bad_request(format!(
            "validation_hands must be between 1 and {MAX_HANDS}"
        )));
    }
    let units_per_generation =
        population as i64 * ((hands_per_candidate + unit_size - 1) / unit_size);
    if units_per_generation > MAX_UNITS_PER_GENERATION {
        return Err(ApiError::bad_request(format!(
            "population x hands_per_candidate / unit_size = {units_per_generation} units per generation; \
             the maximum is {MAX_UNITS_PER_GENERATION}. Use a larger unit_size"
        )));
    }
    let sigma0 = req.sigma0.unwrap_or(20.0);
    if !(sigma0.is_finite() && sigma0 > 0.0) {
        return Err(ApiError::bad_request("sigma0 must be a positive number"));
    }
    let smoothing = req.smoothing.unwrap_or(0.7);
    if !(smoothing > 0.0 && smoothing <= 1.0) {
        return Err(ApiError::bad_request("smoothing must be in (0, 1]"));
    }
    let min_sigma = req.min_sigma.unwrap_or(2.0);
    if !(min_sigma.is_finite() && min_sigma >= 0.0) {
        return Err(ApiError::bad_request("min_sigma must be >= 0"));
    }
    let seed = match req.seed {
        Some(s) if s >= MAX_SEED => return Err(ApiError::bad_request("seed must be < 2^53")),
        Some(s) => s,
        None => rand::random::<u64>() % MAX_SEED,
    };
    if req.name.as_deref().is_some_and(|n| n.len() > MAX_NAME_LEN) {
        return Err(ApiError::bad_request(format!(
            "name must be at most {MAX_NAME_LEN} bytes"
        )));
    }
    let raw_opponents = req.opponents.unwrap_or_else(|| vec!["heuristic".into(); 3]);
    if raw_opponents.len() != 3 {
        return Err(ApiError::bad_request(format!(
            "opponents must have exactly 3 entries, got {}",
            raw_opponents.len()
        )));
    }
    // Stored in canonical form (champion:N resolved, normalised spelling).
    let mut opponents = Vec::with_capacity(3);
    for o in &raw_opponents {
        opponents.push(resolve_strategy(&state.db, o).await?.to_string());
    }

    let cfg = TrainingConfig {
        name: req.name,
        population,
        elites,
        generations,
        hands_per_candidate,
        unit_size,
        opponents,
        init: req.init.unwrap_or(Init::Heuristic),
        sigma0,
        smoothing,
        min_sigma,
        seed,
        validation_hands,
        rules: req.rules.unwrap_or_default(),
    };
    let initial = state.cem.initial_state(cfg.init, cfg.sigma0);
    let id = state.db.create_training_run(&cfg, &initial).await?;
    let run = state
        .db
        .get_training_run(id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "run vanished"))?;
    Ok((StatusCode::CREATED, Json(run)))
}

/// `GET /api/training?limit=50`
pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<TrainingRun>>, ApiError> {
    let limit = match params.get("limit") {
        None => 50,
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=500).contains(n))
            .ok_or_else(|| ApiError::bad_request("limit must be between 1 and 500"))?,
    };
    Ok(Json(state.db.list_training_runs(limit).await?))
}

/// `GET /api/training/{id}`
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TrainingRun>, ApiError> {
    let id = parse_id(&id)?;
    state
        .db
        .get_training_run(id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("training run {id} not found")))
}

/// `POST /api/training/{id}/cancel`
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TrainingRun>, ApiError> {
    let id = parse_id(&id)?;
    state
        .db
        .cancel_training_run(id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("training run {id} not found")))
}
