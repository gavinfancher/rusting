-- Training (Level 1 learning): evolve the weights of the `linear` bot with
-- the cross-entropy method. Each generation = `population` candidate weight
-- vectors, each scored by an ordinary tournament job. All state lives here,
-- so a coordinator restart mid-training just resumes.

CREATE TYPE training_status AS ENUM ('running', 'validating', 'done', 'cancelled', 'failed');

CREATE TABLE training_runs (
    id                 BIGSERIAL PRIMARY KEY,
    name               TEXT,
    status             training_status NOT NULL DEFAULT 'running',
    -- The POST /api/training body after defaults are applied (see CONTRACTS.md).
    config             JSONB  NOT NULL,
    seed               BIGINT NOT NULL,
    -- hearts_core::learn::CemState: the current belief (mean/sigma per weight).
    state              JSONB  NOT NULL,
    -- Completed generations (= state.generation).
    generation         INT    NOT NULL DEFAULT 0,
    -- to_weights(mean) after the latest completed generation, and that
    -- candidate's measured fitness (candidate 0 of the latest generation).
    champion           JSONB,
    champion_fitness   DOUBLE PRECISION,
    -- Final head-to-head check on fresh hands: champion vs the opponents, and
    -- the plain heuristic vs the same opponents on the SAME hands.
    validation_job_id  BIGINT REFERENCES jobs (id) ON DELETE SET NULL,
    baseline_job_id    BIGINT REFERENCES jobs (id) ON DELETE SET NULL,
    last_error         TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at        TIMESTAMPTZ
);

-- One row per candidate per generation. `job_id` is the tournament scoring it.
CREATE TABLE training_candidates (
    run_id     BIGINT NOT NULL REFERENCES training_runs (id) ON DELETE CASCADE,
    generation INT    NOT NULL,
    idx        INT    NOT NULL,          -- 0 = the current mean
    weights    JSONB  NOT NULL,          -- hearts_core::Weights (int array)
    job_id     BIGINT REFERENCES jobs (id) ON DELETE SET NULL,
    fitness    DOUBLE PRECISION,         -- NULL until the job finishes
    PRIMARY KEY (run_id, generation, idx)
);

-- One row per generation: the learning curve.
CREATE TABLE training_generations (
    run_id                  BIGINT NOT NULL REFERENCES training_runs (id) ON DELETE CASCADE,
    generation              INT    NOT NULL,
    seed                    BIGINT NOT NULL,
    mean_fitness            DOUBLE PRECISION,   -- candidate 0 (the belief's mean)
    best_fitness            DOUBLE PRECISION,
    elite_mean_fitness      DOUBLE PRECISION,
    population_mean_fitness DOUBLE PRECISION,   -- over finite fitnesses
    sigma_mean              DOUBLE PRECISION,   -- avg sigma of the belief used to sample
    started_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at             TIMESTAMPTZ,
    PRIMARY KEY (run_id, generation)
);

-- Tag tournament jobs created by training, so the jobs list can hide them.
ALTER TABLE jobs ADD COLUMN training_run_id BIGINT REFERENCES training_runs (id) ON DELETE SET NULL;
CREATE INDEX jobs_training_run ON jobs (training_run_id) WHERE training_run_id IS NOT NULL;
