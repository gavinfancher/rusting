//! The trainer: runs cross-entropy-method (CEM) training **on the cluster**.
//!
//! Each generation of a training run becomes `population` ordinary tournament
//! jobs (candidate weights in slot 0, fixed opponents in slots 1-3). The
//! workers chew through them exactly as they do any job; the trainer only
//! *plans* (sample candidates) and *judges* (read the finished jobs' scores,
//! update the belief). The maths lives in `hearts_core::learn` and is
//! deterministic, so a cluster run and `hearts-cli train` with the same
//! config and seed end with identical champion weights.
//!
//! # The state machine (one step per run per tick)
//!
//! ```text
//! running:    generation g has no candidates yet ──► CREATE generation g
//!             all of g's jobs are terminal       ──► FINISH generation g
//!                                                    (update the belief; then either
//!                                                     create g+1 or start validation)
//!             otherwise                          ──► wait (workers are busy)
//! validating: both validation jobs terminal      ──► run is done
//! ```
//!
//! # Why this design is safe (read this part!)
//!
//! **1. All state lives in Postgres, none in this process.** What the trainer
//! "remembers" between ticks is just rows: the run's `state` (the belief),
//! its `generation` counter, and the candidate/job rows. A freshly started
//! coordinator therefore picks up exactly where the old one stopped; there
//! is no "resume" code path, because every tick *is* the resume path.
//!
//! **2. One transaction per step.** Every step does all its writes inside a
//! single database transaction and commits once at the end. Postgres makes a
//! transaction all-or-nothing: if the process is killed (or the network
//! drops) anywhere before `COMMIT`, none of its changes ever happened. So a
//! crash can never leave "half a generation" (say, 10 of 24 candidate jobs
//! created, or fitness written but the belief not updated). The next tick
//! simply redoes the step from scratch. Steps are written to be *idempotent*:
//! each begins by asking the database "has this already been done?" (does
//! generation g have candidates? is generation g's row finished?), and the
//! primary key `(run_id, generation, idx)` on `training_candidates` is a
//! backstop that makes an accidental duplicate fail loudly instead of
//! silently doubling the work.
//!
//! **3. A Postgres advisory lock so only one coordinator advances a run.**
//! With several coordinator replicas, each runs a trainer. Two of them
//! reading "generation 7 has no candidates" at the same moment would both
//! try to create it. Before touching a run, a step takes
//! `pg_try_advisory_xact_lock(42, run_id)`: a mutex that Postgres keeps for
//! us, identified by the pair of numbers (42 = "trainer", run id). `try`
//! means "don't wait": if somebody else holds it we skip the run this tick
//! (they are already advancing it). `xact` means the lock is released
//! automatically when the transaction commits *or rolls back, or the
//! connection dies*; with a connection pool that is essential, because a
//! session-level lock could leak onto a pooled connection that outlives the
//! step. (A crashed coordinator can't leave a lock behind.)
//!
//! **4. `SELECT ... FOR UPDATE` on the run row, re-read under the lock.**
//! The list of active runs we loop over may be stale by the time we hold the
//! lock, so the step re-reads the run *after* locking and acts only on that
//! fresh copy. The row lock also serialises us with `cancel`: cancelling
//! updates the same row, so it waits for our commit and then cancels
//! everything we created (see `Db::cancel_training_run`).
//!
//! **5. Failure handling.** If a step errors, its transaction rolls back
//! (nothing changed), the error text goes into `training_runs.last_error` so
//! the UI shows it, and we retry next tick. After [`MAX_CONSECUTIVE_ERRORS`]
//! errors in a row the run is marked `failed` and its jobs are cancelled, so
//! a permanently broken run can't spin forever. The consecutive-error
//! counter is the one thing kept in memory; losing it on restart only gives
//! a broken run a few more tries.
//!
//! # The `Cem` trait (test seam)
//! The four `learn` functions are reached through the [`Cem`] trait. Production uses
//! [`CoreCem`], which just forwards to `hearts_core::learn`; tests inject a
//! tiny deterministic fake so they exercise the *plumbing* (jobs, fitness,
//! state progression, resumption) without depending on the real algorithm.

