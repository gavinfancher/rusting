//! The `WorkerService` gRPC server: the thin protocol layer between workers
//! and [`crate::db::Db`]. All the interesting logic (leases, idempotency,
//! job completion) is in SQL; this file validates input, converts types and
//! records handler timings.

use crate::{db::Db, db::DbError, metrics::Metrics};
use hearts_core::UnitStats;
use hearts_proto::pb::{
    self,
    worker_service_server::{WorkerService, WorkerServiceServer},
};
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// Most units one LeaseWork call may return (protects the DB from a worker
/// asking for a million).
const MAX_UNITS_PER_REQUEST: u32 = 64;

pub struct WorkerSvc {
    db: Db,
    metrics: Arc<Metrics>,
    no_work_retry_ms: u32,
}

impl WorkerSvc {
    pub fn new(db: Db, metrics: Arc<Metrics>, no_work_retry_ms: u32) -> WorkerSvc {
        WorkerSvc {
            db,
            metrics,
            no_work_retry_ms,
        }
    }

    pub fn into_server(self) -> WorkerServiceServer<WorkerSvc> {
        WorkerServiceServer::new(self)
    }

    fn timer(&self, method: &str) -> prometheus::HistogramTimer {
        self.metrics
            .grpc_duration
            .with_label_values(&[method])
            .start_timer()
    }
}

/// Database failures become `UNAVAILABLE`, which well-behaved gRPC clients
/// treat as "retry later". We log the detail but don't leak it to clients.
fn db_status(e: DbError) -> Status {
    tracing::error!(error = %e, "database error in gRPC handler");
    match e {
        DbError::BadSpec(_) => Status::internal("corrupt job spec"),
        DbError::Sql(_) => Status::unavailable("database error"),
    }
}

#[tonic::async_trait]
impl WorkerService for WorkerSvc {
    async fn lease_work(
        &self,
        request: Request<pb::LeaseWorkRequest>,
    ) -> Result<Response<pb::LeaseWorkResponse>, Status> {
        let _t = self.timer("LeaseWork");
        let req = request.into_inner();
        let worker = req
            .worker
            .ok_or_else(|| Status::invalid_argument("worker is required"))?;
        if worker.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker.worker_id is required"));
        }
        // Leasing also registers the worker / refreshes `last_seen`.
        self.db
            .upsert_worker(
                &worker.worker_id,
                &worker.pod_name,
                &worker.node_name,
                i32::try_from(worker.threads).unwrap_or(i32::MAX),
            )
            .await
            .map_err(db_status)?;

        let max = req.max_units.clamp(1, MAX_UNITS_PER_REQUEST);
        let leased = self
            .db
            .lease(&worker.worker_id, max)
            .await
            .map_err(db_status)?;

        let units: Vec<pb::WorkUnit> = leased
            .iter()
            .map(|u| pb::WorkUnit {
                unit_id: u.unit_id,
                job_id: u.job_id,
                hand_start: u.hand_start,
                hand_end: u.hand_end,
                spec: Some(pb::JobSpec::from(&u.spec)),
                lease_expires_at_unix_ms: u.lease_expires_at.timestamp_millis(),
                attempt: u.attempt.max(0) as u32,
            })
            .collect();
        let retry_after_ms = if units.is_empty() {
            self.no_work_retry_ms
        } else {
            0
        };
        Ok(Response::new(pb::LeaseWorkResponse {
            units,
            retry_after_ms,
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<pb::HeartbeatRequest>,
    ) -> Result<Response<pb::HeartbeatResponse>, Status> {
        let _t = self.timer("Heartbeat");
        let req = request.into_inner();
        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }
        self.db
            .touch_worker(&req.worker_id)
            .await
            .map_err(db_status)?;
        let (held, lost) = self
            .db
            .heartbeat(&req.worker_id, &req.unit_ids)
            .await
            .map_err(db_status)?;
        let expiry = held
            .iter()
            .map(|(_, t)| t.timestamp_millis())
            .max()
            .unwrap_or(0);
        Ok(Response::new(pb::HeartbeatResponse {
            lost_unit_ids: lost,
            lease_expires_at_unix_ms: expiry,
        }))
    }

    async fn complete_work(
        &self,
        request: Request<pb::CompleteWorkRequest>,
    ) -> Result<Response<pb::CompleteWorkResponse>, Status> {
        let _t = self.timer("CompleteWork");
        let req = request.into_inner();
        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }
        let outcome = match req.outcome {
            Some(pb::complete_work_request::Outcome::Stats(stats)) => {
                // Validates that there are exactly 4 slots.
                let stats = UnitStats::try_from(&stats)?;
                self.db
                    .complete_ok(&req.worker_id, req.unit_id, &stats, req.compute_ms)
                    .await
            }
            Some(pb::complete_work_request::Outcome::Error(msg)) => {
                self.db
                    .complete_err(&req.worker_id, req.unit_id, &msg)
                    .await
            }
            None => {
                return Err(Status::invalid_argument(
                    "outcome (stats or error) is required",
                ));
            }
        }
        .map_err(db_status)?;
        Ok(Response::new(pb::CompleteWorkResponse {
            accepted: outcome.accepted,
            job_finished: outcome.job_finished.is_some(),
        }))
    }
}
