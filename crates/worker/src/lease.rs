//! The pull loop: lease units, compute them, report results, and keep the
//! leases alive with heartbeats.
//!
//! ## Tasks and shared state
//!
//! Two async tasks cooperate, both on the tokio runtime:
//!
//! * the **lease loop** (`lease_loop`): LeaseWork -> compute -> CompleteWork;
//! * the **heartbeat task** (`heartbeat_loop`): every `HEARTBEAT_SECONDS`
//!   tells the coordinator which units we still hold.
//!
//! They share the **in-flight table**: unit id -> an `Arc<AtomicBool>`
//! *cancel flag*. The lease loop inserts a unit when it is leased and removes
//! it when finished. The heartbeat task reads the ids to send, and if the
//! coordinator says a unit is lost, it raises that unit's flag. The blocking
//! compute thread polls the flag between chunks (see `runner`). An
//! `AtomicBool` is the right tool: one writer/one reader, no locking, and the
//! compute thread can read it without touching async machinery.
//!
//! The table itself is a `std::sync::Mutex<HashMap>`, not a tokio mutex:
//! the lock is only held for a few instructions and never across an `.await`,
//! which is exactly when a plain std mutex is preferable.
//!
//! ## Failure handling
//!
//! * Coordinator unreachable: every RPC failure bumps
//!   `hearts_worker_lease_errors_total{rpc}` and the lease loop sleeps with
//!   exponential backoff + full jitter (100ms doubling to 10s). It never exits.
//!   The gRPC channel is *lazy* (see [`connect_lazy`]) so starting before the
//!   coordinator exists is fine.
//! * Lost lease: the unit is abandoned at the next chunk boundary, no
//!   CompleteWork is sent (the result would be partial), and it counts as
//!   wasted. If the unit had already finished computing we still report it;
//!   the coordinator accepts results from any worker until the unit is done.
//! * Shutdown: see [`run`].

use crate::http::Health;
use crate::metrics::Metrics;
use crate::runner::{Runner, UnitOutcome};
use hearts_proto::pb::{self, worker_service_client::WorkerServiceClient};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Status;
use tonic::transport::{Channel, Endpoint};
use tracing::{error, info, warn};

#[derive(Clone, Debug)]
pub struct LeaseSettings {
    pub max_units: u32,
    pub heartbeat: Duration,
    pub prefetch: bool,
    /// After shutdown is requested, how long the current unit may keep running.
    pub shutdown_grace: Duration,
    /// Used when the coordinator says "no work" without a `retry_after_ms`.
    pub default_idle_retry: Duration,
}

/// Build a gRPC channel that connects on first use and reconnects on its own.
/// `connect_lazy` is what lets the worker boot before the coordinator is up:
/// no connection is attempted here, so there is nothing to fail.
pub fn connect_lazy(url: &str) -> anyhow::Result<Channel> {
    let endpoint = Endpoint::from_shared(url.to_string())?
        .connect_timeout(Duration::from_secs(3))
        // A hung coordinator must not hang us forever.
        .timeout(Duration::from_secs(15))
        .tcp_keepalive(Some(Duration::from_secs(30)));
    Ok(endpoint.connect_lazy())
}

/// Exponential backoff with *full jitter*: the delay is uniform in
/// `[0, min(cap, base * 2^attempt)]`. Jitter stops a fleet of workers from
/// retrying in lockstep (a "thundering herd") when the coordinator returns.
#[derive(Debug, Clone)]
pub struct Backoff {
    attempt: u32,
    base: Duration,
    cap: Duration,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Backoff {
        Backoff {
            attempt: 0,
            base,
            cap,
        }
    }

    /// Upper bound of the jitter window for the current attempt.
    pub fn ceiling(&self) -> Duration {
        let factor = 1u32.checked_shl(self.attempt.min(20)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap)
    }

    pub fn next_delay(&mut self) -> Duration {
        let ceiling_ms = self.ceiling().as_millis() as u64;
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(rand::random_range(0..=ceiling_ms))
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::new(Duration::from_millis(100), Duration::from_secs(10))
    }
}

