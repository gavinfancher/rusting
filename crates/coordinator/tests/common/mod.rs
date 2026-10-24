//! Shared helpers for the integration tests.
//!
//! Each test starts its own throwaway `postgres:18.4` container on a random
//! host port (testcontainers picks it), so tests are isolated from each
//! other and from other agents' containers. The container is stopped and
//! removed when the `TestDb` is dropped.

#![allow(dead_code)]

use coordinator::{config::Config, db, db::Db, metrics::Metrics, model::Job, state::AppState};
use hearts_core::{JobSpec, SlotStats, UnitStats};
use std::sync::Arc;
use testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::sync::watch;

pub struct TestDb {
    pub db: Db,
    pub metrics: Arc<Metrics>,
    pub pool: sqlx::PgPool,
    pub url: String,
    // Keep the container alive for the test's duration.
    _container: ContainerAsync<Postgres>,
}

pub async fn start_pg() -> (ContainerAsync<Postgres>, String) {
    let container = Postgres::default()
        .with_tag("18.4")
        .start()
        .await
        .expect("start postgres container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("mapped port");
    // testcontainers' postgres module defaults: user/password/db = postgres.
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    (container, url)
}

/// Fresh database with migrations applied, and a `Db` with the given lease
/// length and attempt limit.
pub async fn setup(lease_seconds: u32, max_attempts: i32) -> TestDb {
    let (container, url) = start_pg().await;
    let pool = db::connect_with_retry(&url, 16, 20).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    let metrics = Arc::new(Metrics::new());
    let db = Db::new(pool.clone(), metrics.clone(), lease_seconds, max_attempts);
    TestDb {
        db,
        metrics,
        pool,
        url,
        _container: container,
    }
}

impl TestDb {
    /// An `AppState` (for the HTTP router / gRPC service) over this DB.
    pub fn app_state(&self, mutate: impl FnOnce(&mut Config)) -> (AppState, watch::Sender<bool>) {
        let mut cfg = Config::for_tests(&self.url);
        mutate(&mut cfg);
        let (tx, rx) = watch::channel(false);
        (
            AppState::new(self.db.clone(), self.metrics.clone(), Arc::new(cfg), rx),
            tx,
        )
    }
}

pub fn spec(seed: u64) -> JobSpec {
    JobSpec {
        seed,
        strategies: ["heuristic", "heuristic", "random", "random"].map(|s| s.parse().unwrap()),
        rules: Default::default(),
    }
}

/// Create a job with `units` units of one hand each.
pub async fn make_job(t: &TestDb, units: i64) -> i64 {
    t.db.create_job(Some("test"), &spec(42), units, 1)
        .await
        .expect("create job")
}

/// Stats for a unit of `hands` hands: slot i has i+1 points per hand, wins
/// by slot 0 only. Easy to check sums against.
pub fn stats(hands: u64) -> UnitStats {
    let mut s = UnitStats {
        hands,
        slots: [SlotStats::default(); 4],
    };
    for (i, slot) in s.slots.iter_mut().enumerate() {
        slot.hands = hands;
        slot.points = hands * (i as u64 + 1);
        slot.wins = if i == 0 { hands } else { 0 };
    }
    s
}

pub async fn job(t: &TestDb, id: i64) -> Job {
    t.db.get_job(id).await.unwrap().expect("job exists")
}
