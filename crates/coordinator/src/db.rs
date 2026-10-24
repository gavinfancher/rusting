//! The data layer: **every SQL statement in the coordinator lives here.**
//!
//! ## Why Postgres rows, not memory
//! The coordinator keeps no job or lease state in memory. A lease is just
//! three columns on a `work_units` row (`state`, `lease_owner`,
//! `lease_expires_at`). Because the database arbitrates every change:
//!   * restarting the coordinator loses nothing, and
//!   * several coordinator replicas can run side by side safely.
//!
//! ## Concurrency toolbox used below
//! * **`FOR UPDATE SKIP LOCKED`**: "lock the rows I select, and silently skip
//!   rows somebody else has locked". It turns a table into a work queue:
//!   two workers asking at the same moment each get *different* rows instead
//!   of blocking each other or both getting the same row.
//! * **Conditional `UPDATE ... WHERE state = ...`**: the `WHERE` clause is
//!   re-checked by Postgres after it acquires the row lock, so an update
//!   only applies if the row is *still* in the state we expected. That is
//!   how "complete twice" is made idempotent without any explicit locking.
//! * **Lock the job row before deciding the job is finished.** Under
//!   Postgres's default isolation level (READ COMMITTED) a transaction can't
//!   see another in-flight transaction's changes. Two workers finishing the
//!   last two units at the same moment would each see the other's unit as
//!   still `leased`, and neither would finish the job. Taking
//!   `SELECT ... FROM jobs ... FOR UPDATE` first makes them take turns; the
//!   second one then sees the first one's committed result.
//!
//! Lock ordering is always **unit rows first, then the job row**, in every
//! code path, so two transactions can never wait on each other in a cycle
//! (a deadlock).

use crate::metrics::Metrics;
use crate::model::*;
use chrono::{DateTime, Utc};
use hearts_core::{JobSpec, SlotStats, UnitStats};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::types::Json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

mod training;
pub use training::*;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("stored job spec is invalid: {0}")]
    BadSpec(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// `(job_id, total, pending, leased, done, failed, cancelled, hands_done, reclaims)`
type ProgressRow = (i64, i64, i64, i64, i64, i64, i64, i64, i64);
/// `(points, hands, moons, queens, wins)` summed over a job's units for one slot.
type SlotSums = (i64, i64, i64, i64, i64);

/// Seconds after which a worker that hasn't called us counts as inactive
/// (3 x the default 10s heartbeat).
pub const WORKER_ACTIVE_SECONDS: i32 = 30;

/// Open the pool, retrying with exponential backoff: in Kubernetes the
/// database pod may start after we do.
pub async fn connect_with_retry(
    url: &str,
    pool_size: u32,
    attempts: u32,
) -> anyhow::Result<PgPool> {
    let mut delay = Duration::from_millis(500);
    let mut last_err = None;
    for attempt in 1..=attempts {
        match PgPoolOptions::new()
            .max_connections(pool_size)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "database not ready, retrying in {:?}", delay);
                last_err = Some(e);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        }
    }
    Err(anyhow::anyhow!(
        "could not connect to database after {attempts} attempts: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Apply the SQL files in `migrations/`. `sqlx::migrate!` embeds them in the
/// binary at compile time and records applied ones in `_sqlx_migrations`, so
/// running it on every start is safe (and safe with several replicas: sqlx
/// takes an advisory lock while migrating).
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("../../migrations").run(pool).await?;
    Ok(())
}

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
    metrics: Arc<Metrics>,
    lease_seconds: f64,
    max_attempts: i32,
}

// ---- row structs for runtime-checked queries --------------------------------

