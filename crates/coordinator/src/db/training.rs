//! Training queries (Level 1 learning). A child module of [`crate::db`], so it
//! can use `Db`'s private pool and timer; the rule "every SQL statement lives
//! under `db`" still holds.
//!
//! Two kinds of methods live here:
//!
//! * **Pool methods** (`&self`) used by the HTTP layer: create, get, list and
//!   cancel runs, plus metric refreshers.
//! * **Transaction helpers** (`tx_*`, associated functions taking an open
//!   transaction) used by the trainer. The trainer opens ONE transaction per
//!   run per tick, takes the run's advisory lock via [`Db::begin_training_step`],
//!   and calls these helpers; nothing is visible to anyone until the trainer
//!   commits, which is what makes every step crash-safe. See
//!   [`crate::trainer`] for the full story.

use super::*;
use hearts_core::{Weights, learn::CemState};

/// An open Postgres transaction. All `tx_*` helpers take `&mut Tx`.
pub type Tx = sqlx::Transaction<'static, sqlx::Postgres>;

/// Terminal statuses of a `jobs` row.
const TERMINAL: [&str; 3] = ["done", "failed", "cancelled"];

pub fn is_terminal_job(status: &str) -> bool {
    TERMINAL.contains(&status)
}

/// A `training_runs` row.
#[derive(sqlx::FromRow, Debug)]
pub struct RunRow {
    pub id: i64,
    pub name: Option<String>,
    pub status: String,
    pub config: Json<TrainingConfig>,
    pub seed: i64,
    pub state: Json<CemState>,
    pub generation: i32,
    pub champion: Option<Json<Weights>>,
    pub champion_fitness: Option<f64>,
    pub validation_job_id: Option<i64>,
    pub baseline_job_id: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// `SELECT <all RunRow columns> FROM training_runs <tail>` as a string
/// literal (sqlx wants literals, not `format!`, to rule out SQL injection).
macro_rules! select_run {
    ($tail:literal) => {
        concat!(
            "SELECT id, name, status::text AS status, config, seed, state, generation,
                    champion, champion_fitness, validation_job_id, baseline_job_id, last_error,
                    created_at, finished_at FROM training_runs ",
            $tail
        )
    };
}

/// One candidate of a generation together with how its scoring job is going.
#[derive(sqlx::FromRow, Debug)]
pub struct CandidateRow {
    pub idx: i32,
    pub weights: Json<Weights>,
    pub job_id: Option<i64>,
    /// `None` if the job row is gone (deleted): treated as a failed evaluation.
    pub job_status: Option<String>,
    /// Slot 0's totals over the job's finished units.
    pub hands: i64,
    pub points: i64,
}

/// The numbers `tx_finish_generation` stores in `training_generations`.
#[derive(Debug, Clone, Copy)]
pub struct GenerationOutcome {
    pub mean_fitness: f64,
    pub best_fitness: f64,
    pub elite_mean_fitness: f64,
    pub population_mean_fitness: f64,
    pub sigma_mean: f64,
}

/// Slot-0 totals of a job (status, hands, points).
type JobSlot0 = (String, i64, i64);

/// Non-finite floats (a failed evaluation is `INFINITY`) can't be written in
/// JSON, so the API reports them as `null`.
fn finite(x: Option<f64>) -> Option<f64> {
    x.filter(|v| v.is_finite())
}

/// Map a (possibly huge) run id to the int4 the two-key advisory-lock
/// function wants. Runs would need to exceed 2^31 for this to matter.
fn lock_key(run_id: i64) -> i32 {
    (run_id & 0x7fff_ffff) as i32
}

/// First advisory-lock key: "this lock belongs to the trainer". The second
/// key is the run id, so each run has its own lock.
pub const TRAINER_LOCK_CLASS: i32 = 42;

impl Db {
    // ---------------------------------------------------------------------
    // Runs: create / read / cancel (HTTP layer)
    // ---------------------------------------------------------------------

