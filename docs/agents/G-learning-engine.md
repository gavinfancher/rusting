# Agent G: Learning engine (linear bot + CEM + CLI)

**Owns:** `crates/hearts-core/src/{linear,learn}.rs` (the bodies of the stubbed functions; keep the generation_seed/candidate_spec/fitness helpers in learn.rs exactly as they are), new tests and benches under `crates/hearts-core/`, `crates/hearts-cli/`, and a new `docs/LEARNING.md`. You may add a `linear/hand` benchmark to `crates/hearts-core/benches/engine.rs`.
**Must not edit:** `types.rs` (the `FEATURE_NAMES` order and meaning are a contract), `card.rs`, `seed.rs`, the other engine files, or anything outside your paths.

## Goal
Make the bot **learn**. `StrategyKind::Linear(Weights)` scores each candidate card as Σ weight × feature and plays the best. CEM (`learn.rs`) evolves the weights. The owner should be able to watch a learning curve: avg points per hand against 3 heuristic opponents dropping generation by generation, ideally below the heuristic's own ~6.5.

## Build
1. **`linear.rs`:**
   - Compute every feature in `FEATURE_NAMES` order, as small integers. Document the exact scale of each one in a table at the top of the file. Suggested scale: booleans 0/1×100, ranks 0..=12 ×8 (≈0..96), counts ×8, points ×7.
   - Keep the magnitudes comparable so one sigma suits every weight.
   - `linear_play`: score every legal card (iterate `legal_moves(view)`, no allocation, i64 accumulation) and pick the highest; ties go to the lowest card index.
   - `linear_pass`: score every card in hand with the pass features and take the top 3.
   - Reuse the "would this card win the trick" logic from `heuristic_play` (read strategy.rs). "Unseen" means `view.unseen()`.
   - `default_weights()`: hand-pick weights so `linear` plays roughly like `heuristic`. Measure it with the CLI against 3 heuristics, aiming within about 0.5 avg points of the heuristic-vs-heuristic baseline (~6.5). Record what you got.
2. **`learn.rs`:** implement `initial_state`, `to_weights` (round, clamp to ±1,000,000), `sample` (use `crate::rng::Rng` seeded from `seed` with Box-Muller or similar for N(0,1), document it, and make it deterministic), and `update` (sort by (fitness, idx); elites = first `cfg.elites`; the new mean and std per weight blend toward the elite statistics by `smoothing`; floor sigma at `min_sigma`; generation+1; treat INFINITY as worst).
3. **CLI** `hearts-cli train`:
   - Flags: `--population 24 --elites 6 --generations 30 --hands 40000 --opponents heuristic,heuristic,heuristic --init heuristic|zero --sigma0 20 --smoothing 0.7 --min-sigma 2 --seed 1 --threads N --validation-hands 200000 [--json]`.
   - Implement the exact algorithm in CONTRACTS.md "Training", using `learn::generation_seed`, `candidate_spec`, `run_hands(spec, 0..hands)` and `fitness`.
   - Each generation prints a line: gen, mean_fitness (candidate 0), best, elite mean, sigma_mean, elapsed. At the end it prints the champion as a `linear:...` string, the per-feature weights table, and the validation result (champion vs baseline heuristic on the same hands).
   - `hearts-cli features --strategy linear:...` pretty-prints the named weights.
4. **Tests:**
   - linear bot plays only legal cards (proptest over random games with linear vs random);
   - `default_weights` round-trips through the text form;
   - `sample` is deterministic and candidate 0 equals the mean;
   - `update` moves the mean toward the elites and respects min_sigma and INFINITY;
   - a tiny end-to-end CEM run (population 8, 3 generations, 2000 hands) is deterministic across 1 and 4 threads, which proves the cluster will reproduce local runs;
   - a learning smoke test: `init zero` for about 5 generations improves mean_fitness (mark it `#[ignore]` if it's slow in debug).
5. **Benchmark:** linear hands/sec compared with heuristic. Add a row to `docs/OPTIMIZATION_LOG.md` only if you can append without restructuring it, since that file is shared; otherwise report it.
6. **Real run and calibration.** Run `cargo run --release -p hearts-cli -- train --init zero --generations 25 ...` and `--init heuristic --generations 25 ...` on 12 threads with a hand count that gives a stable signal. Fitness noise should be below the improvements: estimate the standard error of avg points at N hands from per-generation numbers and pick N accordingly. Report the curves and the chosen defaults. If the default `hands_per_candidate` or `sigma0` in CONTRACTS.md looks wrong, say so in your report with numbers (don't edit CONTRACTS.md).
7. **`docs/LEARNING.md`** for the owner (new to Rust and ML):
   - what is being learned, and the features in plain English;
   - CEM intuition with a small diagram;
   - why common random numbers (one seed per generation);
   - why it's deterministic, and why that matters for distributed training;
   - how the cluster runs it (one generation = `population` tournament jobs; refer to CONTRACTS.md "Training");
   - your measured local learning curves (a table, plus an ASCII sparkline is fine);
   - how to read the learned weights (which features matter);
   - how to try it (CLI and UI/API);
   - ideas for Level 2.

## Acceptance
```
cargo test -p hearts-core && cargo test -p hearts-cli
cargo clippy --workspace --all-targets -- -D warnings
cargo run --release -p hearts-cli -- train --init zero --population 16 --generations 10 --hands 20000 --seed 1 --threads 8
cargo run --release -p hearts-cli -- train --init zero --population 16 --generations 10 --hands 20000 --seed 1 --threads 2   # identical champion
```
