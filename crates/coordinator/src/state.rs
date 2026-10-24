//! Shared application state handed to every HTTP and gRPC handler.
//!
//! Everything here is cheap to clone (`Arc`s and a connection-pool handle)
//! because axum clones the state once per request. Note what is *not* here:
//! no job or lease bookkeeping. All of that lives in Postgres.

use crate::{advisor_fanout::Fanout, config::Config, db::Db, metrics::Metrics};
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub metrics: Arc<Metrics>,
    pub config: Arc<Config>,
    pub fanout: Arc<Fanout>,
    /// Flips to `true` when the process is shutting down. Long-lived
    /// responses (SSE) watch it so graceful shutdown doesn't wait on them.
    pub shutdown: watch::Receiver<bool>,
}

/// Resolve once the shutdown flag is `true` (or its sender is gone).
///
/// `watch::Receiver::wait_for` returns a guard that is not `Send`; wrapping
/// the call in this function drops the guard immediately, so futures that
/// await this stay `Send` and can be `tokio::spawn`ed.
pub async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

impl AppState {
    pub fn new(
        db: Db,
        metrics: Arc<Metrics>,
        config: Arc<Config>,
        shutdown: watch::Receiver<bool>,
    ) -> AppState {
        let fanout = Arc::new(Fanout::new(
            config.advisor_shard_dns.clone(),
            metrics.clone(),
        ));
        AppState {
            db,
            metrics,
            config,
            fanout,
            shutdown,
        }
    }
}
