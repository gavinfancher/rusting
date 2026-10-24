//! HTTP server: REST API, SSE, metrics, health checks and the embedded UI.
//!
//! One axum [`Router`] is built by [`router`]. Handlers share [`AppState`].
//! Errors from any handler become `{"error": "..."}` JSON through
//! [`ApiError`], with the right 4xx/5xx status.

pub mod advise;
pub mod benchmarks;
pub mod health;
pub mod jobs;
pub mod static_files;
pub mod workers;

use crate::{db::DbError, state::AppState};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{MatchedPath, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::de::DeserializeOwned;
use serde_json::json;

/// Error type for all handlers.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

/// Database errors are the server's fault (500). We log the detail and
/// return a generic message so internals don't leak to clients.
impl From<DbError> for ApiError {
    fn from(e: DbError) -> ApiError {
        tracing::error!(error = %e, "database error in HTTP handler");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    }
}

/// Parse a JSON request body ourselves instead of using axum's `Json`
/// extractor, whose rejections are plain text. This keeps every error in the
/// `{"error": ...}` shape the contract promises.
pub fn parse_json<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))
}

/// Parse a `:id` path segment (same reason as [`parse_json`]).
pub fn parse_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>()
        .ok()
        .filter(|&id| id > 0)
        .ok_or_else(|| ApiError::bad_request(format!("invalid id {raw:?}")))
}

/// Counts requests in `hearts_http_requests_total{route,status}`. The route
/// label is the matched *pattern* (`/api/jobs/:id`), never the concrete URL,
/// to keep label cardinality bounded.
async fn count_requests(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().replace("{id}", ":id"))
        .unwrap_or_else(|| "static".to_string());
    let response = next.run(req).await;
    state
        .metrics
        .http_requests
        .with_label_values(&[&route, response.status().as_str()])
        .inc();
    response
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/jobs", post(jobs::create).get(jobs::list))
        .route("/api/jobs/{id}", get(jobs::get_one))
        .route("/api/jobs/{id}/cancel", post(jobs::cancel))
        .route("/api/jobs/{id}/events", get(jobs::events))
        .route("/api/advise", post(advise::advise))
        .route("/api/workers", get(workers::list))
        .route(
            "/api/benchmarks",
            get(benchmarks::list).post(benchmarks::create),
        )
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/metrics", get(metrics_handler))
        // Anything else is a static UI file (or 404).
        .fallback(static_files::serve)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            count_requests,
        ))
        .with_state(state)
}
