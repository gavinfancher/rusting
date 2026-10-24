//! Training (Level 1 learning) against a real Postgres 18.4.
//!
//! The learning maths (`hearts_core::learn`) is implemented elsewhere, so
//! these tests inject [`FakeCem`], a tiny deterministic stand-in, and test the
//! *plumbing*: jobs created per candidate, fitness read back from job
//! standings, state progression, champion, validation, crash-resume, the
//! advisory lock, cancel and the HTTP surface. No workers run here; the
//! tests play the workers' part by leasing units and completing them with
//! fabricated stats, so every fitness is known in advance.
//!
//! Fabricated fitness: a `linear:` candidate with `w0` scores `6.0 - w0/10`
//! points per hand (bigger first weight = better), the plain heuristic scores
//! 6.5. FakeCem's `update` adopts the best candidate, so the champion's w0
//! grows by 3 per generation with population 4 (candidate idx i has w0 =
//! mean_w0 + i).

mod common;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use common::*;
use coordinator::{
    db::{self, Db},
    http,
    state::AppState,
    trainer::{Cem, Step, Trainer},
};
use hearts_core::{
    NUM_WEIGHTS, SlotStats, StrategyKind, UnitStats, Weights,
    learn::{self, CemConfig, CemState, Init},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// The test double
// ---------------------------------------------------------------------------

struct FakeCem;

impl Cem for FakeCem {
    fn initial_state(&self, _init: Init, sigma0: f64) -> CemState {
        CemState {
            generation: 0,
            mean: vec![0.0; NUM_WEIGHTS],
            sigma: vec![sigma0; NUM_WEIGHTS],
        }
    }
    /// Candidate i = mean with w0 += i, and w1 set from the seed (so a test
    /// can see the generation seed reach the sampler).
    fn sample(&self, state: &CemState, population: usize, seed: u64) -> Vec<Weights> {
        (0..population)
            .map(|i| {
                let mut w = self.to_weights(&state.mean);
                w.0[0] += i as i32;
                if i > 0 {
                    w.0[1] = (seed % 1000) as i32;
                }
                w
            })
            .collect()
    }
    /// Adopt the best (lowest fitness, first on ties) candidate as the mean.
    fn update(&self, state: &CemState, _cfg: &CemConfig, scored: &[(Weights, f64)]) -> CemState {
        let best = scored
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.1.total_cmp(&b.1.1).then(a.0.cmp(&b.0)))
            .map(|(_, (w, _))| *w)
            .unwrap();
        CemState {
            generation: state.generation + 1,
            mean: best.0.iter().map(|&x| f64::from(x)).collect(),
            sigma: state.sigma.clone(),
        }
    }
    fn to_weights(&self, v: &[f64]) -> Weights {
        let mut w = [0i32; NUM_WEIGHTS];
        for (o, x) in w.iter_mut().zip(v) {
            *o = x.round() as i32;
        }
        Weights(w)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Env {
    t: TestDb,
    state: AppState,
    trainer: Trainer,
}

async fn env() -> Env {
    let t = setup(30, 3).await;
    let (state, _stop) = t.app_state(|_| {});
    let state = state.with_cem(Arc::new(FakeCem));
    let trainer = Trainer::new(&state);
    Env { t, state, trainer }
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A small run: 4 candidates, 1000 hands each in units of 500 (2 units/job).
fn small_run(generations: u32) -> Value {
    json!({
        "name": "t", "population": 4, "elites": 2, "generations": generations,
        "hands_per_candidate": 1000, "unit_size": 500, "validation_hands": 1000,
        "seed": 7
    })
}

async fn create_run(e: &Env, body: Value) -> i64 {
    let app = http::router(e.state.clone());
    let (status, run) = call(&app, "POST", "/api/training", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    run["id"].as_i64().unwrap()
}

async fn get_run(e: &Env, id: i64) -> Value {
    let app = http::router(e.state.clone());
    let (status, run) = call(&app, "GET", &format!("/api/training/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    run
}

/// Play the workers: lease up to `max_units` units and complete them with
/// the fabricated fitness described in the module docs. Returns how many.
async fn work(db: &Db, max_units: u32) -> usize {
    let units = db.lease("test-worker", max_units).await.unwrap();
    for u in &units {
        let hands = u.hand_end - u.hand_start;
        // points per hand x 10
        let ppm10: u64 = match u.spec.strategies[0] {
            StrategyKind::Linear(w) => (60 - w.0[0]) as u64,
            _ => 65,
        };
        let mut s = UnitStats {
            hands,
            slots: [SlotStats::default(); 4],
        };
        for slot in s.slots.iter_mut() {
            slot.hands = hands;
        }
        s.slots[0].points = hands * ppm10 / 10;
        let out = db
            .complete_ok("test-worker", u.unit_id, &s, 1)
            .await
            .unwrap();
        assert!(out.accepted);
    }
    units.len()
}

async fn work_all(db: &Db) {
    while work(db, 50).await > 0 {}
}

async fn count(t: &TestDb, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&t.pool).await.unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_applies_defaults_and_validates() {
    let e = env().await;
    let app = http::router(e.state.clone());

    let (status, run) = call(&app, "POST", "/api/training", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    let cfg = &run["config"];
    assert_eq!(cfg["population"], 24);
    assert_eq!(cfg["elites"], 6);
    assert_eq!(cfg["generations"], 30);
    assert_eq!(cfg["hands_per_candidate"], 40_000);
    assert_eq!(cfg["unit_size"], 5000);
    assert_eq!(
        cfg["opponents"],
        json!(["heuristic", "heuristic", "heuristic"])
    );
    assert_eq!(cfg["init"], "heuristic");
    assert_eq!(cfg["sigma0"], 20.0);
    assert_eq!(cfg["smoothing"], 0.7);
    assert_eq!(cfg["min_sigma"], 2.0);
    assert_eq!(cfg["validation_hands"], 200_000);
    assert_eq!(cfg["rules"]["no_points_first_trick"], true);
    assert!(cfg["seed"].as_u64().unwrap() < (1 << 53));
    assert_eq!(run["status"], "running");
    assert_eq!(run["generation"], 0);
    assert_eq!(run["generations_total"], 30);
    assert!(run["current"].is_null() && run["champion"].is_null() && run["validation"].is_null());
    assert_eq!(run["generations"], json!([]));

    // Small populations get a legal default for `elites`.
    let (status, run) = call(
        &app,
        "POST",
        "/api/training",
        Some(json!({"population": 4})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    assert_eq!(run["config"]["elites"], 3);

    // The list endpoint omits the per-generation curve.
    let (_, list) = call(&app, "GET", "/api/training", None).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(list[0].get("generations").is_none());
    assert!(
        list[0]["id"].as_i64() > list[1]["id"].as_i64(),
        "newest first"
    );

    for (bad, why) in [
        (json!({"population": 3}), "population"),
        (json!({"population": 257}), "population"),
        (json!({"population": 8, "elites": 8}), "elites"),
        (json!({"elites": 0}), "elites"),
        (json!({"generations": 0}), "generations"),
        (json!({"generations": 1001}), "generations"),
        (json!({"hands_per_candidate": 99}), "hands_per_candidate"),
        (
            json!({"hands_per_candidate": 10_000_001}),
            "hands_per_candidate",
        ),
        (
            json!({"opponents": ["heuristic", "heuristic"]}),
            "opponents",
        ),
        (
            json!({"opponents": ["heuristic", "heuristic", "nope"]}),
            "strategy",
        ),
        (json!({"init": "bogus"}), "init"),
        (json!({"smoothing": 0.0}), "smoothing"),
        (json!({"sigma0": -1.0}), "sigma0"),
        (json!({"seed": 9_007_199_254_740_993u64}), "seed"),
        (json!({"unit_size": 0}), "unit_size"),
        (
            json!({"opponents": ["champion:99", "heuristic", "heuristic"]}),
            "champion",
        ),
    ] {
        let (status, body) = call(&app, "POST", "/api/training", Some(bad.clone())).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{bad} should be rejected ({why})"
        );
        assert!(body["error"].is_string());
    }

    let (status, _) = call(&app, "GET", "/api/training/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, "POST", "/api/training/999/cancel", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn generation_has_one_tagged_job_per_candidate() {
    let e = env().await;
    let id = create_run(&e, small_run(2)).await;

    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Created(0));
    assert_eq!(
        e.trainer.advance(id).await.unwrap(),
        Step::Waiting,
        "idempotent"
    );

    let rows: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT name, training_run_id, num_hands FROM jobs ORDER BY id")
            .fetch_all(&e.t.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 4, "population jobs, no duplicates");
    for (i, (name, run, hands)) in rows.iter().enumerate() {
        assert_eq!(name, &format!("train#{id} g0 c{i}"));
        assert_eq!((*run, *hands), (id, 1000));
    }

    let seed_g = learn::generation_seed(7, 0);
    let specs: Vec<(i32, Value)> = sqlx::query_as(
        "SELECT c.idx, j.spec FROM training_candidates c JOIN jobs j ON j.id = c.job_id
          WHERE c.run_id = $1 AND c.generation = 0 ORDER BY c.idx",
    )
    .bind(id)
    .fetch_all(&e.t.pool)
    .await
    .unwrap();
    assert_eq!(specs.len(), 4);
    for (idx, spec) in &specs {
        let strategies = spec["strategies"].as_array().unwrap();
        assert!(
            strategies[0].as_str().unwrap().starts_with("linear:"),
            "slot 0 is the candidate"
        );
        assert_eq!(
            &strategies[1..],
            &[json!("heuristic"), json!("heuristic"), json!("heuristic")]
        );
        // All candidates play the same hands: the generation seed.
        assert_eq!(spec["seed"].as_u64().unwrap(), seed_g, "candidate {idx}");
    }
    // Candidate 0 is the mean (w0 = 0), candidate 2 has w0 = 2.
    assert!(
        specs[0].1["strategies"][0]
            .as_str()
            .unwrap()
            .starts_with("linear:0,0,")
    );
    assert!(
        specs[2].1["strategies"][0]
            .as_str()
            .unwrap()
            .starts_with("linear:2,")
    );

    assert_eq!(
        count(&e.t, "SELECT count(*) FROM training_generations").await,
        1
    );
    // The sampler got the generation seed (FakeCem writes seed % 1000 into w1).
    let w1 = (seed_g % 1000) as i32;
    assert!(
        specs[1].1["strategies"][0]
            .as_str()
            .unwrap()
            .starts_with(&format!("linear:1,{w1},")),
        "{}",
        specs[1].1["strategies"][0]
    );

    // Progress view while jobs are pending.
    let run = get_run(&e, id).await;
    assert_eq!(run["current"]["generation"], 0);
    assert_eq!(run["current"]["candidates_total"], 4);
    assert_eq!(run["current"]["candidates_done"], 0);
    assert_eq!(run["current"]["hands_total"], 4000);
    assert_eq!(e.state.metrics.jobs_created.get(), 4);
}

#[tokio::test]
async fn full_run_updates_state_champion_and_validates() {
    let e = env().await;
    let id = create_run(&e, small_run(2)).await;
    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Created(0));

    // Half the work: nothing is judged yet.
    work(&e.state.db, 2).await;
    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Waiting);
    let run = get_run(&e, id).await;
    assert_eq!(run["current"]["hands_done"], 1000);

    // Finish generation 0: the next tick judges it and queues generation 1.
    work_all(&e.state.db).await;
    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Finished(0));
    let run = get_run(&e, id).await;
    assert_eq!(run["generation"], 1);
    assert_eq!(run["status"], "running");
    let g0 = &run["generations"][0];
    assert_eq!(g0["generation"], 0);
    assert_eq!(g0["mean_fitness"], 6.0); // candidate 0: w0 = 0
    assert_eq!(g0["best_fitness"], 5.7); // candidate 3: w0 = 3
    assert_eq!(
        g0["elite_mean_fitness"].as_f64().unwrap(),
        (5.7 + 5.8) / 2.0
    );
    assert_eq!(g0["sigma_mean"], 20.0);
    assert!(g0["finished_at"].is_string() && g0["elapsed_ms"].is_i64());
    let cand_fit: Vec<f64> = sqlx::query_scalar(
        "SELECT fitness FROM training_candidates WHERE run_id = $1 AND generation = 0 ORDER BY idx",
    )
    .bind(id)
    .fetch_all(&e.t.pool)
    .await
    .unwrap();
    assert_eq!(cand_fit, vec![6.0, 5.9, 5.8, 5.7]);
    // champion = the new mean (w0 = 3); its fitness = candidate 0's measurement.
    assert!(
        run["champion"]["strategy"]
            .as_str()
            .unwrap()
            .starts_with("linear:3,")
    );
    assert_eq!(run["champion"]["fitness"], 6.0);
    assert_eq!(
        run["champion"]["weights"].as_array().unwrap().len(),
        NUM_WEIGHTS
    );
    assert_eq!(
        run["champion"]["weights"][0],
        json!({"feature": "play.leading", "weight": 3})
    );
    // Generation 1 was queued in the same step.
    assert_eq!(run["current"]["generation"], 1);
    assert_eq!(count(&e.t, "SELECT count(*) FROM jobs").await, 8);

    // Finish generation 1: the last one, so validation starts.
    work_all(&e.state.db).await;
    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Finished(1));
    let run = get_run(&e, id).await;
    assert_eq!(run["status"], "validating");
    assert_eq!(run["generation"], 2);
    assert!(run["current"].is_null());
    assert!(run["validation"].is_null(), "jobs not finished yet");
    assert_eq!(run["generations"].as_array().unwrap().len(), 2);
    assert_eq!(run["generations"][1]["mean_fitness"], 5.7); // the w0=3 champion, measured
    assert!(
        run["champion"]["strategy"]
            .as_str()
            .unwrap()
            .starts_with("linear:6,")
    );

    // The two validation jobs: champion and heuristic baseline, same fresh seed.
    let vjobs: Vec<(String, Value, i64)> = sqlx::query_as(
        "SELECT name, spec, num_hands FROM jobs WHERE name LIKE '%validation%' ORDER BY id",
    )
    .fetch_all(&e.t.pool)
    .await
    .unwrap();
    assert_eq!(vjobs.len(), 2);
    assert_eq!(vjobs[0].1["seed"], vjobs[1].1["seed"]);
    assert_eq!(
        vjobs[0].1["seed"].as_u64().unwrap(),
        hearts_core::seed::child_seed(7, 1_000_000)
    );
    assert!(
        vjobs[0].1["strategies"][0]
            .as_str()
            .unwrap()
            .starts_with("linear:6,")
    );
    assert_eq!(vjobs[1].1["strategies"][0], "heuristic");
    assert_eq!(vjobs[0].2, 1000);

    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Waiting);
    work_all(&e.state.db).await;
    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Completed(true));

    let run = get_run(&e, id).await;
    assert_eq!(run["status"], "done");
    assert!(run["finished_at"].is_string());
    let v = &run["validation"];
    assert_eq!(v["hands"], 1000);
    assert_eq!(v["champion_fitness"], 5.4); // w0 = 6
    assert_eq!(v["baseline_fitness"], 6.5);
    assert!(v["champion_job_id"].is_i64() && v["baseline_job_id"].is_i64());

    // Finished runs are not visited again.
    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Skipped);

    // Metrics.
    e.trainer.tick().await;
    let m = &e.state.metrics;
    assert_eq!(m.training_generations.get(), 2);
    assert_eq!(m.training_generation_duration.get_sample_count(), 2);
    assert_eq!(m.training_runs_active.get(), 0);
    assert_eq!(m.training_latest_fitness.get(), 5.7);
    assert_eq!(m.training_latest_best_fitness.get(), 5.4);
    assert_eq!(m.jobs_created.get(), 4 + 4 + 2);
}

#[tokio::test]
async fn resumes_after_restart_and_rolls_back_half_finished_steps() {
    let e = env().await;
    let id = create_run(&e, small_run(2)).await;

    // A "crash" in the middle of creating generation 0: the transaction is
    // dropped without COMMIT, so Postgres discards everything it did.
    {
        let (mut tx, run) = e
            .state
            .db
            .begin_training_step(id)
            .await
            .unwrap()
            .expect("lock");
        let _ = run;
        let spec = common::spec(1);
        Db::tx_create_generation(
            &mut tx,
            id,
            0,
            1,
            &[(Weights([0; NUM_WEIGHTS]), spec)],
            1000,
            500,
        )
        .await
        .unwrap();
        // `tx` dropped here => rollback.
    }
    assert_eq!(count(&e.t, "SELECT count(*) FROM jobs").await, 0);
    assert_eq!(
        count(&e.t, "SELECT count(*) FROM training_candidates").await,
        0
    );
    assert_eq!(
        count(&e.t, "SELECT count(*) FROM training_generations").await,
        0
    );

    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Created(0));
    work(&e.state.db, 2).await; // half of generation 0

    // "Restart": a brand-new pool, Db and Trainer (nothing in memory).
    let Env {
        t: _t,
        state: _s,
        trainer: old,
    } = &e;
    let _ = old; // the old trainer is never used again
    let pool2 = db::connect_with_retry(&e.t.url, 4, 5).await.unwrap();
    let db2 = Db::new(pool2, Arc::new(coordinator::metrics::Metrics::new()), 30, 3);
    let trainer2 = Trainer::with_parts(db2.clone(), e.state.metrics.clone(), Arc::new(FakeCem));

    trainer2.tick().await;
    trainer2.tick().await;
    assert_eq!(
        count(&e.t, "SELECT count(*) FROM jobs").await,
        4,
        "no duplicate jobs"
    );
    assert_eq!(
        count(&e.t, "SELECT count(*) FROM training_candidates").await,
        4
    );

    work_all(&db2).await;
    trainer2.tick().await;
    assert_eq!(get_run(&e, id).await["generation"], 1);
    assert_eq!(
        count(
            &e.t,
            "SELECT count(*) FROM training_candidates WHERE generation = 0"
        )
        .await,
        4
    );
    assert_eq!(
        count(
            &e.t,
            "SELECT count(*) FROM training_candidates WHERE generation = 1"
        )
        .await,
        4
    );
}

