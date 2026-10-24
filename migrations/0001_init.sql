-- Initial schema. Postgres is the single source of truth: if the coordinator
-- dies, nothing is lost. Applied at coordinator startup by sqlx::migrate!.
--
-- u64 values from Rust (seeds) are stored as BIGINT with the same bit
-- pattern (`as i64` / `as u64`), so large seeds show up negative in psql.

CREATE TYPE job_status AS ENUM ('pending', 'running', 'done', 'failed', 'cancelled');
CREATE TYPE unit_state AS ENUM ('pending', 'leased', 'done', 'failed', 'cancelled');

CREATE TABLE jobs (
    id           BIGSERIAL PRIMARY KEY,
    name         TEXT,
    -- 'pending' until the first unit is leased, then 'running'. When no units
    -- are pending or leased: 'done' if none failed, otherwise 'failed'.
    status       job_status NOT NULL DEFAULT 'pending',
    -- hearts_core::JobSpec as JSON: {"seed":..,"strategies":[4],"rules":{..}}
    spec         JSONB NOT NULL,
    seed         BIGINT NOT NULL,
    num_hands    BIGINT NOT NULL CHECK (num_hands > 0),
    unit_size    INT    NOT NULL CHECK (unit_size > 0),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ
);

CREATE TABLE work_units (
    id               BIGSERIAL PRIMARY KEY,
    job_id           BIGINT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    hand_start       BIGINT NOT NULL,
    hand_end         BIGINT NOT NULL,
    state            unit_state NOT NULL DEFAULT 'pending',
    lease_owner      TEXT,
    lease_expires_at TIMESTAMPTZ,
    -- Incremented on every lease. attempts - 1 = number of reclaims/retries.
    attempts         INT NOT NULL DEFAULT 0,
    last_error       TEXT,
    compute_ms       BIGINT,
    completed_by     TEXT,
    completed_at     TIMESTAMPTZ,
    CHECK (hand_end > hand_start)
);

-- The lease query scans only leasable units: pending, or leased but expired.
CREATE INDEX work_units_leasable ON work_units (state, lease_expires_at, id)
    WHERE state IN ('pending', 'leased');
CREATE INDEX work_units_job_state ON work_units (job_id, state);

-- One row per (unit, strategy slot). Standings = SUM over a job's units.
CREATE TABLE unit_results (
    unit_id BIGINT   NOT NULL REFERENCES work_units (id) ON DELETE CASCADE,
    slot    SMALLINT NOT NULL CHECK (slot BETWEEN 0 AND 3),
    points  BIGINT   NOT NULL,
    hands   BIGINT   NOT NULL,
    moons   BIGINT   NOT NULL,
    queens  BIGINT   NOT NULL,
    wins    BIGINT   NOT NULL,
    PRIMARY KEY (unit_id, slot)
);

-- Upserted on every LeaseWork/Heartbeat call.
CREATE TABLE workers (
    id         TEXT PRIMARY KEY,
    pod_name   TEXT NOT NULL,
    node_name  TEXT NOT NULL,
    threads    INT  NOT NULL,
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per scaling experiment run (scripts/bench). Compare runs over time.
CREATE TABLE benchmark_runs (
    id                 BIGSERIAL PRIMARY KEY,
    experiment         TEXT NOT NULL,          -- e.g. 'E2-horizontal'
    job_id             BIGINT REFERENCES jobs (id) ON DELETE SET NULL,
    git_sha            TEXT,
    worker_replicas    INT,
    threads_per_worker INT,
    cpu_limit          TEXT,                   -- as written in k8s, e.g. '2' or '1500m'
    unit_size          INT,
    pimc_samples       INT,
    hands              BIGINT,
    wall_ms            BIGINT,
    hands_per_sec      DOUBLE PRECISION,
    notes              TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