    pub async fn create_training_run(&self, cfg: &TrainingConfig, state: &CemState) -> Result<i64> {
        let _t = self.timer("create_training_run");
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO training_runs (name, config, seed, state)
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(&cfg.name)
        .bind(Json(cfg))
        .bind(cfg.seed as i64)
        .bind(Json(state))
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn get_training_run(&self, id: i64) -> Result<Option<TrainingRun>> {
        let _t = self.timer("get_training_run");
        let rows: Vec<RunRow> = sqlx::query_as(select_run!("WHERE id = $1"))
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        Ok(self.assemble_runs(rows, true).await?.into_iter().next())
    }

    /// Newest first, without the per-generation curve.
    pub async fn list_training_runs(&self, limit: i64) -> Result<Vec<TrainingRun>> {
        let _t = self.timer("list_training_runs");
        let rows: Vec<RunRow> = sqlx::query_as(select_run!("ORDER BY id DESC LIMIT $1"))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        self.assemble_runs(rows, false).await
    }

    /// The champion of a run: `Ok(None)` = no such run;
    /// `Ok(Some(None))` = run exists but has no champion yet.
    pub async fn training_champion(&self, run_id: i64) -> Result<Option<Option<Weights>>> {
        let row: Option<(Option<Json<Weights>>,)> =
            sqlx::query_as("SELECT champion FROM training_runs WHERE id = $1")
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(c,)| c.map(|j| j.0)))
    }

    /// Cancel a run and every non-terminal job it owns. Returns `None` if
    /// the run doesn't exist; a run already finished is returned unchanged.
    ///
    /// Locking: the `UPDATE training_runs` takes the run's row lock, which
    /// the trainer also holds (`SELECT ... FOR UPDATE`) while it advances the
    /// run. So either the trainer's tick finishes first (and its freshly
    /// created jobs are visible to the statements below, because each
    /// statement in READ COMMITTED sees everything committed before it
    /// starts), or we win and the trainer then finds `cancelled` and stops.
    /// Either way no job can escape the cancel. Lock order is run row, then
    /// units, then jobs, the same unit-before-job order used elsewhere.
    pub async fn cancel_training_run(&self, id: i64) -> Result<Option<TrainingRun>> {
        {
            let _t = self.timer("cancel_training_run");
            let mut tx = self.pool.begin().await?;
            let cancelled = sqlx::query(
                "UPDATE training_runs
                    SET status = 'cancelled', finished_at = now(), updated_at = now()
                  WHERE id = $1 AND status IN ('running', 'validating')",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            if cancelled.rows_affected() > 0 {
                Self::tx_cancel_run_jobs(&mut tx, id).await?;
            }
            tx.commit().await?;
        }
        self.get_training_run(id).await
    }

