//! Periodic housekeeping, every `STATS_POLL_SECONDS`:
//!
//! 1. fail units whose lease expired and that have no attempts left (and
//!    finish jobs that this completes),
//! 2. refresh the polled gauges: `hearts_units{state}`,
//!    `hearts_workers_active`, `hearts_db_pool_connections{state}`.
//!
//! Several coordinator replicas may each run this loop; every step is an
//! idempotent SQL statement, so that is harmless.

use crate::state::AppState;
use std::time::Duration;

/// Run one round (also called directly by tests).
pub async fn tick(state: &AppState) {
    match state.db.sweep_expired().await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(units = n, "failed units that exhausted their attempts"),
        Err(e) => tracing::error!(error = %e, "sweep failed"),
    }

    match state.db.unit_counts().await {
        Ok(rows) => {
            // Reset first: a state with zero rows won't appear in the result.
            for s in ["pending", "leased", "done", "failed", "cancelled"] {
                state.metrics.units.with_label_values(&[s]).set(0);
            }
            for (s, n) in rows {
                state.metrics.units.with_label_values(&[&s]).set(n);
            }
        }
        Err(e) => tracing::error!(error = %e, "unit count refresh failed"),
    }

    match state.db.count_active_workers().await {
        Ok(n) => state.metrics.workers_active.set(n),
        Err(e) => tracing::error!(error = %e, "worker count refresh failed"),
    }

    let pool = state.db.pool();
    let size = i64::from(pool.size());
    let idle = pool.num_idle() as i64;
    state.metrics.db_pool.with_label_values(&["idle"]).set(idle);
    state
        .metrics
        .db_pool
        .with_label_values(&["in_use"])
        .set((size - idle).max(0));
}

/// Loop until shutdown.
pub async fn run(state: AppState) {
    let mut shutdown = state.shutdown.clone();
    let period = Duration::from_secs(state.config.stats_poll_seconds.max(1));
    let mut interval = tokio::time::interval(period);
    loop {
        tokio::select! {
            _ = interval.tick() => tick(&state).await,
            _ = crate::state::wait_for_shutdown(&mut shutdown) => break,
        }
    }
}