use crate::{
    db::{CandidateRow, Db, GenerationOutcome, RunRow, Tx, is_terminal_job, slot0_fitness},
    metrics::Metrics,
    model::TrainingConfig,
    state::{AppState, wait_for_shutdown},
};
use hearts_core::{
    JobSpec, StrategyKind, Weights,
    learn::{self, CemConfig, CemState, Init},
    seed::child_seed,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Errors in a row on one run before the trainer gives up and marks it failed.
pub const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// The validation jobs use the seed `child_seed(run_seed, VALIDATION_SEED_INDEX)`:
/// a stream of hands no training generation ever saw (generations use indices
/// 0..generations, which is capped at 1000).
pub const VALIDATION_SEED_INDEX: u64 = 1_000_000;

/// The learning algorithm, as the trainer sees it. See the module docs.
pub trait Cem: Send + Sync + 'static {
    fn initial_state(&self, init: Init, sigma0: f64) -> CemState;
    fn sample(&self, state: &CemState, population: usize, seed: u64) -> Vec<Weights>;
    fn update(&self, state: &CemState, cfg: &CemConfig, scored: &[(Weights, f64)]) -> CemState;
    fn to_weights(&self, v: &[f64]) -> Weights;
}

/// The real thing: forwards to `hearts_core::learn`.
pub struct CoreCem;

impl Cem for CoreCem {
    fn initial_state(&self, init: Init, sigma0: f64) -> CemState {
        learn::initial_state(init, sigma0)
    }
    fn sample(&self, state: &CemState, population: usize, seed: u64) -> Vec<Weights> {
        learn::sample(state, population, seed)
    }
    fn update(&self, state: &CemState, cfg: &CemConfig, scored: &[(Weights, f64)]) -> CemState {
        learn::update(state, cfg, scored)
    }
    fn to_weights(&self, v: &[f64]) -> Weights {
        learn::to_weights(v)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TrainerError {
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("run is inconsistent: {0}")]
    Inconsistent(String),
}

/// What one call to [`Trainer::advance`] did (mostly useful for tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Another replica holds the run's lock (or it just stopped being active).
    Skipped,
    /// Nothing to do yet: waiting for workers.
    Waiting,
    /// Created the candidate jobs for this generation.
    Created(i32),
    /// Judged this generation (and created the next one, or validation).
    Finished(i32),
    /// The run is `done` (`true`) or `failed` (`false`) after validation.
    Completed(bool),
}

pub struct Trainer {
    db: Db,
    metrics: Arc<Metrics>,
    cem: Arc<dyn Cem>,
    /// Consecutive failed steps per run id (see module docs, point 5).
    errors: Mutex<HashMap<i64, u32>>,
}

/// Bookkeeping a step wants applied *after* its transaction commits, so a
/// rolled-back step never moves a metric.
#[derive(Default)]
struct Effects {
    jobs_created: u64,
    generation_secs: Option<f64>,
}

impl Trainer {
    pub fn new(state: &AppState) -> Trainer {
        Trainer::with_parts(state.db.clone(), state.metrics.clone(), state.cem.clone())
    }

    pub fn with_parts(db: Db, metrics: Arc<Metrics>, cem: Arc<dyn Cem>) -> Trainer {
        Trainer {
            db,
            metrics,
            cem,
            errors: Mutex::new(HashMap::new()),
        }
    }

