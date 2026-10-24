//! Prometheus metrics (names and meanings fixed by docs/CONTRACTS.md).
//!
//! We keep the metrics in a `Metrics` struct that owns its own `Registry`
//! rather than using process-wide globals. That lets each test build a fresh
//! set and assert on it without tests interfering with one another.
//! Every handle (`IntCounter`, ...) is internally an `Arc` with atomics, so
//! cloning is cheap and updating from any thread is lock-free.
//!
//! No high-cardinality labels: never a unit id, job id or worker id.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

pub struct Metrics {
    pub registry: Registry,
    pub threads: IntGauge,
    pub busy: IntGauge,
    pub hands_simulated: IntCounter,
    pub units_completed: IntCounterVec, // label: result = accepted|duplicate|error
    pub wasted_units: IntCounter,
    pub unit_duration: Histogram,
    pub lease_errors: IntCounterVec, // label: rpc = lease|heartbeat|complete
    pub idle_polls: IntCounter,
    pub advise_samples: IntCounter,
    pub advise_duration: Histogram,
}

impl Metrics {
    pub fn new() -> Metrics {
        let registry = Registry::new();
        // `expect` is acceptable here: these only fail on programmer error
        // (duplicate or invalid metric names) and would fail on first run.
        let reg = |c: Box<dyn prometheus::core::Collector>| {
            registry.register(c).expect("metric registration");
        };

        let threads = IntGauge::new("hearts_worker_threads", "rayon pool size").expect("metric");
        let busy = IntGauge::new("hearts_worker_busy", "1 while computing a unit").expect("metric");
        let hands_simulated = IntCounter::new(
            "hearts_worker_hands_simulated_total",
            "hands simulated (including wasted work)",
        )
        .expect("metric");
        let units_completed = IntCounterVec::new(
            Opts::new(
                "hearts_worker_units_completed_total",
                "units reported to the coordinator, by result",
            ),
            &["result"],
        )
        .expect("metric");
        let wasted_units = IntCounter::new(
            "hearts_worker_wasted_units_total",
            "units finished but rejected, or abandoned after a lost lease",
        )
        .expect("metric");
        let unit_duration = Histogram::with_opts(
            HistogramOpts::new(
                "hearts_worker_unit_duration_seconds",
                "compute time per unit",
            )
            // Units range from milliseconds (tests) to minutes (PIMC).
            .buckets(vec![
                0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
            ]),
        )
        .expect("metric");
        let lease_errors = IntCounterVec::new(
            Opts::new(
                "hearts_worker_lease_errors_total",
                "gRPC failures talking to the coordinator",
            ),
            &["rpc"],
        )
        .expect("metric");
        let idle_polls = IntCounter::new(
            "hearts_worker_idle_polls_total",
            "LeaseWork returned no units",
        )
        .expect("metric");
        let advise_samples = IntCounter::new(
            "hearts_worker_advise_samples_total",
            "samples run for the advisor",
        )
        .expect("metric");
        let advise_duration = Histogram::with_opts(
            HistogramOpts::new("hearts_worker_advise_duration_seconds", "per Simulate call")
                .buckets(vec![
                    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
                ]),
        )
        .expect("metric");

        reg(Box::new(threads.clone()));
        reg(Box::new(busy.clone()));
        reg(Box::new(hands_simulated.clone()));
        reg(Box::new(units_completed.clone()));
        reg(Box::new(wasted_units.clone()));
        reg(Box::new(unit_duration.clone()));
        reg(Box::new(lease_errors.clone()));
        reg(Box::new(idle_polls.clone()));
        reg(Box::new(advise_samples.clone()));
        reg(Box::new(advise_duration.clone()));
        // CPU, RSS, open fds. Only reports on Linux (which is what runs in k8s).
        #[cfg(target_os = "linux")]
        reg(Box::new(
            prometheus::process_collector::ProcessCollector::for_self(),
        ));

        // Touch each label value once so the series exist (at 0) from
        // startup; otherwise `rate()` and dashboards see nothing until the
        // first event.
        for r in ["accepted", "duplicate", "error"] {
            units_completed.with_label_values(&[r]);
        }
        for r in ["lease", "heartbeat", "complete"] {
            lease_errors.with_label_values(&[r]);
        }

        Metrics {
            registry,
            threads,
            busy,
            hands_simulated,
            units_completed,
            wasted_units,
            unit_duration,
            lease_errors,
            idle_polls,
            advise_samples,
            advise_duration,
        }
    }

    /// Render in the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        // Encoding into a Vec cannot fail in practice; fall back to empty.
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
        Self::new()
    }
}
