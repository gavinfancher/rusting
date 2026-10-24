//! Coordinator binary: wires configuration, the database, the gRPC server,
//! the HTTP server and the background task together, and shuts them all
//! down cleanly on SIGINT / SIGTERM.

use anyhow::Context;
use clap::Parser;
use coordinator::{
    background,
    config::{Config, LogFormat},
    db::{self, Db},
    grpc::WorkerSvc,
    http,
    metrics::Metrics,
    state::AppState,
    trainer,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Pretty => builder.init(),
    }
}

/// Resolves when the process is asked to stop (Ctrl-C locally, SIGTERM from
/// Kubernetes).
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Arc::new(Config::parse());
    init_logging(config.log_format);
    tracing::info!(http = %config.http_addr, grpc = %config.grpc_addr, "starting coordinator");

    // Shutdown plumbing: one `watch` channel; flipping it to `true` tells
    // every server and loop to wind down. (Each holds a Receiver clone.)
    let (stop_tx, stop_rx) = watch::channel(false);

    let pool = db::connect_with_retry(&config.database_url, config.db_pool_size, 60)
        .await
        .context("connecting to the database")?;
    db::migrate(&pool).await.context("running migrations")?;
    tracing::info!("database ready, migrations applied");

    let metrics = Arc::new(Metrics::new());
    let db = Db::new(
        pool,
        metrics.clone(),
        config.lease_seconds,
        config.max_attempts,
    );
    let state = AppState::new(db.clone(), metrics.clone(), config.clone(), stop_rx.clone());

    // --- gRPC (workers) ---
    let grpc_svc = WorkerSvc::new(db, metrics, config.no_work_retry_ms).into_server();
    let mut grpc_stop = stop_rx.clone();
    let grpc_addr = config.grpc_addr;
    let grpc = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(grpc_svc)
            .serve_with_shutdown(grpc_addr, async move {
                let _ = grpc_stop.wait_for(|s| *s).await;
            })
            .await
    });

    // --- HTTP (UI, REST, SSE, metrics, health) ---
    let listener = tokio::net::TcpListener::bind(config.http_addr)
        .await
        .with_context(|| format!("binding HTTP {}", config.http_addr))?;
    let mut http_stop = stop_rx.clone();
    let app = http::router(state.clone());
    let http = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = http_stop.wait_for(|s| *s).await;
            })
            .await
    });

    // --- background sweeper / gauges ---
    // --- training: the trainer task advances CEM runs (all state in Postgres) ---
    let trainer = tokio::spawn(trainer::run(state.clone()));
    let bg = tokio::spawn(background::run(state));

    wait_for_signal().await;
    tracing::info!("shutdown requested; draining");
    let _ = stop_tx.send(true);

    // Give in-flight requests a bounded time to finish.
    let drain = async {
        match http.await {
            Ok(Err(e)) => tracing::error!(error = %e, "HTTP server error"),
            Err(e) => tracing::error!(error = %e, "HTTP task failed"),
            Ok(Ok(())) => {}
        }
        match grpc.await {
            Ok(Err(e)) => tracing::error!(error = %e, "gRPC server error"),
            Err(e) => tracing::error!(error = %e, "gRPC task failed"),
            Ok(Ok(())) => {}
        }
        let _ = bg.await;
        let _ = trainer.await;
    };
    if tokio::time::timeout(Duration::from_secs(20), drain)
        .await
        .is_err()
    {
        tracing::warn!("drain timed out; exiting anyway");
    }
    tracing::info!("bye");
    Ok(())
}
