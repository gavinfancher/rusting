//! Job endpoints: create, list, get, cancel, and the SSE progress stream.

use super::{ApiError, parse_id, parse_json};
use crate::{db::Db, model::Job, state::AppState};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{self, Stream};
use hearts_core::{JobSpec, Rules, StrategyKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

const MAX_HANDS: i64 = 1_000_000_000;
const MAX_UNIT_SIZE: i64 = 1_000_000;
const DEFAULT_UNIT_SIZE: i64 = 2000;
/// Keeps the single set-based INSERT of units to a sane size.
const MAX_UNITS: i64 = 1_000_000;
/// Seeds must survive a round trip through JSON numbers (f64 in JS): < 2^53.
const MAX_SEED: u64 = 1 << 53;
const MAX_NAME_LEN: usize = 200;

#[derive(Debug, Deserialize)]
struct CreateJob {
    name: Option<String>,
    strategies: Vec<String>,
    num_hands: i64,
    seed: Option<u64>,
    unit_size: Option<i64>,
    rules: Option<Rules>,
}

/// `POST /api/jobs`
pub async fn create(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    let req: CreateJob = parse_json(&body)?;

    let strategies: Vec<StrategyKind> = req
        .strategies
        .iter()
        .map(|s| s.parse::<StrategyKind>().map_err(ApiError::bad_request))
        .collect::<Result<_, _>>()?;
    let strategies: [StrategyKind; 4] = strategies.try_into().map_err(|v: Vec<_>| {
        ApiError::bad_request(format!(
            "strategies must have exactly 4 entries, got {}",
            v.len()
        ))
    })?;

    if !(1..=MAX_HANDS).contains(&req.num_hands) {
        return Err(ApiError::bad_request(format!(
            "num_hands must be between 1 and {MAX_HANDS}"
        )));
    }
    let unit_size = req.unit_size.unwrap_or(DEFAULT_UNIT_SIZE);
    if !(1..=MAX_UNIT_SIZE).contains(&unit_size) {
        return Err(ApiError::bad_request(format!(
            "unit_size must be between 1 and {MAX_UNIT_SIZE}"
        )));
    }
    let units = (req.num_hands + unit_size - 1) / unit_size;
    if units > MAX_UNITS {
        return Err(ApiError::bad_request(format!(
            "num_hands / unit_size = {units} units; the maximum is {MAX_UNITS}. Use a larger unit_size"
        )));
    }
    let seed = match req.seed {
        Some(s) if s >= MAX_SEED => {
            return Err(ApiError::bad_request("seed must be < 2^53"));
        }
        Some(s) => s,
        None => rand::random::<u64>() % MAX_SEED,
    };
    if req.name.as_deref().is_some_and(|n| n.len() > MAX_NAME_LEN) {
        return Err(ApiError::bad_request(format!(
            "name must be at most {MAX_NAME_LEN} bytes"
        )));
    }

    let spec = JobSpec {
        seed,
        strategies,
        rules: req.rules.unwrap_or_default(),
    };
    let id = state
        .db
        .create_job(req.name.as_deref(), &spec, req.num_hands, unit_size as i32)
        .await?;
    let job = state
        .db
        .get_job(id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "job vanished"))?;
    Ok((StatusCode::CREATED, Json(job)))
}

/// `GET /api/jobs?limit=50`
pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<Job>>, ApiError> {
    let limit = match params.get("limit") {
        None => 50,
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=500).contains(n))
            .ok_or_else(|| ApiError::bad_request("limit must be between 1 and 500"))?,
    };
    Ok(Json(state.db.list_jobs(limit).await?))
}

/// `GET /api/jobs/{id}`
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let id = parse_id(&id)?;
    state
        .db
        .get_job(id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("job {id} not found")))
}

/// `POST /api/jobs/{id}/cancel`
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let id = parse_id(&id)?;
    state
        .db
        .cancel_job(id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("job {id} not found")))
}

/// Per-subscriber state of an SSE stream.
struct SseState {
    db: Db,
    id: i64,
    ticker: tokio::time::Interval,
    shutdown: tokio::sync::watch::Receiver<bool>,
    finished: bool,
}

/// `GET /api/jobs/{id}/events`: `event: progress` every second, then one
/// `event: done` when the job reaches a terminal status.
///
/// Each subscriber polls the database once a second. That is simple and fine
/// at this scale; an optimization candidate is one shared poller per job
/// fanned out through a `tokio::sync::broadcast` channel (or Postgres
/// LISTEN/NOTIFY), so N browser tabs cost one query per second, not N.
pub async fn events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let id = parse_id(&id)?;
    // 404 up front, before committing to a streaming response.
    if state.db.get_job(id).await?.is_none() {
        return Err(ApiError::not_found(format!("job {id} not found")));
    }
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    // If a poll is slow, don't fire a burst of catch-up ticks afterwards.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let init = SseState {
        db: state.db.clone(),
        id,
        ticker,
        shutdown: state.shutdown.clone(),
        finished: false,
    };

    // `unfold` turns "state -> next item + next state" into a Stream.
    let stream = stream::unfold(init, |mut st| async move {
        if st.finished {
            return None; // we already sent `done`; closing the stream ends the response
        }
        loop {
            // Wait for the next tick, or stop if the server is shutting down.
            tokio::select! {
                _ = st.ticker.tick() => {}
                _ = crate::state::wait_for_shutdown(&mut st.shutdown) => return None,
            }
            match st.db.get_job(st.id).await {
                Ok(Some(job)) => {
                    let name = if job.is_terminal() {
                        "done"
                    } else {
                        "progress"
                    };
                    st.finished = job.is_terminal();
                    let event = Event::default()
                        .event(name)
                        .json_data(&job)
                        .unwrap_or_else(|_| Event::default().event("error").data("serialize"));
                    return Some((Ok(event), st));
                }
                // Job deleted (cascade) or transient DB error: end / retry.
                Ok(None) => return None,
                Err(e) => tracing::warn!(error = %e, job = st.id, "SSE poll failed; retrying"),
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