/// Everything the lease and heartbeat tasks share.
pub struct LeaseCtx {
    pub client: WorkerServiceClient<Channel>,
    pub info: pb::WorkerInfo,
    pub runner: Arc<Runner>,
    pub metrics: Arc<Metrics>,
    pub health: Arc<Health>,
    pub settings: LeaseSettings,
    inflight: Mutex<HashMap<i64, Arc<AtomicBool>>>,
}

impl LeaseCtx {
    pub fn new(
        client: WorkerServiceClient<Channel>,
        info: pb::WorkerInfo,
        runner: Arc<Runner>,
        metrics: Arc<Metrics>,
        health: Arc<Health>,
        settings: LeaseSettings,
    ) -> LeaseCtx {
        LeaseCtx {
            client,
            info,
            runner,
            metrics,
            health,
            settings,
            inflight: Mutex::new(HashMap::new()),
        }
    }

    // A poisoned mutex only means another thread panicked while holding it;
    // the map itself is still valid, so recover the guard.
    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Arc<AtomicBool>>> {
        self.inflight.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register(&self, unit_id: i64) {
        self.table()
            .insert(unit_id, Arc::new(AtomicBool::new(false)));
    }

    fn release(&self, unit_id: i64) {
        self.table().remove(&unit_id);
    }

    fn flag(&self, unit_id: i64) -> Option<Arc<AtomicBool>> {
        self.table().get(&unit_id).cloned()
    }

    /// Ids still worth heart-beating (not already known lost).
    fn live_ids(&self) -> Vec<i64> {
        self.table()
            .iter()
            .filter(|(_, f)| !f.load(Ordering::Relaxed))
            .map(|(id, _)| *id)
            .collect()
    }

    fn mark_lost(&self, unit_id: i64) {
        if let Some(f) = self.table().get(&unit_id) {
            f.store(true, Ordering::Relaxed);
        }
    }

    /// Number of units currently held (for tests / diagnostics).
    pub fn inflight_count(&self) -> usize {
        self.table().len()
    }
}

/// Run until `shutdown` fires and the current unit is handled.
///
/// Shutdown semantics (SIGTERM in production):
/// 1. stop leasing; units leased but not started are abandoned (their leases
///    expire and another worker reclaims them);
/// 2. the unit being computed may take up to `shutdown_grace` to finish and
///    be reported;
/// 3. if it doesn't, raise its cancel flag, stop heartbeating and return
///    (the lease expires and the unit is reclaimed).
pub async fn run(ctx: Arc<LeaseCtx>, shutdown: CancellationToken) {
    let hb = tokio::spawn(heartbeat_loop(ctx.clone()));
    lease_loop(&ctx, &shutdown).await;
    hb.abort(); // no more heartbeats once we're leaving
    info!("lease loop stopped");
}

type LeaseResult = Result<pb::LeaseWorkResponse, Status>;

/// One LeaseWork call. Registers the returned units as in-flight *inside*
/// this function so that a prefetched unit is heart-beaten from the moment
/// it is leased, even while we're still busy with the previous one.
async fn lease_once(ctx: Arc<LeaseCtx>) -> LeaseResult {
    let mut client = ctx.client.clone(); // cheap: clones a handle to the shared channel
    let resp = client
        .lease_work(pb::LeaseWorkRequest {
            worker: Some(ctx.info.clone()),
            max_units: ctx.settings.max_units,
        })
        .await
        .inspect_err(|_| {
            ctx.metrics.lease_errors.with_label_values(&["lease"]).inc();
        })?
        .into_inner();
    ctx.health.mark_ok();
    for u in &resp.units {
        ctx.register(u.unit_id);
    }
    Ok(resp)
}

/// Sleep for `d` unless shutdown fires first. Returns true if shutting down.
async fn sleep_or_shutdown(d: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = shutdown.cancelled() => true,
    }
}

