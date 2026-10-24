//! `GET /api/workers`

use super::ApiError;
use crate::{model::WorkerRow, state::AppState};
use axum::{Json, extract::State};

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<WorkerRow>>, ApiError> {
    Ok(Json(state.db.list_workers().await?))
}
