//! Command-line / environment configuration (names fixed by docs/CONTRACTS.md).
//!
//! `clap` derives a parser from this struct; `env = "..."` lets every flag
//! also be set through an environment variable, which is how Kubernetes
//! configures the pod.

use crate::lease::LeaseSettings;
use clap::Parser;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Parser, Debug, Clone)]
#[command(name = "worker", about = "Hearts simulator compute node")]
pub struct Config {
    #[arg(
        long,
        env = "COORDINATOR_GRPC_URL",
        default_value = "http://localhost:50051"
    )]
    pub coordinator_grpc_url: String,

    /// Size of the rayon pool. 0 = `available_parallelism()` (cgroup-aware).
    #[arg(long, env = "WORKER_THREADS", default_value_t = 0)]
    pub worker_threads: usize,

    #[arg(long, env = "MAX_UNITS_PER_LEASE", default_value_t = 1)]
    pub max_units_per_lease: u32,

    #[arg(long, env = "HEARTBEAT_SECONDS", default_value_t = 10)]
    pub heartbeat_seconds: u64,

    /// Lease the next unit while computing the current one.
    #[arg(long, env = "PREFETCH", default_value_t = false, action = clap::ArgAction::Set)]
    pub prefetch: bool,

    #[arg(long, env = "GRPC_ADDR", default_value = "0.0.0.0:50052")]
    pub grpc_addr: SocketAddr,

    #[arg(long, env = "METRICS_ADDR", default_value = "0.0.0.0:9090")]
    pub metrics_addr: SocketAddr,

    /// Kubernetes downward API. Falls back to the hostname.
    #[arg(long, env = "POD_NAME")]
    pub pod_name: Option<String>,

    #[arg(long, env = "NODE_NAME", default_value = "local")]
    pub node_name: String,

    /// Game day: panic on every Nth unit (0 = off).
    #[arg(long, env = "FAULT_PANIC_EVERY_N", default_value_t = 0)]
    pub fault_panic_every_n: u64,

    /// Game day: extra sleep per unit in ms (0 = off).
    #[arg(long, env = "FAULT_SLOW_MS", default_value_t = 0)]
    pub fault_slow_ms: u64,

    /// How long a SIGTERM lets the in-flight unit finish. Keep this below
    /// the pod's terminationGracePeriodSeconds (60 by default in k8s).
    #[arg(long, env = "SHUTDOWN_GRACE_SECONDS", default_value_t = 45)]
    pub shutdown_grace_seconds: u64,

    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub rust_log: String,

    /// `json` (k8s) or `pretty` (local).
    #[arg(long, env = "LOG_FORMAT", default_value = "json")]
    pub log_format: String,
}

impl Config {
    /// `worker_id = POD_NAME`, defaulting to the machine's hostname.
    pub fn worker_id(&self) -> String {
        match &self.pod_name {
            Some(p) if !p.is_empty() => p.clone(),
            _ => gethostname::gethostname().to_string_lossy().into_owned(),
        }
    }

    /// Resolve `WORKER_THREADS=0` to the real thread count.
    pub fn resolved_threads(&self) -> usize {
        if self.worker_threads > 0 {
            self.worker_threads
        } else {
            // In a container this honours the cgroup CPU limit, which is
            // exactly what we want for sizing a CPU-bound pool.
            std::thread::available_parallelism().map_or(1, |n| n.get())
        }
    }

    pub fn lease_settings(&self) -> LeaseSettings {
        LeaseSettings {
            max_units: self.max_units_per_lease.max(1),
            heartbeat: Duration::from_secs(self.heartbeat_seconds.max(1)),
            prefetch: self.prefetch,
            shutdown_grace: Duration::from_secs(self.shutdown_grace_seconds),
            default_idle_retry: Duration::from_secs(1),
        }
    }
}
