//! Command-line / environment configuration (names are fixed by
//! `docs/CONTRACTS.md`). `clap` with the `env` feature lets every flag also
//! be set through an environment variable, which is how Kubernetes passes
//! settings.

use clap::{Parser, ValueEnum};
use std::net::SocketAddr;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Json,
    Pretty,
}

#[derive(Parser, Clone, Debug)]
#[command(name = "coordinator", about = "Hearts cluster coordinator")]
pub struct Config {
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,
    #[arg(long, env = "DB_POOL_SIZE", default_value_t = 16)]
    pub db_pool_size: u32,
    #[arg(long, env = "HTTP_ADDR", default_value = "0.0.0.0:8080")]
    pub http_addr: SocketAddr,
    #[arg(long, env = "GRPC_ADDR", default_value = "0.0.0.0:50051")]
    pub grpc_addr: SocketAddr,
    /// Lease length; every heartbeat extends it to now + LEASE_SECONDS.
    #[arg(long, env = "LEASE_SECONDS", default_value_t = 30)]
    pub lease_seconds: u32,
    /// A unit that fails or expires this many times becomes `failed`.
    #[arg(long, env = "MAX_ATTEMPTS", default_value_t = 3)]
    pub max_attempts: i32,
    #[arg(long, env = "NO_WORK_RETRY_MS", default_value_t = 1000)]
    pub no_work_retry_ms: u32,
    /// `host:port` that resolves (A records) to every worker's AdvisorShard.
    /// Empty disables the advisor (HTTP 503).
    #[arg(long, env = "ADVISOR_SHARD_DNS", default_value = "")]
    pub advisor_shard_dns: String,
    #[arg(long, env = "ADVISOR_DEFAULT_DEADLINE_MS", default_value_t = 2000)]
    pub advisor_default_deadline_ms: u32,
    #[arg(long, env = "STATS_POLL_SECONDS", default_value_t = 5)]
    pub stats_poll_seconds: u64,
    /// How often the trainer looks at running training runs (milliseconds).
    #[arg(long, env = "TRAINER_TICK_MS", default_value_t = 2000)]
    pub trainer_tick_ms: u64,
    #[arg(long, env = "LOG_FORMAT", value_enum, default_value = "json")]
    pub log_format: LogFormat,
}

impl Config {
    /// A config suitable for tests: only the database URL is needed.
    pub fn for_tests(database_url: &str) -> Config {
        Config::parse_from(["coordinator", "--database-url", database_url])
    }
}