async fn lease_loop(ctx: &Arc<LeaseCtx>, shutdown: &CancellationToken) {
    let mut backoff = Backoff::default();
    // With PREFETCH, the next LeaseWork runs as its own task while we compute.
    let mut prefetched: Option<JoinHandle<LeaseResult>> = None;

    loop {
        if shutdown.is_cancelled() {
            break;
        }

        let result: LeaseResult = match prefetched.take() {
            Some(h) => h
                .await
                .unwrap_or_else(|e| Err(Status::internal(format!("prefetch task failed: {e}")))),
            None => {
                // If shutdown wins the race we drop the in-flight RPC; at
                // worst a unit is leased and never started, and it is
                // reclaimed when its lease expires.
                tokio::select! {
                    r = lease_once(ctx.clone()) => r,
                    _ = shutdown.cancelled() => break,
                }
            }
        };

        let resp = match result {
            Ok(r) => {
                backoff.reset();
                r
            }
            Err(status) => {
                let delay = backoff.next_delay();
                warn!(
                    code = ?status.code(),
                    error = %status.message(),
                    retry_in_ms = delay.as_millis() as u64,
                    "LeaseWork failed; backing off"
                );
                if sleep_or_shutdown(delay, shutdown).await {
                    break;
                }
                continue;
            }
        };

        if resp.units.is_empty() {
            ctx.metrics.idle_polls.inc();
            let wait = if resp.retry_after_ms > 0 {
                Duration::from_millis(u64::from(resp.retry_after_ms))
            } else {
                ctx.settings.default_idle_retry
            };
            if sleep_or_shutdown(wait, shutdown).await {
                break;
            }
            continue;
        }

        let total = resp.units.len();
        let mut stop = false;
        for (i, unit) in resp.units.into_iter().enumerate() {
            if shutdown.is_cancelled() || stop {
                // Not started: drop our claim; the lease will expire.
                ctx.release(unit.unit_id);
                warn!(
                    unit_id = unit.unit_id,
                    "shutdown: abandoning unstarted unit"
                );
                continue;
            }
            // Start fetching the next batch while the *last* unit of this
            // batch computes (earlier units would just delay it).
            if ctx.settings.prefetch && i + 1 == total && prefetched.is_none() {
                prefetched = Some(tokio::spawn(lease_once(ctx.clone())));
            }
            if let Flow::Stop = process_unit(ctx, unit, shutdown).await {
                stop = true;
            }
        }
        if stop {
            break;
        }
    }

    if let Some(h) = prefetched {
        // Units this may have leased are simply left to expire.
        h.abort();
    }
}

enum Flow {
    Continue,
    Stop,
}

async fn process_unit(
    ctx: &Arc<LeaseCtx>,
    unit: pb::WorkUnit,
    shutdown: &CancellationToken,
) -> Flow {
    let unit_id = unit.unit_id;
    let Some(flag) = ctx.flag(unit_id) else {
        // Cannot happen (lease_once registered it), but don't panic over it.
        error!(unit_id, "unit missing from in-flight table");
        return Flow::Continue;
    };
    if flag.load(Ordering::Relaxed) {
        // Heartbeat reported it lost before we even started.
        warn!(unit_id, "lease lost before start; skipping");
        ctx.metrics.wasted_units.inc();
        ctx.release(unit_id);
        return Flow::Continue;
    }

    info!(
        unit_id,
        job_id = unit.job_id,
        hands = unit.hand_end.saturating_sub(unit.hand_start),
        attempt = unit.attempt,
        "computing unit"
    );
    let started = Instant::now();
    let run = ctx.runner.run_unit(&unit, flag.clone());
    tokio::pin!(run);

    // Wait for the unit, but once shutdown is requested only for `grace`.
    let grace = ctx.settings.shutdown_grace;
    let outcome = tokio::select! {
        o = &mut run => Some(o),
        _ = async {
            shutdown.cancelled().await;
            tokio::time::sleep(grace).await;
        } => None,
    };

    let Some(outcome) = outcome else {
        warn!(
            unit_id,
            grace_s = grace.as_secs(),
            "shutdown grace expired; abandoning unit (its lease will expire and be reclaimed)"
        );
        flag.store(true, Ordering::Relaxed); // makes the compute thread stop at the next chunk
        ctx.metrics.wasted_units.inc();
        ctx.release(unit_id);
        return Flow::Stop;
    };

    let elapsed = started.elapsed();
    match outcome {
        UnitOutcome::Done(stats) => {
            ctx.metrics.unit_duration.observe(elapsed.as_secs_f64());
            let req = pb::CompleteWorkRequest {
                worker_id: ctx.info.worker_id.clone(),
                unit_id,
                outcome: Some(pb::complete_work_request::Outcome::Stats(
                    pb::UnitStats::from(&stats),
                )),
                compute_ms: elapsed.as_millis() as u64,
            };
            match complete(ctx, req).await {
                Some(resp) if resp.accepted => {
                    ctx.metrics
                        .units_completed
                        .with_label_values(&["accepted"])
                        .inc();
                    info!(unit_id, ms = elapsed.as_millis() as u64, "unit accepted");
                }
                Some(_) => {
                    ctx.metrics
                        .units_completed
                        .with_label_values(&["duplicate"])
                        .inc();
                    ctx.metrics.wasted_units.inc();
                    warn!(unit_id, "result rejected (unit already done): wasted work");
                }
                None => {
                    ctx.metrics.wasted_units.inc();
                    error!(unit_id, "could not report result; giving up on this unit");
                }
            }
        }
        UnitOutcome::Cancelled => {
            warn!(
                unit_id,
                "unit abandoned mid-way (lease lost); no result sent"
            );
            ctx.metrics.wasted_units.inc();
        }
        UnitOutcome::Failed(msg) => {
            error!(unit_id, error = %msg, "unit failed; reporting error to coordinator");
            let req = pb::CompleteWorkRequest {
                worker_id: ctx.info.worker_id.clone(),
                unit_id,
                outcome: Some(pb::complete_work_request::Outcome::Error(msg)),
                compute_ms: elapsed.as_millis() as u64,
            };
            let _ = complete(ctx, req).await;
            ctx.metrics
                .units_completed
                .with_label_values(&["error"])
                .inc();
        }
    }
    ctx.release(unit_id);
    Flow::Continue
}