    /// Cancel the run's pending/leased units and its unfinished jobs.
    async fn tx_cancel_run_jobs(tx: &mut Tx, run_id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE work_units
                SET state = 'cancelled', lease_owner = NULL, lease_expires_at = NULL
              WHERE state IN ('pending', 'leased')
                AND job_id IN (SELECT id FROM jobs WHERE training_run_id = $1)",
        )
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE jobs SET status = 'cancelled', finished_at = now()
              WHERE training_run_id = $1 AND status IN ('pending', 'running')",
        )
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Turn raw rows into API objects. Uses a handful of set-based queries
    /// regardless of how many runs there are.
    async fn assemble_runs(
        &self,
        rows: Vec<RunRow>,
        with_generations: bool,
    ) -> Result<Vec<TrainingRun>> {
        if rows.is_empty() {
            return Ok(vec![]);
        }
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();

        // Learning curve (finished generations only).
        let mut curves: HashMap<i64, Vec<GenerationStats>> = HashMap::new();
        if with_generations {
            #[allow(clippy::type_complexity)]
            let gens: Vec<(
                i64,
                i32,
                i64,
                Option<f64>,
                Option<f64>,
                Option<f64>,
                Option<f64>,
                Option<f64>,
                DateTime<Utc>,
                Option<DateTime<Utc>>,
                Option<i64>,
            )> = sqlx::query_as(
                "SELECT run_id, generation, seed, mean_fitness, best_fitness,
                        elite_mean_fitness, population_mean_fitness, sigma_mean,
                        started_at, finished_at,
                        (EXTRACT(EPOCH FROM (finished_at - started_at)) * 1000)::bigint
                   FROM training_generations
                  WHERE run_id = ANY($1) AND finished_at IS NOT NULL
                  ORDER BY run_id, generation",
            )
            .bind(&ids)
            .fetch_all(&self.pool)
            .await?;
            for (run, generation, seed, mean, best, elite, pop, sigma, started, finished, ms) in
                gens
            {
                curves.entry(run).or_default().push(GenerationStats {
                    generation,
                    seed: seed as u64,
                    mean_fitness: finite(mean),
                    best_fitness: finite(best),
                    elite_mean_fitness: finite(elite),
                    population_mean_fitness: finite(pop),
                    sigma_mean: finite(sigma),
                    started_at: started,
                    finished_at: finished,
                    elapsed_ms: ms,
                });
            }
        }

        // Progress of each running run's in-flight generation.
        let progress: Vec<(i64, i32, i64, i64, i64)> = sqlx::query_as(
            "SELECT c.run_id, c.generation,
                    count(*),
                    count(*) FILTER (WHERE j.status IN ('done', 'failed', 'cancelled')),
                    COALESCE(SUM(p.hands_done), 0)::bigint
               FROM training_candidates c
               JOIN training_runs r ON r.id = c.run_id AND r.generation = c.generation
               LEFT JOIN jobs j ON j.id = c.job_id
               LEFT JOIN LATERAL (SELECT SUM(hand_end - hand_start) AS hands_done
                                    FROM work_units u
                                   WHERE u.job_id = j.id AND u.state = 'done') p ON true
              WHERE c.run_id = ANY($1) AND r.status = 'running'
              GROUP BY c.run_id, c.generation",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let progress: HashMap<i64, (i32, i64, i64, i64)> = progress
            .into_iter()
            .map(|(run, g, total, done, hands)| (run, (g, total, done, hands)))
            .collect();

        // Validation jobs' results.
        let val_ids: Vec<i64> = rows
            .iter()
            .flat_map(|r| [r.validation_job_id, r.baseline_job_id])
            .flatten()
            .collect();
        let slot0 = self.job_slot0(&val_ids).await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let cfg = r.config.0;
                let current =
                    progress
                        .get(&r.id)
                        .map(|&(g, total, done, hands)| CurrentGeneration {
                            generation: g,
                            candidates_total: total,
                            candidates_done: done,
                            hands_done: hands,
                            hands_total: cfg.population as i64 * cfg.hands_per_candidate,
                        });
                let champion = r
                    .champion
                    .map(|w| Champion::new(w.0, finite(r.champion_fitness)));
                let validation = match (r.validation_job_id, r.baseline_job_id) {
                    (Some(cj), Some(bj)) => {
                        let fit = |job: i64| -> Option<Option<f64>> {
                            let (status, hands, points) = slot0.get(&job)?;
                            if !is_terminal_job(status) {
                                return None; // still running: no result yet
                            }
                            Some(if status == "done" {
                                Some(slot0_fitness(*hands, *points)).and_then(|f| finite(Some(f)))
                            } else {
                                None
                            })
                        };
                        match (fit(cj), fit(bj)) {
                            (Some(c), Some(b)) => Some(Validation {
                                hands: cfg.validation_hands,
                                champion_fitness: c,
                                baseline_fitness: b,
                                champion_job_id: cj,
                                baseline_job_id: bj,
                            }),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                TrainingRun {
                    id: r.id,
                    name: r.name,
                    status: r.status,
                    generations_total: cfg.generations as i32,
                    config: cfg,
                    seed: r.seed,
                    generation: r.generation,
                    current,
                    champion,
                    generations: with_generations.then(|| curves.remove(&r.id).unwrap_or_default()),
                    validation,
                    created_at: r.created_at,
                    finished_at: r.finished_at,
                    last_error: r.last_error,
                }
            })
            .collect())
    }

    /// `(status, hands, points)` of slot 0 for each job id.
    async fn job_slot0(&self, ids: &[i64]) -> Result<HashMap<i64, JobSlot0>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(i64, String, i64, i64)> = sqlx::query_as(
            "SELECT j.id, j.status::text,
                    COALESCE(s.hands, 0)::bigint, COALESCE(s.points, 0)::bigint
               FROM jobs j
               LEFT JOIN LATERAL (SELECT SUM(r.hands) AS hands, SUM(r.points) AS points
                                    FROM unit_results r JOIN work_units u ON u.id = r.unit_id
                                   WHERE u.job_id = j.id AND r.slot = 0) s ON true
              WHERE j.id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, st, h, p)| (id, (st, h, p)))
            .collect())
    }

    // ---------------------------------------------------------------------
    // Metrics refreshers (called by the trainer each tick)
    // ---------------------------------------------------------------------

    /// Ids of runs the trainer must look at.
    pub async fn active_training_run_ids(&self) -> Result<Vec<i64>> {
        let _t = self.timer("training_active_runs");
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM training_runs WHERE status IN ('running', 'validating') ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(i,)| i).collect())
    }

    /// `(mean_fitness, best_fitness)` of the most recently finished
    /// generation of any run (read from the DB, not remembered in memory, so
    /// every coordinator replica reports the same value and a restart
    /// doesn't reset the gauge).
    pub async fn latest_generation_fitness(&self) -> Result<Option<(f64, f64)>> {
        let _t = self.timer("training_latest_fitness");
        let row: Option<(Option<f64>, Option<f64>)> = sqlx::query_as(
            "SELECT mean_fitness, best_fitness FROM training_generations
              WHERE finished_at IS NOT NULL
              ORDER BY finished_at DESC, run_id DESC, generation DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .and_then(|(m, b)| Some((m.filter(|x| x.is_finite())?, b.filter(|x| x.is_finite())?))))
    }

    // ---------------------------------------------------------------------
    // Trainer transaction helpers
    // ---------------------------------------------------------------------

    /// Open the transaction in which the trainer advances one run.
    ///
    /// 1. `pg_try_advisory_xact_lock(42, run_id)`: a cheap, application-level
    ///    mutex held by Postgres until this transaction ends. If another
    ///    coordinator replica (or an overlapping tick) already holds it we get
    ///    `false` immediately and return `None`: that someone else is
    ///    advancing this run, so we just skip it.
    /// 2. `SELECT ... FOR UPDATE` on the run row, re-reading the run *under the
    ///    lock*. Our earlier "list active runs" query may be stale; only this
    ///    read can be trusted. The row lock also makes `cancel` wait for us.
    ///
    /// Returns `None` if the lock is busy or the run is no longer running/validating.
    pub async fn begin_training_step(&self, run_id: i64) -> Result<Option<(Tx, RunRow)>> {
        let mut tx = self.pool.begin().await?;
        let (got,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_xact_lock($1, $2)")
            .bind(TRAINER_LOCK_CLASS)
            .bind(lock_key(run_id))
            .fetch_one(&mut *tx)
            .await?;
        if !got {
            return Ok(None); // dropping `tx` rolls back (nothing was changed)
        }
        let run: Option<RunRow> = sqlx::query_as(select_run!(
            "WHERE id = $1 AND status IN ('running', 'validating') FOR UPDATE"
        ))
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(run.map(|r| (tx, r)))
    }

    /// Candidates of generation `generation` with their job status and slot-0
    /// totals. Empty means the generation hasn't been created yet.
    pub async fn tx_candidates(
        tx: &mut Tx,
        run_id: i64,
        generation: i32,
    ) -> Result<Vec<CandidateRow>> {
        Ok(sqlx::query_as(
            "SELECT c.idx, c.weights, c.job_id, j.status::text AS job_status,
                    COALESCE(s.hands, 0)::bigint AS hands, COALESCE(s.points, 0)::bigint AS points
               FROM training_candidates c
               LEFT JOIN jobs j ON j.id = c.job_id
               LEFT JOIN LATERAL (SELECT SUM(r.hands) AS hands, SUM(r.points) AS points
                                    FROM unit_results r JOIN work_units u ON u.id = r.unit_id
                                   WHERE u.job_id = j.id AND r.slot = 0) s ON true
              WHERE c.run_id = $1 AND c.generation = $2
              ORDER BY c.idx",
        )
        .bind(run_id)
        .bind(generation)
        .fetch_all(&mut **tx)
        .await?)
    }

    /// Create a whole generation: one tournament job per candidate, the
    /// `training_candidates` rows, and the `training_generations` row. The
    /// caller's transaction makes this all-or-nothing.
    pub async fn tx_create_generation(
        tx: &mut Tx,
        run_id: i64,
        generation: i32,
        gen_seed: u64,
        candidates: &[(Weights, JobSpec)],
        hands: i64,
        unit_size: i32,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO training_generations (run_id, generation, seed) VALUES ($1, $2, $3)",
        )
        .bind(run_id)
        .bind(generation)
        .bind(gen_seed as i64)
        .execute(&mut **tx)
        .await?;
        for (idx, (weights, spec)) in candidates.iter().enumerate() {
            let name = format!("train#{run_id} g{generation} c{idx}");
            let job_id =
                Db::create_job_in_tx(tx, Some(&name), spec, hands, unit_size, Some(run_id)).await?;
            // The primary key (run_id, generation, idx) is the last line of
            // defence: a duplicate generation fails here instead of silently
            // double-creating.
            sqlx::query(
                "INSERT INTO training_candidates (run_id, generation, idx, weights, job_id)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(run_id)
            .bind(generation)
            .bind(idx as i32)
            .bind(Json(weights))
            .bind(job_id)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    /// Record a finished generation. Returns how long it took (seconds).
    #[allow(clippy::too_many_arguments)]
    pub async fn tx_finish_generation(
        tx: &mut Tx,
        run_id: i64,
        generation: i32,
        fitness: &[f64],
        out: &GenerationOutcome,
        new_state: &CemState,
        champion: Weights,
        champion_fitness: f64,
    ) -> Result<f64> {
        let idx: Vec<i32> = (0..fitness.len() as i32).collect();
        sqlx::query(
            "UPDATE training_candidates c SET fitness = f.fitness
               FROM UNNEST($3::int[], $4::float8[]) AS f(idx, fitness)
              WHERE c.run_id = $1 AND c.generation = $2 AND c.idx = f.idx",
        )
        .bind(run_id)
        .bind(generation)
        .bind(&idx)
        .bind(fitness)
        .execute(&mut **tx)
        .await?;
        let (secs,): (f64,) = sqlx::query_as(
            "UPDATE training_generations
                SET mean_fitness = $3, best_fitness = $4, elite_mean_fitness = $5,
                    population_mean_fitness = $6, sigma_mean = $7, finished_at = now()
              WHERE run_id = $1 AND generation = $2
          RETURNING EXTRACT(EPOCH FROM (now() - started_at))::float8",
        )
        .bind(run_id)
        .bind(generation)
        // Infinity is a legal DOUBLE PRECISION in Postgres; NaN is not what
        // we want, so the trainer never produces it.
        .bind(out.mean_fitness)
        .bind(out.best_fitness)
        .bind(out.elite_mean_fitness)
        .bind(out.population_mean_fitness)
        .bind(out.sigma_mean)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE training_runs
                SET state = $2, generation = $3, champion = $4, champion_fitness = $5,
                    last_error = NULL, updated_at = now()
              WHERE id = $1",
        )
        .bind(run_id)
        .bind(Json(new_state))
        .bind(new_state.generation as i32)
        .bind(Json(champion))
        .bind(champion_fitness)
        .execute(&mut **tx)
        .await?;
        Ok(secs)
    }

    /// Create the two validation jobs (champion and heuristic baseline,
    /// same seed so they play identical hands) and link them to the run.
    pub async fn tx_create_validation(
        tx: &mut Tx,
        run_id: i64,
        champion: &JobSpec,
        baseline: &JobSpec,
        hands: i64,
        unit_size: i32,
    ) -> Result<()> {
        let c = Db::create_job_in_tx(
            tx,
            Some(&format!("train#{run_id} validation champion")),
            champion,
            hands,
            unit_size,
            Some(run_id),
        )
        .await?;
        let b = Db::create_job_in_tx(
            tx,
            Some(&format!("train#{run_id} validation baseline")),
            baseline,
            hands,
            unit_size,
            Some(run_id),
        )
        .await?;
        sqlx::query(
            "UPDATE training_runs
                SET validation_job_id = $2, baseline_job_id = $3, status = 'validating',
                    updated_at = now()
              WHERE id = $1",
        )
        .bind(run_id)
        .bind(c)
        .bind(b)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// `(job status)` of each given job, in the same order (`None` = missing).
    pub async fn tx_job_statuses(tx: &mut Tx, ids: &[i64]) -> Result<Vec<Option<String>>> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, status::text FROM jobs WHERE id = ANY($1)")
                .bind(ids)
                .fetch_all(&mut **tx)
                .await?;
        Ok(ids
            .iter()
            .map(|id| rows.iter().find(|(i, _)| i == id).map(|(_, s)| s.clone()))
            .collect())
    }

    /// Mark a run `done` or `failed` (end of validation).
    pub async fn tx_finish_run(
        tx: &mut Tx,
        run_id: i64,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE training_runs
                SET status = $2::training_status, finished_at = now(), updated_at = now(),
                    last_error = COALESCE($3, last_error)
              WHERE id = $1",
        )
        .bind(run_id)
        .bind(status)
        .bind(error)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    // ---- error bookkeeping (own transactions: the step's tx was rolled back)

    /// Remember the latest error on the run so the UI can show it.
    pub async fn record_training_error(&self, run_id: i64, error: &str) -> Result<()> {
        sqlx::query("UPDATE training_runs SET last_error = $2, updated_at = now() WHERE id = $1")
            .bind(run_id)
            .bind(error)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Give up on a run after repeated errors: mark it `failed` and cancel
    /// its jobs so the cluster stops working on it.
    pub async fn fail_training_run(&self, run_id: i64, error: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let failed = sqlx::query(
            "UPDATE training_runs
                SET status = 'failed', last_error = $2, finished_at = now(), updated_at = now()
              WHERE id = $1 AND status IN ('running', 'validating')",
        )
        .bind(run_id)
        .bind(error)
        .execute(&mut *tx)
        .await?;
        if failed.rows_affected() > 0 {
            Self::tx_cancel_run_jobs(&mut tx, run_id).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Slot 0's average points per hand from summed totals, via the shared
/// `learn::fitness` so the cluster and the CLI use literally the same formula.
pub fn slot0_fitness(hands: i64, points: i64) -> f64 {
    let mut stats = UnitStats {
        hands: hands as u64,
        ..UnitStats::default()
    };
    stats.slots[0].points = points as u64;
    hearts_core::learn::fitness(&stats)
}