#[derive(sqlx::FromRow)]
struct JobRow {
    id: i64,
    name: Option<String>,
    status: String,
    spec: Json<JobSpec>,
    num_hands: i64,
    unit_size: i32,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    elapsed_ms: Option<i64>,
    training_run_id: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct LeaseRow {
    id: i64,
    job_id: i64,
    hand_start: i64,
    hand_end: i64,
    attempts: i32,
    lease_expires_at: DateTime<Utc>,
    reclaimed: bool,
    spec: Json<JobSpec>,
}

impl Db {
    pub fn new(pool: PgPool, metrics: Arc<Metrics>, lease_seconds: u32, max_attempts: i32) -> Db {
        Db {
            pool,
            metrics,
            lease_seconds: f64::from(lease_seconds),
            max_attempts,
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Starts a timer that records into `hearts_db_query_duration_seconds`
    /// when dropped (so `let _t = self.timer("lease");` times the rest of the
    /// enclosing scope).
    fn timer(&self, query: &str) -> prometheus::HistogramTimer {
        self.metrics
            .db_duration
            .with_label_values(&[query])
            .start_timer()
    }

    // ---------------------------------------------------------------------
    // Jobs
    // ---------------------------------------------------------------------

    /// Insert a job and **all** its work units in one transaction.
    ///
    /// Units are generated inside Postgres with `generate_series`, so a job
    /// of a million units costs one round trip rather than a million.
    /// Unit `k` covers hands `[k*unit_size, min((k+1)*unit_size, num_hands))`,
    /// so the last unit may be shorter.
    pub async fn create_job(
        &self,
        name: Option<&str>,
        spec: &JobSpec,
        num_hands: i64,
        unit_size: i32,
    ) -> Result<i64> {
        let _t = self.timer("create_job");
        let mut tx = self.pool.begin().await?;
        let job_id =
            Self::create_job_in_tx(&mut tx, name, spec, num_hands, unit_size, None).await?;
        tx.commit().await?;
        self.metrics.jobs_created.inc();
        Ok(job_id)
    }

    /// The body of [`Db::create_job`], taking an open transaction so the
    /// trainer can create a whole generation (many jobs plus its bookkeeping
    /// rows) atomically. The caller commits and bumps `jobs_created`.
    /// `training_run_id` tags jobs that belong to a training run.
    pub async fn create_job_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        name: Option<&str>,
        spec: &JobSpec,
        num_hands: i64,
        unit_size: i32,
        training_run_id: Option<i64>,
    ) -> Result<i64> {
        let (job_id,): (i64,) = sqlx::query_as(
            "INSERT INTO jobs (name, spec, seed, num_hands, unit_size, training_run_id)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(name)
        .bind(Json(spec))
        // u64 seed stored with the same bit pattern in a BIGINT.
        .bind(spec.seed as i64)
        .bind(num_hands)
        .bind(unit_size)
        .bind(training_run_id)
        .fetch_one(&mut **tx)
        .await?;

        // ceil(num_hands / unit_size) units, numbered k = 0 .. count-1.
        sqlx::query(
            "INSERT INTO work_units (job_id, hand_start, hand_end)
             SELECT $1, k * $3, LEAST((k + 1) * $3, $2)
             FROM generate_series(
                      0::bigint,
                      (($2::bigint + $3::bigint - 1) / $3::bigint) - 1
                  ) AS k",
        )
        .bind(job_id)
        .bind(num_hands)
        .bind(i64::from(unit_size))
        .execute(&mut **tx)
        .await?;
        Ok(job_id)
    }

    pub async fn get_job(&self, id: i64) -> Result<Option<Job>> {
        Ok(self.load_jobs(&[id]).await?.into_iter().next())
    }

    /// Newest first.
    /// Jobs created by training runs are hidden unless `include_training`.
    pub async fn list_jobs(&self, limit: i64, include_training: bool) -> Result<Vec<Job>> {
        let ids: Vec<(i64,)> = {
            let _t = self.timer("list_jobs");
            sqlx::query_as(
                "SELECT id FROM jobs
                  WHERE $2 OR training_run_id IS NULL
                  ORDER BY id DESC LIMIT $1",
            )
            .bind(limit)
            .bind(include_training)
            .fetch_all(&self.pool)
            .await?
        };
        let ids: Vec<i64> = ids.into_iter().map(|(i,)| i).collect();
        self.load_jobs(&ids).await
    }

    /// Build full [`Job`] objects for `ids` (order preserved, missing ids
    /// skipped) using three set-based queries regardless of how many jobs
    /// are requested: job rows, per-state unit counts, and standings.
    ///
    /// Optimization candidate: the counts scan every unit of each job. At
    /// millions of units, keep running counters on `jobs` instead.
    async fn load_jobs(&self, ids: &[i64]) -> Result<Vec<Job>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let _t = self.timer("stats");
        let rows: Vec<JobRow> = sqlx::query_as(
            "SELECT id, name, status::text AS status, spec, num_hands, unit_size,
                    created_at, started_at, finished_at, training_run_id,
                    CASE WHEN started_at IS NULL THEN NULL
                         ELSE (EXTRACT(EPOCH FROM (COALESCE(finished_at, now()) - started_at))
                               * 1000)::bigint END AS elapsed_ms
             FROM jobs WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;

        // One pass over the units of all requested jobs. `FILTER (WHERE ..)`
        // is a conditional count; SUM(bigint) yields NUMERIC in Postgres, so
        // cast back to bigint for sqlx.
        let progress: Vec<ProgressRow> = sqlx::query_as(
            "SELECT job_id,
                    count(*),
                    count(*) FILTER (WHERE state = 'pending'),
                    count(*) FILTER (WHERE state = 'leased'),
                    count(*) FILTER (WHERE state = 'done'),
                    count(*) FILTER (WHERE state = 'failed'),
                    count(*) FILTER (WHERE state = 'cancelled'),
                    COALESCE(SUM(hand_end - hand_start) FILTER (WHERE state = 'done'), 0)::bigint,
                    COALESCE(SUM(GREATEST(attempts - 1, 0)), 0)::bigint
             FROM work_units WHERE job_id = ANY($1) GROUP BY job_id",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        let progress: HashMap<i64, Progress> = progress
            .into_iter()
            .map(
                |(job, total, pending, leased, done, failed, cancelled, hands_done, reclaims)| {
                    (
                        job,
                        Progress {
                            units_total: total,
                            pending,
                            leased,
                            done,
                            failed,
                            cancelled,
                            hands_done,
                            reclaims,
                        },
                    )
                },
            )
            .collect();

        // Standings: sum the per-(unit, slot) result rows per job and slot.
        let standing_rows: Vec<(i64, i16, i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT u.job_id, r.slot,
                    SUM(r.points)::bigint, SUM(r.hands)::bigint, SUM(r.moons)::bigint,
                    SUM(r.queens)::bigint, SUM(r.wins)::bigint
             FROM unit_results r JOIN work_units u ON u.id = r.unit_id
             WHERE u.job_id = ANY($1)
             GROUP BY u.job_id, r.slot",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        let mut sums: HashMap<(i64, i16), SlotSums> = HashMap::new();
        for (job, slot, points, hands, moons, queens, wins) in standing_rows {
            sums.insert((job, slot), (points, hands, moons, queens, wins));
        }

        let mut by_id: HashMap<i64, Job> = HashMap::new();
        for r in rows {
            let progress = progress.get(&r.id).cloned().unwrap_or_default();
            let spec = r.spec.0;
            // Always 4 standings, zeros before any unit completes.
            let standings = (0..4u8)
                .map(|slot| {
                    let (points, hands, moons, queens, wins) = sums
                        .get(&(r.id, i16::from(slot)))
                        .copied()
                        .unwrap_or_default();
                    let per_hand = |x: i64| {
                        if hands > 0 {
                            x as f64 / hands as f64
                        } else {
                            0.0
                        }
                    };
                    Standing {
                        slot,
                        strategy: spec.strategies[slot as usize].to_string(),
                        hands,
                        points,
                        avg_points: per_hand(points),
                        moons,
                        queens,
                        wins,
                        win_pct: 100.0 * per_hand(wins),
                    }
                })
                .collect();
            let hands_per_sec = r
                .elapsed_ms
                .filter(|&ms| ms > 0)
                .map(|ms| progress.hands_done as f64 / (ms as f64 / 1000.0));
            by_id.insert(
                r.id,
                Job {
                    id: r.id,
                    name: r.name,
                    status: r.status,
                    spec,
                    num_hands: r.num_hands,
                    unit_size: r.unit_size,
                    created_at: r.created_at,
                    started_at: r.started_at,
                    finished_at: r.finished_at,
                    progress,
                    standings,
                    elapsed_ms: r.elapsed_ms,
                    hands_per_sec,
                    training_run_id: r.training_run_id,
                },
            );
        }
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    /// Cancel a job: its pending and leased units become `cancelled` and the
    /// job `cancelled`. A job already finished is returned unchanged.
    /// `None` if the job doesn't exist.
    ///
    /// Lock order: units first, then the job row (see the module docs).
    pub async fn cancel_job(&self, id: i64) -> Result<Option<Job>> {
        {
            let _t = self.timer("cancel_job");
            let mut tx = self.pool.begin().await?;
            // Workers still computing a cancelled unit learn about it from
            // their next heartbeat (the unit is no longer `leased`).
            sqlx::query(
                "UPDATE work_units
                    SET state = 'cancelled', lease_owner = NULL, lease_expires_at = NULL
                  WHERE job_id = $1 AND state IN ('pending', 'leased')",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            let cancelled = sqlx::query(
                "UPDATE jobs SET status = 'cancelled', finished_at = now()
                  WHERE id = $1 AND status IN ('pending', 'running')",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            if cancelled.rows_affected() > 0 {
                self.metrics
                    .jobs_finished
                    .with_label_values(&["cancelled"])
                    .inc();
            }
        }
        self.get_job(id).await
    }

    // ---------------------------------------------------------------------
    // Leasing
    // ---------------------------------------------------------------------

    /// Hand out up to `max_units` units to `worker_id`.
    ///
    /// The whole thing is ONE statement, built from three parts:
    ///
    /// 1. `picked`: choose candidate rows. A unit is leasable if it is
    ///    `pending`, or `leased` but past its expiry (the worker died or
    ///    stalled), and it has attempts left. `FOR UPDATE SKIP LOCKED` locks
    ///    the chosen rows and skips any row another transaction is already
    ///    leasing, so concurrent callers never get the same unit.
    ///    `ORDER BY job_id, id` drains older jobs first. We also remember
    ///    each row's previous state to detect reclaims.
    /// 2. `leased`: flip those rows to `leased` for this worker, push the
    ///    expiry to `now() + lease`, and bump `attempts`. (`now()` is the
    ///    database's clock, so coordinators and workers need no clock sync.)
    /// 3. `started`: move any `pending` job we just took a unit from to
    ///    `running`. The `status = 'pending'` condition makes only the first
    ///    caller set `started_at`.
    ///
    /// Data-modifying CTEs all run exactly once even if the final SELECT
    /// doesn't read them, and all see the same snapshot. The final SELECT
    /// joins `jobs` just to fetch each job's `spec`.
    pub async fn lease(&self, worker_id: &str, max_units: u32) -> Result<Vec<LeasedUnit>> {
        let rows: Vec<LeaseRow> = {
            let _t = self.timer("lease");
            sqlx::query_as(
                "WITH picked AS (
                     SELECT id, state AS prev_state
                       FROM work_units
                      WHERE (state = 'pending'
                             OR (state = 'leased' AND lease_expires_at < now()))
                        AND attempts < $3
                      ORDER BY job_id, id
                        FOR UPDATE SKIP LOCKED
                      LIMIT $2
                 ), leased AS (
                     UPDATE work_units u
                        SET state = 'leased',
                            lease_owner = $1,
                            lease_expires_at = now() + make_interval(secs => $4),
                            attempts = u.attempts + 1
                       FROM picked p
                      WHERE u.id = p.id
                  RETURNING u.id, u.job_id, u.hand_start, u.hand_end, u.attempts,
                            u.lease_expires_at, (p.prev_state = 'leased') AS reclaimed
                 ), started AS (
                     UPDATE jobs
                        SET status = 'running', started_at = now()
                      WHERE status = 'pending'
                        AND id IN (SELECT job_id FROM leased)
                  RETURNING id
                 )
                 SELECT l.id, l.job_id, l.hand_start, l.hand_end, l.attempts,
                        l.lease_expires_at, l.reclaimed, j.spec
                   FROM leased l JOIN jobs j ON j.id = l.job_id
                  ORDER BY l.id",
            )
            .bind(worker_id)
            .bind(i64::from(max_units))
            .bind(self.max_attempts)
            .bind(self.lease_seconds)
            .fetch_all(&self.pool)
            .await?
        };
        let reclaimed = rows.iter().filter(|r| r.reclaimed).count() as u64;
        self.metrics.units_leased.inc_by(rows.len() as u64);
        self.metrics.units_reclaimed.inc_by(reclaimed);
        Ok(rows
            .into_iter()
            .map(|r| LeasedUnit {
                unit_id: r.id,
                job_id: r.job_id,
                hand_start: r.hand_start as u64,
                hand_end: r.hand_end as u64,
                spec: r.spec.0,
                attempt: r.attempts,
                lease_expires_at: r.lease_expires_at,
                reclaimed: r.reclaimed,
            })
            .collect())
    }

    /// Extend the leases `worker_id` still holds. Returns
    /// `(still_held ids with their new expiry, lost ids)`. A unit is lost if
    /// it is no longer `leased` by this worker: it was reclaimed by someone
    /// else, finished, or its job was cancelled.
    ///
    /// Note an *expired but not yet reclaimed* lease can still be extended:
    /// nobody took it, so the worker may as well keep it.
    pub async fn heartbeat(
        &self,
        worker_id: &str,
        unit_ids: &[i64],
    ) -> Result<(Vec<(i64, DateTime<Utc>)>, Vec<i64>)> {
        let _t = self.timer("heartbeat");
        let held: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
            "UPDATE work_units
                SET lease_expires_at = now() + make_interval(secs => $3)
              WHERE id = ANY($1) AND lease_owner = $2 AND state = 'leased'
          RETURNING id, lease_expires_at",
        )
        .bind(unit_ids)
        .bind(worker_id)
        .bind(self.lease_seconds)
        .fetch_all(&self.pool)
        .await?;
        let lost = unit_ids
            .iter()
            .copied()
            .filter(|id| !held.iter().any(|(h, _)| h == id))
            .collect();
        Ok((held, lost))
    }

    // ---------------------------------------------------------------------
    // Completion
    // ---------------------------------------------------------------------

    /// Record a finished unit. Idempotent and accepted from *any* worker:
    /// simulations are deterministic, so a duplicate result is identical.
    ///
    /// Everything happens in one transaction so a crash can't leave a unit
    /// `done` without its results (or vice versa).
    pub async fn complete_ok(
        &self,
        worker_id: &str,
        unit_id: i64,
        stats: &UnitStats,
        compute_ms: u64,
    ) -> Result<CompleteOutcome> {
        let _t = self.timer("complete");
        let mut tx = self.pool.begin().await?;

        // The guard `state NOT IN ('done','cancelled')` is what makes this
        // idempotent: if two workers report the same unit, Postgres
        // serialises the two UPDATEs on the row lock and the second one finds
        // the row already `done`, so it updates 0 rows. We deliberately also
        // accept `leased` rows owned by someone else and `failed` ones: the
        // work is valid regardless of who did it.
        let updated: Option<(i64,)> = sqlx::query_as(
            "UPDATE work_units
                SET state = 'done', completed_by = $2, completed_at = now(),
                    compute_ms = $3, lease_owner = NULL, lease_expires_at = NULL
              WHERE id = $1 AND state NOT IN ('done', 'cancelled')
          RETURNING job_id",
        )
        .bind(unit_id)
        .bind(worker_id)
        .bind(compute_ms as i64)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((job_id,)) = updated else {
            // Nothing changed; dropping `tx` rolls back.
            self.metrics.units_duplicate.inc();
            return Ok(CompleteOutcome {
                accepted: false,
                job_finished: None,
            });
        };

        // One row per strategy slot, inserted with a single statement by
        // passing parallel arrays and `UNNEST`-ing them into rows.
        let slots: Vec<i16> = (0..4).collect();
        let col = |f: fn(&SlotStats) -> u64| -> Vec<i64> {
            stats.slots.iter().map(|s| f(s) as i64).collect()
        };
        sqlx::query(
            "INSERT INTO unit_results (unit_id, slot, points, hands, moons, queens, wins)
             SELECT $1, * FROM UNNEST($2::smallint[], $3::bigint[], $4::bigint[],
                                      $5::bigint[], $6::bigint[], $7::bigint[])",
        )
        .bind(unit_id)
        .bind(&slots)
        .bind(col(|s| s.points))
        .bind(col(|s| s.hands))
        .bind(col(|s| s.moons))
        .bind(col(|s| s.queens))
        .bind(col(|s| s.wins))
        .execute(&mut *tx)
        .await?;

        let finished = Self::finish_job_if_complete(&mut tx, job_id).await?;
        tx.commit().await?;

        self.metrics.units_completed.inc();
        self.metrics.hands_completed.inc_by(stats.hands);
        if let Some(status) = &finished {
            self.metrics
                .jobs_finished
                .with_label_values(&[status])
                .inc();
        }
        Ok(CompleteOutcome {
            accepted: true,
            job_finished: finished,
        })
    }

    /// Record a failed attempt. The unit goes back to `pending` for another
    /// worker to retry, or to `failed` once `attempts >= max_attempts`.
    ///
    /// Only the *current* lease holder's error is honoured: if the lease
    /// already moved to another worker, this worker's failure says nothing
    /// about the new attempt.
    pub async fn complete_err(
        &self,
        worker_id: &str,
        unit_id: i64,
        error: &str,
    ) -> Result<CompleteOutcome> {
        let _t = self.timer("complete_error");
        let mut tx = self.pool.begin().await?;
        let updated: Option<(i64, String)> = sqlx::query_as(
            "UPDATE work_units
                SET state = (CASE WHEN attempts >= $3 THEN 'failed' ELSE 'pending' END)::unit_state,
                    last_error = $4, lease_owner = NULL, lease_expires_at = NULL
              WHERE id = $1 AND state = 'leased' AND lease_owner = $2
          RETURNING job_id, state::text",
        )
        .bind(unit_id)
        .bind(worker_id)
        .bind(self.max_attempts)
        .bind(error)
        .fetch_optional(&mut *tx)
        .await?;
        self.metrics.units_errored.inc();
        let Some((job_id, new_state)) = updated else {
            return Ok(CompleteOutcome {
                accepted: false,
                job_finished: None,
            });
        };
        let mut finished = None;
        if new_state == "failed" {
            self.metrics.units_failed.inc();
            finished = Self::finish_job_if_complete(&mut tx, job_id).await?;
        }
        tx.commit().await?;
        if let Some(status) = &finished {
            self.metrics
                .jobs_finished
                .with_label_values(&[status])
                .inc();
        }
        Ok(CompleteOutcome {
            accepted: true,
            job_finished: finished,
        })
    }

    /// If the job has no `pending`/`leased` units left, mark it `done` (or
    /// `failed` if any unit failed) and return the new status.
    ///
    /// Step 1 locks the job row so concurrent completions queue up (see the
    /// module docs for why this matters). Step 2 is a separate statement on
    /// purpose: in READ COMMITTED each statement takes a fresh snapshot, so
    /// by the time we get the lock we can see what the previous holder
    /// committed. (A single `UPDATE ... WHERE NOT EXISTS (...)` would
    /// evaluate its subquery with a snapshot from *before* the lock wait.)
    async fn finish_job_if_complete(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job_id: i64,
    ) -> Result<Option<String>> {
        sqlx::query("SELECT id FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(job_id)
            .fetch_optional(&mut **tx)
            .await?;
        let row: Option<(String,)> = sqlx::query_as(
            "UPDATE jobs
                SET status = (CASE WHEN EXISTS (SELECT 1 FROM work_units
                                                 WHERE job_id = $1 AND state = 'failed')
                                   THEN 'failed' ELSE 'done' END)::job_status,
                    finished_at = now()
              WHERE id = $1
                AND status IN ('pending', 'running')
                AND NOT EXISTS (SELECT 1 FROM work_units
                                 WHERE job_id = $1 AND state IN ('pending', 'leased'))
          RETURNING status::text",
        )
        .bind(job_id)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(row.map(|(s,)| s))
    }

    /// Fail units whose lease expired and that have used all their attempts
    /// (nobody can lease them any more, so without this they would sit
    /// `leased` forever and the job would never finish), then finish any
    /// jobs that thereby became complete. Returns how many units failed.
    ///
    /// Safe to run on several replicas at once: the conditional UPDATE only
    /// matches a row once.
    pub async fn sweep_expired(&self) -> Result<u64> {
        let _t = self.timer("sweep");
        let mut tx = self.pool.begin().await?;
        let failed: Vec<(i64,)> = sqlx::query_as(
            "UPDATE work_units
                SET state = 'failed', lease_owner = NULL, lease_expires_at = NULL,
                    last_error = COALESCE(last_error, 'lease expired too many times')
              WHERE state = 'leased' AND lease_expires_at < now() AND attempts >= $1
          RETURNING job_id",
        )
        .bind(self.max_attempts)
        .fetch_all(&mut *tx)
        .await?;
        let mut job_ids: Vec<i64> = failed.iter().map(|(j,)| *j).collect();
        job_ids.sort_unstable();
        job_ids.dedup();
        let mut finished = vec![];
        for job_id in job_ids {
            if let Some(status) = Self::finish_job_if_complete(&mut tx, job_id).await? {
                finished.push(status);
            }
        }
        tx.commit().await?;
        self.metrics.units_failed.inc_by(failed.len() as u64);
        for status in finished {
            self.metrics
                .jobs_finished
                .with_label_values(&[&status])
                .inc();
        }
        Ok(failed.len() as u64)
    }

    // ---------------------------------------------------------------------
    // Workers
    // ---------------------------------------------------------------------

    /// Register a worker or refresh its `last_seen` (upsert).
    pub async fn upsert_worker(
        &self,
        worker_id: &str,
        pod_name: &str,
        node_name: &str,
        threads: i32,
    ) -> Result<()> {
        let _t = self.timer("upsert_worker");
        sqlx::query(
            "INSERT INTO workers (id, pod_name, node_name, threads)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE
                SET pod_name = EXCLUDED.pod_name, node_name = EXCLUDED.node_name,
                    threads = EXCLUDED.threads, last_seen = now()",
        )
        .bind(worker_id)
        .bind(pod_name)
        .bind(node_name)
        .bind(threads)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Heartbeats only carry the worker id, so just refresh `last_seen`.
    pub async fn touch_worker(&self, worker_id: &str) -> Result<()> {
        let _t = self.timer("touch_worker");
        sqlx::query("UPDATE workers SET last_seen = now() WHERE id = $1")
            .bind(worker_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Workers seen within the last hour, active ones first. The
    /// `leased_units` count comes from a grouped subquery over only the
    /// currently-leased rows (served by the partial index).
    pub async fn list_workers(&self) -> Result<Vec<WorkerRow>> {
        let _t = self.timer("list_workers");
        Ok(sqlx::query_as(
            "SELECT w.id AS worker_id, w.pod_name, w.node_name, w.threads,
                    w.first_seen, w.last_seen,
                    (w.last_seen > now() - make_interval(secs => $1)) AS active,
                    COALESCE(l.n, 0)::bigint AS leased_units
               FROM workers w
               LEFT JOIN (SELECT lease_owner, count(*) AS n
                            FROM work_units WHERE state = 'leased'
                           GROUP BY lease_owner) l ON l.lease_owner = w.id
              WHERE w.last_seen > now() - interval '1 hour'
              ORDER BY active DESC, w.last_seen DESC",
        )
        .bind(f64::from(WORKER_ACTIVE_SECONDS))
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn count_active_workers(&self) -> Result<i64> {
        let _t = self.timer("count_workers");
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM workers WHERE last_seen > now() - make_interval(secs => $1)",
        )
        .bind(f64::from(WORKER_ACTIVE_SECONDS))
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    /// Units per state, for the `hearts_units{state}` gauges.
    pub async fn unit_counts(&self) -> Result<Vec<(String, i64)>> {
        let _t = self.timer("unit_counts");
        Ok(
            sqlx::query_as("SELECT state::text, count(*) FROM work_units GROUP BY state")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    // ---------------------------------------------------------------------
    // Benchmarks
    // ---------------------------------------------------------------------

    pub async fn insert_benchmark(&self, b: &NewBenchmark) -> Result<BenchmarkRun> {
        let _t = self.timer("insert_benchmark");
        Ok(sqlx::query_as(
            "INSERT INTO benchmark_runs
                 (experiment, job_id, git_sha, worker_replicas, threads_per_worker, cpu_limit,
                  unit_size, pimc_samples, hands, wall_ms, hands_per_sec, notes)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             RETURNING *",
        )
        .bind(&b.experiment)
        .bind(b.job_id)
        .bind(&b.git_sha)
        .bind(b.worker_replicas)
        .bind(b.threads_per_worker)
        .bind(&b.cpu_limit)
        .bind(b.unit_size)
        .bind(b.pimc_samples)
        .bind(b.hands)
        .bind(b.wall_ms)
        .bind(b.hands_per_sec)
        .bind(&b.notes)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Newest first, optionally filtered by experiment name. `$1 IS NULL OR`
    /// lets one statement serve both the filtered and unfiltered case.
    pub async fn list_benchmarks(
        &self,
        experiment: Option<&str>,
        limit: i64,
    ) -> Result<Vec<BenchmarkRun>> {
        let _t = self.timer("list_benchmarks");
        Ok(sqlx::query_as(
            "SELECT * FROM benchmark_runs
              WHERE ($1::text IS NULL OR experiment = $1)
              ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(experiment)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    // ---------------------------------------------------------------------
    // Health
    // ---------------------------------------------------------------------

    /// Readiness probe: can we run a trivial query within `timeout`?
    pub async fn ping(&self, timeout: Duration) -> bool {
        matches!(
            tokio::time::timeout(timeout, sqlx::query("SELECT 1").execute(&self.pool)).await,
            Ok(Ok(_))
        )
    }
}
