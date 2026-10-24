# Agent H: Training service (coordinator trainer, API, UI, dashboard)

**Owns:**
- New files `crates/coordinator/src/trainer.rs`, `crates/coordinator/src/http/training.rs`, and new test files in `crates/coordinator/tests/`.
- Edits to existing coordinator files only where needed to wire things in: `db.rs` (add training queries; the jobs list filter; the `training_run_id` field; `champion:` resolution), `http/mod.rs` (routes), `http/jobs.rs` (`champion:` alias, `include_training`), `metrics.rs`, `background.rs` or `main.rs` (spawn the trainer), `model.rs`.
- `web/training.html`, plus a nav link added in the other pages, small additions to `web/app.js`, `web/mock.js` and `web/style.css`.
- `deploy/monitoring/dashboards/hearts-training.json` plus the dashboards `kustomization.yaml` entry.
- A new drill `scripts/gamedays/11-kill-coordinator-mid-training.sh` plus a section appended to `docs/GAMEDAYS.md`.

**Must not edit:** `hearts-core`, `migrations/0001*` or `0002*`, `proto/`, the worker.

## Goal
Run CEM training **on the cluster**: the coordinator turns each generation into `population` ordinary tournament jobs, which the existing workers chew through. All state lives in Postgres (`migrations/0002_training.sql`), so killing the coordinator mid-training resumes cleanly, and the result is **bit-for-bit identical** to `hearts-cli train` with the same config and seed.

## Read first
`docs/CONTRACTS.md`, especially the new "Training" section, which is the spec. Also `migrations/0002_training.sql`, `crates/hearts-core/src/learn.rs` (the API you call: `initial_state`, `sample`, `update`, `to_weights`, `generation_seed`, `candidate_spec`, `fitness`, and the `CemConfig`/`CemState`/`Init` types), `crates/hearts-core/src/types.rs` (`StrategyKind::Linear`, `Weights`, `FEATURE_NAMES`), and the existing coordinator code (`db.rs` job creation and standings, `background.rs`, `http/jobs.rs`, the tests).

`learn::{initial_state, sample, update, to_weights}` and the linear bot are being implemented **in parallel by Agent G** and are `todo!()` in your worktree.
- Write the trainer against those signatures.
- In tests, inject a small **local test double**: define a trait or function pointers in `trainer.rs` with a default that calls `hearts_core::learn`, plus a simple deterministic fake for tests.
- Workers aren't running in your tests, so drive the jobs yourself: lease the units through the DB layer and complete them with fabricated `UnitStats`, so fitness is controlled. Assert that fitness, state progression, the champion and validation are written correctly.

## Build
1. **DB:** queries for runs, candidates and generations. Create-generation (candidates plus their jobs plus the generation row) happens in ONE transaction using your existing job-creation code (refactor it to accept a `&mut Transaction` if needed). There's a query for "candidate job states and slot-0 stats for generation g", the update writes, the validation jobs, cancel (cascading to the run's non-terminal jobs), and the list and get with their aggregates.
2. **Trainer** (`trainer.rs`): a tokio task ticking every 2s (configurable `TRAINER_TICK_MS`) that runs the state machine in CONTRACTS.md. Use `pg_try_advisory_xact_lock(42, run_id)` and skip the run if the lock isn't acquired. Make every step idempotent. Errors get logged and written to `last_error`; after 5 consecutive errors on a run, mark it `failed`. Update the training metrics. Comment the design heavily, since the owner will learn from it: why advisory locks, why one transaction, why it's safe to crash anywhere.
3. **HTTP:** `POST/GET /api/training`, `GET /api/training/:id`, `POST /api/training/:id/cancel`, exactly the JSON in CONTRACTS.md. On `POST /api/jobs`, a `champion:<id>` strategy is resolved before validation. `GET /api/jobs` hides training jobs unless `include_training=true`; the Job JSON gains `training_run_id`.
4. **UI** (`web/training.html`, vanilla JS like the rest, also working in mock mode with fixtures in `mock.js`):
   - **Start form:** the main fields (name, population, generations, hands per candidate, init, opponents, seed), with an "advanced" disclosure for the rest.
   - **Runs list.**
   - **Run detail:**
     - Progress: generation `x/y`, a bar for candidates done, hands/sec.
     - **Learning curve** (inline SVG): `mean_fitness` and `best_fitness` per generation, plus a dashed horizontal line at the heuristic baseline (6.5, or `validation.baseline_fitness` once it exists), with the y-axis labelled "avg points / hand (lower is better)".
     - A champion card with the strategy string and a copy button.
     - A weights table of the 24 features, with bars for positive and negative values and plain-English hints for each feature name.
     - Validation result: "champion 5.88 vs heuristic 6.49 on the same 200k hands".
     - A cancel button.
   - The live view polls `GET /api/training/:id` every 2s; no SSE is needed.
   - Add "Training" to the nav on every page. On the tournament page, let the strategy select accept a "champion of run N" option by typing `champion:N` in a free-text strategy field (keep it simple).
5. **Dashboard** `hearts-training.json` (same conventions as the others):
   - fitness per generation: `hearts_training_latest_fitness` and `hearts_training_latest_best_fitness` over time;
   - generations/hour;
   - generation duration p50/p95;
   - active runs;
   - cluster hands/sec;
   - units pending.
   
   Add it to the dashboards `kustomization.yaml`.
6. **Drill 11:** start a small training run, kill the coordinator pod mid-generation, wait for it to finish, then compare the champion against `hearts-cli train` with the same config and seed (print the command for the owner to run locally; the script can compare if `hearts-cli` is on PATH). Use the same style and `lib.sh` helpers as the other drills, and append a section to `docs/GAMEDAYS.md`.

## Tests (testcontainers postgres:18.4, like the existing ones)
- `POST /api/training` applies the defaults and validation (400s).
- Creating a generation produces `population` jobs, all with the same seed, slot 0 = `linear:` and slots 1–3 = the opponents, tagged `training_run_id`.
- Completing all the jobs with fabricated stats makes the next tick write fitness and generation stats, advance the state, and set the champion.
- After the last generation, validation jobs are created; completing them sets status `done` and fills `validation`.
- Resumption: tick partway, drop the trainer and reconnect, tick again, and no duplicate candidates or jobs appear.
- Advisory lock: two concurrent ticks on the same run create exactly one generation.
- Cancel cascades to the jobs.
- `champion:<id>` resolves; a run with no champion yet gives a 400.
- `include_training` filtering works.

## Acceptance
```
cargo test -p coordinator
cargo clippy --workspace --all-targets -- -D warnings
kubectl kustomize deploy/monitoring/dashboards > /dev/null
python3 -m http.server -d web 8000   # check /training.html?mock=1 renders (headless Chrome via CDP if available, like Agent D did)
bash -n scripts/gamedays/11-kill-coordinator-mid-training.sh
```