/// CompleteWork with a few retries. The unit stays in the in-flight table
/// meanwhile, so heartbeats keep its lease alive while the coordinator is
/// briefly unreachable. Returns None if every attempt failed.
async fn complete(
    ctx: &Arc<LeaseCtx>,
    req: pb::CompleteWorkRequest,
) -> Option<pb::CompleteWorkResponse> {
    const ATTEMPTS: u32 = 6;
    let mut backoff = Backoff::new(Duration::from_millis(200), Duration::from_secs(5));
    for attempt in 1..=ATTEMPTS {
        let mut client = ctx.client.clone();
        match client.complete_work(req.clone()).await {
            Ok(r) => {
                ctx.health.mark_ok();
                return Some(r.into_inner());
            }
            Err(status) => {
                ctx.metrics
                    .lease_errors
                    .with_label_values(&["complete"])
                    .inc();
                let delay = backoff.next_delay();
                warn!(
                    attempt,
                    code = ?status.code(),
                    error = %status.message(),
                    "CompleteWork failed"
                );
                if attempt < ATTEMPTS {
                    // Deliberately NOT cut short by shutdown: reporting a
                    // finished unit is the point of the shutdown grace period.
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    None
}

async fn heartbeat_loop(ctx: Arc<LeaseCtx>) {
    let period = ctx.settings.heartbeat;
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    // If a tick is late (e.g. slow RPC) don't fire a burst to catch up.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let ids = ctx.live_ids();
        if ids.is_empty() {
            continue;
        }
        let mut client = ctx.client.clone();
        let req = pb::HeartbeatRequest {
            worker_id: ctx.info.worker_id.clone(),
            unit_ids: ids,
        };
        match client.heartbeat(req).await {
            Ok(resp) => {
                ctx.health.mark_ok();
                for id in resp.into_inner().lost_unit_ids {
                    warn!(unit_id = id, "coordinator says lease lost; cancelling unit");
                    ctx.mark_lost(id);
                }
            }
            Err(status) => {
                ctx.metrics
                    .lease_errors
                    .with_label_values(&["heartbeat"])
                    .inc();
                warn!(code = ?status.code(), error = %status.message(), "Heartbeat failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps_and_resets() {
        let mut b = Backoff::default();
        assert_eq!(b.ceiling(), Duration::from_millis(100));
        for _ in 0..3 {
            b.next_delay();
        }
        assert_eq!(b.ceiling(), Duration::from_millis(800));
        for _ in 0..30 {
            let d = b.next_delay();
            assert!(d <= Duration::from_secs(10));
        }
        assert_eq!(b.ceiling(), Duration::from_secs(10));
        b.reset();
        assert_eq!(b.ceiling(), Duration::from_millis(100));
    }
}
