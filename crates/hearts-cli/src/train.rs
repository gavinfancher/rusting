//! Local CEM training: the same algorithm the cluster's trainer task runs
//! (see "Training" in docs/CONTRACTS.md), driven by a plain loop.
//!
//! Every step is deterministic, so for the same configuration and seed this
//! produces the same champion weights regardless of the thread count, and the
//! same weights a cluster run would produce.

use hearts_core::learn::{
    CemConfig, CemState, Init, candidate_spec, fitness, generation_seed, initial_state, sample,
    to_weights, update,
};
use hearts_core::seed::child_seed;
use hearts_core::tournament::run_hands;
use hearts_core::types::{JobSpec, NUM_WEIGHTS, Rules, StrategyKind, UnitStats, Weights};
use serde::Serialize;
use std::time::Instant;

/// Everything that defines a training run.
#[derive(Clone, Debug)]
pub struct TrainConfig {
    pub cem: CemConfig,
    pub generations: u32,
    pub hands: u64,
    pub opponents: [StrategyKind; 3],
    pub init: Init,
    pub sigma0: f64,
    pub seed: u64,
    pub validation_hands: u64,
    pub rules: Rules,
}

/// Statistics of one finished generation.
#[derive(Clone, Debug, Serialize)]
pub struct GenerationStats {
    pub generation: u32,
    pub seed: u64,
    /// Fitness of candidate 0 = the current belief's mean: the learning curve.
    pub mean_fitness: f64,
    pub best_fitness: f64,
    pub elite_mean_fitness: f64,
    pub population_mean_fitness: f64,
    pub sigma_mean: f64,
    pub elapsed_ms: u64,
}