#[tokio::test]
async fn advisory_lock_lets_exactly_one_tick_advance_a_run() {
    let e = env().await;
    let id = create_run(&e, small_run(2)).await;

    // Someone else (another coordinator replica) holds the run's lock.
    let (tx, _run) = e
        .state
        .db
        .begin_training_step(id)
        .await
        .unwrap()
        .expect("lock");
    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Skipped);
    assert_eq!(count(&e.t, "SELECT count(*) FROM jobs").await, 0);
    drop(tx); // releases the lock (rollback)

    // Two ticks racing: exactly one generation comes out.
    let trainer_b = Trainer::new(&e.state);
    let (a, b) = tokio::join!(e.trainer.advance(id), trainer_b.advance(id));
    let steps = [a.unwrap(), b.unwrap()];
    assert_eq!(
        steps.iter().filter(|s| **s == Step::Created(0)).count(),
        1,
        "{steps:?}"
    );
    assert_eq!(count(&e.t, "SELECT count(*) FROM jobs").await, 4);
    assert_eq!(
        count(&e.t, "SELECT count(*) FROM training_generations").await,
        1
    );
}

#[tokio::test]
async fn cancel_cascades_to_jobs() {
    let e = env().await;
    let app = http::router(e.state.clone());
    let id = create_run(&e, small_run(3)).await;
    e.trainer.advance(id).await.unwrap();
    // One unit is in a worker's hands when the cancel arrives.
    assert_eq!(e.state.db.lease("w", 1).await.unwrap().len(), 1);

    let (status, run) = call(&app, "POST", &format!("/api/training/{id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["status"], "cancelled");
    assert!(run["finished_at"].is_string());
    assert_eq!(
        count(
            &e.t,
            "SELECT count(*) FROM jobs WHERE status <> 'cancelled'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &e.t,
            "SELECT count(*) FROM work_units WHERE state IN ('pending','leased')"
        )
        .await,
        0
    );
    assert_eq!(
        e.state.db.lease("w", 10).await.unwrap().len(),
        0,
        "nothing left to lease"
    );

    // The trainer leaves a cancelled run alone.
    assert_eq!(e.trainer.advance(id).await.unwrap(), Step::Skipped);
    e.trainer.tick().await;
    assert_eq!(count(&e.t, "SELECT count(*) FROM jobs").await, 4);

    // Cancelling again is harmless.
    let (status, run) = call(&app, "POST", &format!("/api/training/{id}/cancel"), None).await;
    assert_eq!(
        (status, &run["status"]),
        (StatusCode::OK, &json!("cancelled"))
    );
}

#[tokio::test]
async fn failed_candidate_job_scores_infinity() {
    let e = env().await;
    let id = create_run(&e, small_run(3)).await;
    e.trainer.advance(id).await.unwrap();
    // Cancel candidate 3's job (the would-be winner): its fitness is INFINITY.
    let (jid,): (i64,) = sqlx::query_as(
        "SELECT job_id FROM training_candidates WHERE run_id = $1 AND generation = 0 AND idx = 3",
    )
    .bind(id)
    .fetch_one(&e.t.pool)
    .await
    .unwrap();
    e.state.db.cancel_job(jid).await.unwrap();
    work_all(&e.state.db).await;
    e.trainer.advance(id).await.unwrap();

    let run = get_run(&e, id).await;
    let g0 = &run["generations"][0];
    assert_eq!(
        g0["best_fitness"], 5.8,
        "the cancelled candidate is ignored"
    );
    assert!(
        run["champion"]["strategy"]
            .as_str()
            .unwrap()
            .starts_with("linear:2,")
    );
    let inf: Vec<f64> = sqlx::query_scalar(
        "SELECT fitness FROM training_candidates WHERE run_id = $1 AND generation = 0 AND idx = 3",
    )
    .bind(id)
    .fetch_all(&e.t.pool)
    .await
    .unwrap();
    assert!(inf[0].is_infinite());
}

#[tokio::test]
async fn repeated_errors_fail_the_run() {
    let e = env().await;
    let id = create_run(&e, small_run(2)).await;
    // Corrupt the stored config so every step fails the same way.
    sqlx::query(
        "UPDATE training_runs SET config = jsonb_set(config, '{opponents}', '[\"x\",\"y\",\"z\"]')",
    )
    .execute(&e.t.pool)
    .await
    .unwrap();
    for _ in 0..4 {
        e.trainer.tick().await;
    }
    let run = get_run(&e, id).await;
    assert_eq!(run["status"], "running");
    assert!(run["last_error"].as_str().unwrap().contains("opponent"));
    e.trainer.tick().await;
    let run = get_run(&e, id).await;
    assert_eq!(run["status"], "failed");
    assert!(run["last_error"].as_str().unwrap().contains("gave up"));
}

#[tokio::test]
async fn champion_alias_resolves_in_jobs_and_include_training_filters() {
    let e = env().await;
    let app = http::router(e.state.clone());
    let id = create_run(&e, small_run(2)).await;

    let job_body =
        |s: &str| json!({"strategies": [s, "heuristic", "random", "random"], "num_hands": 100});
    // No champion yet.
    let (status, body) = call(
        &app,
        "POST",
        "/api/jobs",
        Some(job_body(&format!("champion:{id}"))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("no champion"));
    let (status, _) = call(&app, "POST", "/api/jobs", Some(job_body("champion:999"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&app, "POST", "/api/jobs", Some(job_body("champion:abc"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Finish a generation to get a champion.
    e.trainer.advance(id).await.unwrap();
    work_all(&e.state.db).await;
    e.trainer.advance(id).await.unwrap();
    let champion = get_run(&e, id).await["champion"]["strategy"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, job) = call(
        &app,
        "POST",
        "/api/jobs",
        Some(job_body(&format!("champion:{id}"))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    assert_eq!(job["spec"]["strategies"][0], champion.as_str());
    assert_eq!(job["standings"][0]["strategy"], champion.as_str());
    assert!(job["training_run_id"].is_null());
    let plain_id = job["id"].as_i64().unwrap();

    // The list hides the 8 training jobs unless asked.
    let (_, list) = call(&app, "GET", "/api/jobs", None).await;
    let ids: Vec<i64> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![plain_id]);
    let (_, all) = call(&app, "GET", "/api/jobs?include_training=true", None).await;
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 9);
    assert_eq!(
        all.iter()
            .filter(|j| j["training_run_id"] == json!(id))
            .count(),
        8
    );

    // champion: also works as a training opponent.
    let (status, run) = call(
        &app,
        "POST",
        "/api/training",
        Some(json!({"opponents": [format!("champion:{id}"), "heuristic", "random"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    assert_eq!(run["config"]["opponents"][0], champion.as_str());
}

/// The real adapter exists and is object-safe (compile-time check only: the
/// real `learn` functions are exercised by hearts-core's own tests).
#[test]
fn core_cem_is_a_cem() {
    let _: Arc<dyn Cem> = Arc::new(coordinator::trainer::CoreCem);
}
