//! The small HTTP server on :9090: `/metrics`, `/healthz`, `/readyz`.
//!
//! * `/healthz` (liveness): 200 whenever the process can answer at all.
//! * `/readyz` (readiness): 200 if we talked to the coordinator recently, or
//!   if we're still inside the startup grace period and have simply not had a
//!   chance yet ("no work yet"). Otherwise 503. Kubernetes uses this to take
//!   an isolated pod out of rotation, without restarting it (a coordinator
//!   outage is not the worker's fault, so liveness must not depend on it).

use crate::metrics::Metrics;
use axum::{Router, extract::State, http::StatusCode, routing::get};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Tracks when we last completed a successful RPC with the coordinator.
pub struct Health {
    started: Instant,
    /// Milliseconds since `started` of the last success, plus 1; 0 = never.
    /// (A single atomic is enough; no lock needed for a timestamp.)
    last_ok: AtomicU64,
    window: Duration,
}

impl Health {
    pub fn new(window: Duration) -> Health {
        Health {
            started: Instant::now(),
            last_ok: AtomicU64::new(0),
            window,
        }
    }

    pub fn mark_ok(&self) {
        self.last_ok.store(
            self.started.elapsed().as_millis() as u64 + 1,
            Ordering::Relaxed,
        );
    }

    pub fn is_ready(&self) -> bool {
        let now = self.started.elapsed().as_millis() as u64;
        let window = self.window.as_millis() as u64;
        match self.last_ok.load(Ordering::Relaxed) {
            // Never connected: ready only during the startup grace period.
            0 => now < window,
            t => now.saturating_sub(t - 1) < window,
        }
    }
}

#[derive(Clone)]
pub struct HttpState {
    pub metrics: Arc<Metrics>,
    pub health: Arc<Health>,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .with_state(state)
}

async fn metrics(State(s): State<HttpState>) -> ([(&'static str, &'static str); 1], String) {
    (
        [("content-type", "text/plain; version=0.0.4")],
        s.metrics.render(),
    )
}

async fn readyz(State(s): State<HttpState>) -> (StatusCode, &'static str) {
    if s.health.is_ready() {
        (StatusCode::OK, "ready")
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "no recent contact with coordinator",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_during_startup_then_depends_on_contact() {
        let h = Health::new(Duration::from_millis(50));
        assert!(h.is_ready()); // startup grace
        std::thread::sleep(Duration::from_millis(80));
        assert!(!h.is_ready()); // never connected, grace over
        h.mark_ok();
        assert!(h.is_ready());
    }
}
