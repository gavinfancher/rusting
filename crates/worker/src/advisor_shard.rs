//! gRPC `AdvisorShard` server: one slice of a distributed advisor query.
//!
//! The coordinator splits the requested samples across all worker pods and
//! merges. Here we just run our share.
//!
//! The advisor and the tournament loop deliberately share ONE rayon pool, so
//! they compete for the same CPUs. A busy worker answers advisor queries more
//! slowly (or hits the deadline and returns fewer samples), which makes a
//! good experiment on latency vs. throughput under contention.
//!
//! As in `runner`, the CPU work runs in `spawn_blocking` + `pool.install` so
//! the async runtime stays responsive.

use crate::engine::Engine;
use crate::metrics::Metrics;
use crate::runner::panic_message;
use hearts_core::PlayerView;
use hearts_proto::pb::{self, advisor_shard_server::AdvisorShard};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tonic::{Request, Response, Status};

pub struct AdvisorService {
    pub engine: Arc<dyn Engine>,
    pub pool: Arc<rayon::ThreadPool>,
    pub metrics: Arc<Metrics>,
}

#[tonic::async_trait]
impl AdvisorShard for AdvisorService {
    async fn simulate(
        &self,
        request: Request<pb::SimulateRequest>,
    ) -> Result<Response<pb::SimulateResponse>, Status> {
        let started = Instant::now();
        let req = request.into_inner();
        let view_pb = req
            .view
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing view"))?;
        // ConvertError -> Status::invalid_argument via `From`, so `?` works.
        let view = PlayerView::try_from(view_pb)?;

        // The deadline counts from when the request arrived, not from when a
        // thread frees up. deadline_ms == 0 means "no deadline".
        let deadline = (req.deadline_ms > 0)
            .then(|| started + Duration::from_millis(u64::from(req.deadline_ms)));
        let (samples, seed) = (req.samples, req.seed);

        let engine = self.engine.clone();
        let pool = self.pool.clone();
        let joined = tokio::task::spawn_blocking(move || {
            catch_unwind(AssertUnwindSafe(|| {
                pool.install(|| engine.simulate(&view, samples, seed, deadline))
            }))
        })
        .await;

        let totals = match joined {
            Ok(Ok(t)) => t,
            Ok(Err(p)) => {
                tracing::error!(panic = %panic_message(&p), "advisor simulate panicked");
                return Err(Status::internal("simulation panicked"));
            }
            Err(e) => return Err(Status::internal(format!("blocking task failed: {e}"))),
        };

        self.metrics
            .advise_samples
            .inc_by(u64::from(totals.samples_done));
        self.metrics
            .advise_duration
            .observe(started.elapsed().as_secs_f64());
        Ok(Response::new(pb::SimulateResponse::from(&totals)))
    }
}