    /// One pass over every active run, then refresh the training gauges.
    /// Never fails: per-run errors are logged and recorded on the run.
    pub async fn tick(&self) {
        let ids = match self.db.active_training_run_ids().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(error = %e, "trainer: listing active runs failed");
                return;
            }
        };
        for id in ids {
            match self.advance(id).await {
                Ok(step) => {
                    if step != Step::Skipped {
                        self.errors
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&id);
                    }
                    if !matches!(step, Step::Waiting | Step::Skipped) {
                        tracing::info!(run = id, ?step, "trainer step");
                    }
                }
                Err(e) => self.handle_error(id, e).await,
            }
        }
        self.refresh_gauges().await;
    }

    async fn handle_error(&self, id: i64, e: TrainerError) {
        let n = {
            let mut errors = self.errors.lock().unwrap_or_else(|p| p.into_inner());
            let n = errors.entry(id).or_insert(0);
            *n += 1;
            *n
        };
        let msg = e.to_string();
        tracing::error!(run = id, attempt = n, error = %msg, "trainer step failed");
        let result = if n >= MAX_CONSECUTIVE_ERRORS {
            tracing::error!(
                run = id,
                "too many consecutive errors; marking the run failed"
            );
            self.errors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            self.db
                .fail_training_run(id, &format!("gave up after {n} errors; last: {msg}"))
                .await
        } else {
            self.db.record_training_error(id, &msg).await
        };
        if let Err(e) = result {
            tracing::error!(run = id, error = %e, "could not record the trainer error");
        }
    }

    async fn refresh_gauges(&self) {
        match self.db.active_training_run_ids().await {
            Ok(ids) => self.metrics.training_runs_active.set(ids.len() as i64),
            Err(e) => tracing::warn!(error = %e, "trainer: counting active runs failed"),
        }
        match self.db.latest_generation_fitness().await {
            Ok(Some((mean, best))) => {
                self.metrics.training_latest_fitness.set(mean);
                self.metrics.training_latest_best_fitness.set(best);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "trainer: reading latest fitness failed"),
        }
    }

    /// Advance one run by one step (the module docs explain the locking).
    pub async fn advance(&self, run_id: i64) -> Result<Step, TrainerError> {
        // Lock + fresh read. `None` = busy or no longer active.
        let Some((mut tx, run)) = self.db.begin_training_step(run_id).await? else {
            return Ok(Step::Skipped);
        };
        let mut fx = Effects::default();
        let step = match run.status.as_str() {
            "running" => self.advance_running(&mut tx, &run, &mut fx).await?,
            "validating" => self.advance_validating(&mut tx, &run).await?,
            other => {
                return Err(TrainerError::Inconsistent(format!(
                    "unexpected status {other:?}"
                )));
            }
        };
        // The one and only COMMIT: everything the step did becomes visible
        // atomically, and the advisory lock is released with it.
        tx.commit().await?;

        self.metrics.jobs_created.inc_by(fx.jobs_created);
        if let Some(secs) = fx.generation_secs {
            self.metrics.training_generations.inc();
            self.metrics.training_generation_duration.observe(secs);
        }
        Ok(step)
    }

    // ---- running ---------------------------------------------------------

    async fn advance_running(
        &self,
        tx: &mut Tx,
        run: &RunRow,
        fx: &mut Effects,
    ) -> Result<Step, TrainerError> {
        let cfg = &run.config.0;
        let opponents = parse_opponents(cfg)?;
        let state = &run.state.0;
        let g = run.generation;
        if state.generation as i32 != g {
            return Err(TrainerError::Inconsistent(format!(
                "state.generation {} != generation column {g}",
                state.generation
            )));
        }

        // Safety net: a "running" run that already did all its generations
        // (can't normally happen, since finishing the last one switches to
        // validating in the same transaction).
        if g as u32 >= cfg.generations {
            self.start_validation(tx, run, &opponents, fx).await?;
            return Ok(Step::Finished(g));
        }

        let candidates = Db::tx_candidates(tx, run.id, g).await?;
        if candidates.is_empty() {
            self.create_generation(tx, run.id, cfg, &opponents, state, fx)
                .await?;
            return Ok(Step::Created(g));
        }
        let all_terminal = candidates
            .iter()
            .all(|c| c.job_status.as_deref().is_none_or(is_terminal_job));
        if !all_terminal {
            return Ok(Step::Waiting);
        }

        // ---- every candidate has been scored: judge the generation --------
        let fitness: Vec<f64> = candidates.iter().map(fitness_of).collect();
        let scored: Vec<(Weights, f64)> = candidates
            .iter()
            .zip(&fitness)
            .map(|(c, &f)| (c.weights.0, f))
            .collect();
        let cem_cfg = cem_config(cfg);
        let new_state = self.cem.update(state, &cem_cfg, &scored);
        // CONTRACT: `new_state.generation == g + 1`; we trust the column we
        // write, not whatever the algorithm reports.
        let mut new_state = new_state;
        new_state.generation = g as u32 + 1;
        let champion = self.cem.to_weights(&new_state.mean);
        let outcome = outcome_of(&fitness, cfg.elites, state);
        let secs = Db::tx_finish_generation(
            tx, run.id, g, &fitness, &outcome, &new_state, champion, fitness[0],
        )
        .await?;
        fx.generation_secs = Some(secs);

        if new_state.generation >= cfg.generations {
            // Last generation judged: queue the validation jobs in the same
            // transaction, so there is no moment where the run is "between".
            let mut finished = run_with_champion(run, champion, &new_state);
            finished.generation = new_state.generation as i32;
            self.start_validation(tx, &finished, &opponents, fx).await?;
        } else {
            // Keep the cluster busy: queue the next generation right away
            // instead of idling until the next tick.
            self.create_generation(tx, run.id, cfg, &opponents, &new_state, fx)
                .await?;
        }
        Ok(Step::Finished(g))
    }

    /// Sample `population` candidates from `state` and create their jobs.
    async fn create_generation(
        &self,
        tx: &mut Tx,
        run_id: i64,
        cfg: &TrainingConfig,
        opponents: &[StrategyKind; 3],
        state: &CemState,
        fx: &mut Effects,
    ) -> Result<(), TrainerError> {
        let g = state.generation;
        let seed_g = learn::generation_seed(cfg.seed, g);
        let weights = self.cem.sample(state, cfg.population, seed_g);
        if weights.len() != cfg.population {
            return Err(TrainerError::Inconsistent(format!(
                "sample returned {} candidates, expected {}",
                weights.len(),
                cfg.population
            )));
        }
        // Every candidate plays the SAME hands (seed_g): "common random
        // numbers" remove most luck from comparing candidates.
        let candidates: Vec<(Weights, JobSpec)> = weights
            .into_iter()
            .map(|w| (w, learn::candidate_spec(w, *opponents, seed_g, cfg.rules)))
            .collect();
        Db::tx_create_generation(
            tx,
            run_id,
            g as i32,
            seed_g,
            &candidates,
            cfg.hands_per_candidate,
            cfg.unit_size as i32,
        )
        .await?;
        fx.jobs_created += candidates.len() as u64;
        Ok(())
    }

    /// Champion and plain heuristic, each against the opponents, on the same
    /// fresh hands.
    async fn start_validation(
        &self,
        tx: &mut Tx,
        run: &RunRow,
        opponents: &[StrategyKind; 3],
        fx: &mut Effects,
    ) -> Result<(), TrainerError> {
        let cfg = &run.config.0;
        let champion = match &run.champion {
            Some(w) => w.0,
            None => self.cem.to_weights(&run.state.0.mean),
        };
        let seed = child_seed(cfg.seed, VALIDATION_SEED_INDEX);
        let champion_spec = learn::candidate_spec(champion, *opponents, seed, cfg.rules);
        let mut baseline_spec = champion_spec.clone();
        baseline_spec.strategies[0] = StrategyKind::Heuristic;
        Db::tx_create_validation(
            tx,
            run.id,
            &champion_spec,
            &baseline_spec,
            cfg.validation_hands,
            cfg.unit_size as i32,
        )
        .await?;
        fx.jobs_created += 2;
        Ok(())
    }

    // ---- validating ------------------------------------------------------

    async fn advance_validating(&self, tx: &mut Tx, run: &RunRow) -> Result<Step, TrainerError> {
        let (Some(c), Some(b)) = (run.validation_job_id, run.baseline_job_id) else {
            Db::tx_finish_run(tx, run.id, "failed", Some("validation jobs are missing")).await?;
            return Ok(Step::Completed(false));
        };
        let statuses = Db::tx_job_statuses(tx, &[c, b]).await?;
        if statuses.iter().any(Option::is_none) {
            Db::tx_finish_run(tx, run.id, "failed", Some("a validation job was deleted")).await?;
            return Ok(Step::Completed(false));
        }
        if !statuses.iter().flatten().all(|s| is_terminal_job(s)) {
            return Ok(Step::Waiting);
        }
        if statuses.iter().flatten().all(|s| s == "done") {
            Db::tx_finish_run(tx, run.id, "done", None).await?;
            Ok(Step::Completed(true))
        } else {
            Db::tx_finish_run(
                tx,
                run.id,
                "failed",
                Some("a validation job failed or was cancelled"),
            )
            .await?;
            Ok(Step::Completed(false))
        }
    }
}

