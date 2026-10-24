//! Liveness and readiness probes.
//!
//! Kubernetes uses these differently: a failing *liveness* probe restarts the
//! container, a failing *readiness* probe merely stops routing traffic to it.
//! So `/healthz` never touches the database (a DB outage must not cause
//! restart loops), while `/readyz` checks it.

use crate::state::AppState;
use axum::{extract::State, http::StatusCode};
use std::time::Duration;

pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn readyz(State(state): State<AppState>) -> (StatusCode, &'static str) {
    if state.db.ping(Duration::from_secs(1)).await {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
    }
}
