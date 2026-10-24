//! Wiring: build the pool, servers and lease loop, and run them until shutdown.

use crate::advisor_shard::AdvisorService;
use crate::config::Config;
use crate::engine::Engine;
use crate::http::{self, Health, HttpState};
use crate::lease::{self, LeaseCtx};
use crate::metrics::Metrics;
use crate::runner::{Faults, Runner};
use anyhow::Context;
use hearts_proto::pb::{
    self, advisor_shard_server::AdvisorShardServer, worker_service_client::WorkerServiceClient,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Run the worker until `shutdown` is cancelled (SIGTERM in `main`; tests
/// cancel it directly, which is why this takes a token rather than
/// listening for signals itself).
pub async fn run(
    cfg: Config,
    engine: Arc<dyn Engine>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let threads = cfg.resolved_threads();
    info!(
        threads,
        requested = cfg.worker_threads,
        "building rayon pool"
    );
    // ONE pool for the whole process, shared by tournament units and advisor
    // requests via Arc (a reference-counted pointer, so clones are cheap).
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("hearts-rayon-{i}"))
            .build()
            .context("building rayon pool")?,
    );

    let metrics = Arc::new(Metrics::new());
    metrics.threads.set(threads as i64);

    let faults = Faults {
        panic_every_n: cfg.fault_panic_every_n,
        slow_ms: cfg.fault_slow_ms,
    };
    if faults.panic_every_n > 0 {
        warn!(
            every_n = faults.panic_every_n,
            "FAULT INJECTION ACTIVE: panicking on every Nth unit"
        );
    }
    if faults.slow_ms > 0 {
        warn!(
            slow_ms = faults.slow_ms,
            "FAULT INJECTION ACTIVE: adding sleep to every unit"
        );
    }

    let settings = cfg.lease_settings();
    // Ready if we spoke to the coordinator within 3 heartbeats (min 30s).
    let health = Arc::new(Health::new(
        (settings.heartbeat * 3).max(Duration::from_secs(30)),
    ));

    let worker_id = cfg.worker_id();
    let info = pb::WorkerInfo {
        worker_id: worker_id.clone(),
        pod_name: worker_id.clone(),
        node_name: cfg.node_name.clone(),
        threads: threads as u32,
    };
    info!(%worker_id, node = %cfg.node_name, coordinator = %cfg.coordinator_grpc_url, "worker starting");

    let channel = lease::connect_lazy(&cfg.coordinator_grpc_url)?;
    let runner = Arc::new(Runner::new(
        engine.clone(),
        pool.clone(),
        metrics.clone(),
        faults,
    ));
    let ctx = Arc::new(LeaseCtx::new(
        WorkerServiceClient::new(channel),
        info,
        runner,
        metrics.clone(),
        health.clone(),
        settings,
    ));

    // Bind the HTTP socket up front so a port clash is a startup error.
    let listener = tokio::net::TcpListener::bind(cfg.metrics_addr)
        .await
        .with_context(|| format!("binding metrics addr {}", cfg.metrics_addr))?;
    info!(addr = %cfg.metrics_addr, "metrics/health HTTP listening");
    // The HTTP server outlives `shutdown` so /metrics works during the grace
    // period; it gets its own token, cancelled once the lease loop is done.
    let http_stop = CancellationToken::new();
    let http_task = {
        let stop = http_stop.clone();
        let app = http::router(HttpState {
            metrics: metrics.clone(),
            health,
        });
        tokio::spawn(async move {
            let r = axum::serve(listener, app)
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await;
            if let Err(e) = r {
                warn!(error = %e, "http server error");
            }
        })
    };

    let advisor = AdvisorService {
        engine,
        pool,
        metrics,
    };
    let grpc_addr = cfg.grpc_addr;
    info!(addr = %grpc_addr, "AdvisorShard gRPC listening");
    let grpc_task = {
        let stop = shutdown.clone();
        tokio::spawn(async move {
            let r = tonic::transport::Server::builder()
                .add_service(AdvisorShardServer::new(advisor))
                .serve_with_shutdown(grpc_addr, async move { stop.cancelled().await })
                .await;
            if let Err(e) = r {
                warn!(error = %e, "gRPC server error");
            }
        })
    };

    lease::run(ctx, shutdown).await;

    http_stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = http_task.await;
        let _ = grpc_task.await;
    })
    .await;
    info!("worker stopped");
    Ok(())
}
