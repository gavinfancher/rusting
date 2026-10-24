//! worker binary entry point: parse config, set up logging and signal
//! handling, then hand over to `worker::app::run`.

use anyhow::Context;
use clap::Parser;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use worker::config::Config;
use worker::engine::CoreEngine;

fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();
    init_logging(&cfg);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    let result = rt.block_on(async {
        let shutdown = CancellationToken::new();
        tokio::spawn(wait_for_signal(shutdown.clone()));
        worker::app::run(cfg, Arc::new(CoreEngine), shutdown).await
    });
    // Dropping a runtime waits for blocking threads; a unit abandoned after
    // the grace period might still be finishing a chunk. Don't wait long.
    rt.shutdown_timeout(Duration::from_secs(2));
    result
}

fn init_logging(cfg: &Config) {
    let filter = EnvFilter::try_new(&cfg.rust_log).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if cfg.log_format == "pretty" {
        builder.pretty().init();
    } else {
        builder.json().init();
    }
}

/// Cancel `shutdown` on SIGTERM (Kubernetes) or Ctrl-C (local).
async fn wait_for_signal(shutdown: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot install SIGTERM handler");
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM received: draining"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received: draining"),
    }
    shutdown.cancel();
}