// ---- pure helpers (unit-tested below) ---------------------------------------

fn parse_opponents(cfg: &TrainingConfig) -> Result<[StrategyKind; 3], TrainerError> {
    let parsed: Vec<StrategyKind> = cfg
        .opponents
        .iter()
        .map(|s| s.parse::<StrategyKind>())
        .collect::<Result<_, _>>()
        .map_err(|e| TrainerError::Inconsistent(format!("bad opponent in config: {e}")))?;
    parsed.try_into().map_err(|v: Vec<_>| {
        TrainerError::Inconsistent(format!("config has {} opponents, expected 3", v.len()))
    })
}

pub fn cem_config(cfg: &TrainingConfig) -> CemConfig {
    CemConfig {
        population: cfg.population,
        elites: cfg.elites,
        smoothing: cfg.smoothing,
        min_sigma: cfg.min_sigma,
    }
}

/// A candidate's fitness: slot 0's average points per hand if its job
/// finished `done`, else `INFINITY` (failed / cancelled / vanished job).
fn fitness_of(c: &CandidateRow) -> f64 {
    match c.job_status.as_deref() {
        Some("done") => slot0_fitness(c.hands, c.points),
        _ => f64::INFINITY,
    }
}

/// Summary statistics for the learning curve. Non-finite fitnesses
/// (failed evaluations) are ignored by the means; if none is finite the
/// statistic is `INFINITY`, which the API reports as `null`.
fn outcome_of(fitness: &[f64], elites: usize, sampled_from: &CemState) -> GenerationOutcome {
    let mean_of = |v: &[f64]| {
        if v.is_empty() {
            f64::INFINITY
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    // Best first; ties broken by candidate index (matches `learn::update`).
    let mut order: Vec<usize> = (0..fitness.len()).collect();
    order.sort_by(|&a, &b| fitness[a].total_cmp(&fitness[b]).then(a.cmp(&b)));
    let elite: Vec<f64> = order
        .iter()
        .take(elites)
        .map(|&i| fitness[i])
        .filter(|f| f.is_finite())
        .collect();
    let finite: Vec<f64> = fitness.iter().copied().filter(|f| f.is_finite()).collect();
    GenerationOutcome {
        mean_fitness: fitness[0],
        best_fitness: order.first().map_or(f64::INFINITY, |&i| fitness[i]),
        elite_mean_fitness: mean_of(&elite),
        population_mean_fitness: mean_of(&finite),
        sigma_mean: sampled_from.sigma.iter().sum::<f64>() / sampled_from.sigma.len().max(1) as f64,
    }
}

/// A copy of `run` as it looks right after its last generation was judged,
/// so `start_validation` sees the fresh champion without re-reading the DB.
fn run_with_champion(run: &RunRow, champion: Weights, state: &CemState) -> RunRow {
    RunRow {
        id: run.id,
        name: run.name.clone(),
        status: run.status.clone(),
        config: sqlx::types::Json(run.config.0.clone()),
        seed: run.seed,
        state: sqlx::types::Json(state.clone()),
        generation: run.generation,
        champion: Some(sqlx::types::Json(champion)),
        champion_fitness: run.champion_fitness,
        validation_job_id: None,
        baseline_job_id: None,
        last_error: None,
        created_at: run.created_at,
        finished_at: None,
    }
}

/// The background loop: tick every `TRAINER_TICK_MS` until shutdown.
pub async fn run(state: AppState) {
    let trainer = Trainer::new(&state);
    let mut shutdown = state.shutdown.clone();
    let mut interval =
        tokio::time::interval(Duration::from_millis(state.config.trainer_tick_ms.max(50)));
    // If a tick runs long, don't fire a burst of catch-up ticks afterwards.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => trainer.tick().await,
            _ = wait_for_shutdown(&mut shutdown) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(status: Option<&str>, hands: i64, points: i64) -> CandidateRow {
        CandidateRow {
            idx: 0,
            weights: sqlx::types::Json(Weights([0; hearts_core::NUM_WEIGHTS])),
            job_id: Some(1),
            job_status: status.map(String::from),
            hands,
            points,
        }
    }

    #[test]
    fn fitness_is_points_per_hand_or_infinity() {
        assert_eq!(fitness_of(&cand(Some("done"), 100, 650)), 6.5);
        assert!(fitness_of(&cand(Some("failed"), 100, 650)).is_infinite());
        assert!(fitness_of(&cand(Some("cancelled"), 0, 0)).is_infinite());
        assert!(fitness_of(&cand(None, 0, 0)).is_infinite());
    }

    #[test]
    fn outcome_ignores_failed_candidates_in_means() {
        let state = CemState {
            generation: 0,
            mean: vec![0.0; 2],
            sigma: vec![10.0, 20.0],
        };
        let f = [6.0, f64::INFINITY, 5.0, 7.0];
        let o = outcome_of(&f, 2, &state);
        assert_eq!(o.mean_fitness, 6.0);
        assert_eq!(o.best_fitness, 5.0);
        assert_eq!(o.elite_mean_fitness, 5.5);
        assert_eq!(o.population_mean_fitness, 6.0);
        assert_eq!(o.sigma_mean, 15.0);

        let all_bad = outcome_of(&[f64::INFINITY; 3], 2, &state);
        assert!(
            all_bad.best_fitness.is_infinite() && all_bad.population_mean_fitness.is_infinite()
        );
    }
}
