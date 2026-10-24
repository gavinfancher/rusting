//! Prometheus metrics. Names, types and labels come from `docs/CONTRACTS.md`.
//!
//! We use the `prometheus` crate with one explicit [`Registry`] (not the
//! global default) so each test gets isolated counters. No label ever holds a
//! job, unit or worker id: that would create unbounded time series.

use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry, TextEncoder,
};

/// Buckets for fast RPC / SQL work: 0.5 ms .. 5 s.
const FAST: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];
/// Buckets for advisor calls: 10 ms .. 10 s.
const SLOW: &[f64] = &[0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0];

pub struct Metrics {
    pub registry: Registry,
    pub units_leased: IntCounter,
    pub units_reclaimed: IntCounter,
    pub units_completed: IntCounter,
    pub units_duplicate: IntCounter,
    pub units_errored: IntCounter,
    pub units_failed: IntCounter,
    pub units: IntGaugeVec,
    pub hands_completed: IntCounter,
    pub jobs_created: IntCounter,
    pub jobs_finished: IntCounterVec,
    pub grpc_duration: HistogramVec,
    pub db_duration: HistogramVec,
    pub db_pool: IntGaugeVec,
    pub workers_active: IntGauge,
    pub advise_duration: Histogram,
    pub advise_shard_duration: Histogram,
    pub advise_shard_errors: IntCounterVec,
    pub advise_partial: IntCounter,
    pub http_requests: IntCounterVec,
}

impl Metrics {
    pub fn new() -> Metrics {
        let registry = Registry::new();
        // Registration only fails on a duplicate name or invalid definition,
        // both programming errors that should surface immediately at
        // startup, so `expect` is appropriate here.
        macro_rules! reg {
            ($m:expr) => {{
                let m = $m;
                registry
                    .register(Box::new(m.clone()))
                    .expect("metric registration (duplicate name?)");
                m
            }};
        }
        let counter =
            |name: &str, help: &str| IntCounter::with_opts(Opts::new(name, help)).expect("counter");
        let hist = |name: &str, help: &str, b: &[f64]| {
            Histogram::with_opts(HistogramOpts::new(name, help).buckets(b.to_vec()))
                .expect("histogram")
        };
        let hist_vec = |name: &str, help: &str, label: &str, b: &[f64]| {
            HistogramVec::new(HistogramOpts::new(name, help).buckets(b.to_vec()), &[label])
                .expect("histogram vec")
        };
        let gauge_vec = |name: &str, help: &str, label: &str| {
            IntGaugeVec::new(Opts::new(name, help), &[label]).expect("gauge vec")
        };
        let counter_vec = |name: &str, help: &str, labels: &[&str]| {
            IntCounterVec::new(Opts::new(name, help), labels).expect("counter vec")
        };

        let m = Metrics {
            units_leased: reg!(counter("hearts_units_leased_total", "Units handed out")),
            units_reclaimed: reg!(counter(
                "hearts_units_reclaimed_total",
                "Leased units whose previous lease had expired"
            )),
            units_completed: reg!(counter(
                "hearts_units_completed_total",
                "Accepted completions"
            )),
            units_duplicate: reg!(counter(
                "hearts_units_duplicate_total",
                "Completions rejected because the unit was already done"
            )),
            units_errored: reg!(counter(
                "hearts_units_errored_total",
                "CompleteWork calls carrying an error"
            )),
            units_failed: reg!(counter(
                "hearts_units_failed_total",
                "Units moved to failed"
            )),
            units: reg!(gauge_vec(
                "hearts_units",
                "Current units per state",
                "state"
            )),
            hands_completed: reg!(counter(
                "hearts_hands_completed_total",
                "Hands in accepted completions"
            )),
            jobs_created: reg!(counter("hearts_jobs_created_total", "Jobs created")),
            jobs_finished: reg!(counter_vec(
                "hearts_jobs_finished_total",
                "Jobs finished",
                &["status"]
            )),
            grpc_duration: reg!(hist_vec(
                "hearts_grpc_request_duration_seconds",
                "WorkerService handler time",
                "method",
                FAST
            )),
            db_duration: reg!(hist_vec(
                "hearts_db_query_duration_seconds",
                "Database query time",
                "query",
                FAST
            )),
            db_pool: reg!(gauge_vec(
                "hearts_db_pool_connections",
                "DB pool connections",
                "state"
            )),
            workers_active: reg!(
                IntGauge::new("hearts_workers_active", "Workers seen within 30s").expect("gauge")
            ),
            advise_duration: reg!(hist(
                "hearts_advise_duration_seconds",
                "End-to-end /api/advise latency",
                SLOW
            )),
            advise_shard_duration: reg!(hist(
                "hearts_advise_shard_duration_seconds",
                "Per-shard Simulate call latency",
                SLOW
            )),
            advise_shard_errors: reg!(counter_vec(
                "hearts_advise_shard_errors_total",
                "Shard call failures",
                &["reason"]
            )),
            advise_partial: reg!(counter(
                "hearts_advise_partial_total",
                "Partial advisor answers"
            )),
            http_requests: reg!(counter_vec(
                "hearts_http_requests_total",
                "HTTP requests",
                &["route", "status"]
            )),
            registry: registry.clone(),
        };

        // Default process metrics (CPU, RSS, fds). The crate only implements
        // them on Linux; elsewhere we skip the collector.
        #[cfg(target_os = "linux")]
        {
            let _ = registry.register(Box::new(
                prometheus::process_collector::ProcessCollector::for_self(),
            ));
        }

        // Make labelled series visible (as 0) before the first event so
        // dashboards and `rate()` don't have gaps.
        for s in ["pending", "leased", "done", "failed", "cancelled"] {
            m.units.with_label_values(&[s]).set(0);
        }
        for s in ["idle", "in_use"] {
            m.db_pool.with_label_values(&[s]).set(0);
        }
        for s in ["timeout", "error", "connect"] {
            m.advise_shard_errors.with_label_values(&[s]);
        }
        m
    }

    /// Render in the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        if TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .is_err()
        {
            return String::new();
        }
        String::from_utf8(buf).unwrap_or_default()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics::new()
    }
}