/// Champion vs the baseline heuristic on identical hands.
#[derive(Clone, Debug, Serialize)]
pub struct Validation {
    pub hands: u64,
    pub champion_fitness: f64,
    pub baseline_fitness: f64,
    /// champion - baseline (negative = the champion is better).
    pub difference: f64,
    /// Standard error of `difference` estimated from paired blocks of hands.
    pub difference_stderr: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrainResult {
    pub generations: Vec<GenerationStats>,
    pub champion: Weights,
    pub champion_strategy: String,
    pub validation: Option<Validation>,
}

/// Mean of the best `k` finite values; NaN if there are none.
fn elite_mean(fits: &[f64], k: usize) -> f64 {
    let mut v: Vec<f64> = fits.iter().copied().filter(|f| f.is_finite()).collect();
    v.sort_by(f64::total_cmp);
    v.truncate(k);
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// Run the whole search. `on_generation` is called after each generation (for
/// progress output). Call it inside the rayon pool you want to use
/// (`pool.install(|| train(..))`).
pub fn train(
    cfg: &TrainConfig,
    mut on_generation: impl FnMut(&GenerationStats, &CemState),
) -> TrainResult {
    let mut state = initial_state(cfg.init, cfg.sigma0);
    let mut gens = Vec::new();
    for g in 0..cfg.generations {
        let t0 = Instant::now();
        let seed_g = generation_seed(cfg.seed, g);
        let candidates = sample(&state, cfg.cem.population, seed_g);
        // All candidates play the same hands (same seed): common random numbers.
        let scored: Vec<(Weights, f64)> = candidates
            .iter()
            .map(|&c| {
                let spec = candidate_spec(c, cfg.opponents, seed_g, cfg.rules);
                (c, fitness(&run_hands(&spec, 0..cfg.hands)))
            })
            .collect();
        let fits: Vec<f64> = scored.iter().map(|s| s.1).collect();
        let finite: Vec<f64> = fits.iter().copied().filter(|f| f.is_finite()).collect();
        let stats = GenerationStats {
            generation: g,
            seed: seed_g,
            mean_fitness: fits.first().copied().unwrap_or(f64::NAN),
            best_fitness: finite.iter().copied().fold(f64::INFINITY, f64::min),
            elite_mean_fitness: elite_mean(&fits, cfg.cem.elites),
            population_mean_fitness: finite.iter().sum::<f64>() / finite.len().max(1) as f64,
            sigma_mean: state.sigma.iter().sum::<f64>() / NUM_WEIGHTS as f64,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        };
        state = update(&state, &cfg.cem, &scored);
        on_generation(&stats, &state);
        gens.push(stats);
    }
    let champion = to_weights(&state.mean);
    let validation = (cfg.validation_hands > 0).then(|| validate(cfg, champion));
    TrainResult {
        generations: gens,
        champion,
        champion_strategy: StrategyKind::Linear(champion).to_string(),
        validation,
    }
}

/// Champion vs the plain heuristic in the same seat of the same opponent
/// table, on the same hands. To get a standard error on the difference, the
/// hands are split into ~20 blocks (each a multiple of 96, the full seating
/// cycle, so each block is balanced); both strategies play identical blocks,
/// so the per-block differences are paired.
pub fn validate(cfg: &TrainConfig, champion: Weights) -> Validation {
    let seed = child_seed(cfg.seed, 1_000_000);
    let champ_spec = candidate_spec(champion, cfg.opponents, seed, cfg.rules);
    let base_spec = JobSpec {
        strategies: [
            StrategyKind::Heuristic,
            cfg.opponents[0],
            cfg.opponents[1],
            cfg.opponents[2],
        ],
        ..champ_spec.clone()
    };
    let n = cfg.validation_hands;
    let block = ((n / 20) / 96 * 96).max(96);
    let (mut champ_tot, mut base_tot) = (UnitStats::default(), UnitStats::default());
    let mut diffs = Vec::new();
    let mut start = 0;
    while start < n {
        let end = (start + block).min(n);
        let c = run_hands(&champ_spec, start..end);
        let b = run_hands(&base_spec, start..end);
        diffs.push(fitness(&c) - fitness(&b));
        champ_tot.merge(&c);
        base_tot.merge(&b);
        start = end;
    }
    let (cf, bf) = (fitness(&champ_tot), fitness(&base_tot));
    // Standard error of the mean of the block differences.
    let k = diffs.len() as f64;
    let mean = diffs.iter().sum::<f64>() / k;
    let var = diffs.iter().map(|d| (d - mean) * (d - mean)).sum::<f64>() / (k - 1.0).max(1.0);
    Validation {
        hands: n,
        champion_fitness: cf,
        baseline_fitness: bf,
        difference: cf - bf,
        difference_stderr: (var / k).sqrt(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> TrainConfig {
        TrainConfig {
            cem: CemConfig {
                population: 8,
                elites: 3,
                smoothing: 0.7,
                min_sigma: 2.0,
            },
            generations: 3,
            hands: 2000,
            opponents: [StrategyKind::Heuristic; 3],
            init: Init::Zero,
            sigma0: 20.0,
            seed: 7,
            validation_hands: 1920,
            rules: Rules::default(),
        }
    }

    fn run_with(threads: usize) -> TrainResult {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        pool.install(|| train(&tiny(), |_, _| {}))
    }

    /// The cluster reproduces local runs only if the thread count (and so the
    /// order in which hands finish) cannot change the result.
    #[test]
    fn training_is_identical_on_1_and_4_threads() {
        let a = run_with(1);
        let b = run_with(4);
        assert_eq!(a.champion, b.champion);
        assert_eq!(a.champion_strategy, b.champion_strategy);
        for (x, y) in a.generations.iter().zip(&b.generations) {
            assert_eq!(x.mean_fitness.to_bits(), y.mean_fitness.to_bits());
            assert_eq!(x.best_fitness.to_bits(), y.best_fitness.to_bits());
        }
        let (va, vb) = (a.validation.unwrap(), b.validation.unwrap());
        assert_eq!(va.champion_fitness.to_bits(), vb.champion_fitness.to_bits());
    }

    /// Starting from all-zero weights (a nearly random player), a few
    /// generations should visibly improve the mean. Slow in debug builds.
    #[test]
    #[ignore = "slow in debug; run with --release -- --ignored"]
    fn zero_init_learns() {
        let mut cfg = tiny();
        cfg.generations = 5;
        cfg.hands = 4000;
        cfg.cem.population = 12;
        cfg.validation_hands = 0;
        let r = train(&cfg, |_, _| {});
        let first = r.generations.first().unwrap().mean_fitness;
        let last = r.generations.last().unwrap().mean_fitness;
        assert!(last < first - 0.3, "no learning: {first} -> {last}");
    }
}
