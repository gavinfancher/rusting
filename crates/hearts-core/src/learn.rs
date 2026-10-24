//! Learning by evolution: the cross-entropy method (CEM).
//!
//! Keep a Gaussian "belief" over good weights (a mean and a standard
//! deviation per weight). Each generation: sample a population of weight
//! vectors from it, score each one by playing a tournament (lower average
//! points per hand = fitter), then move the belief toward the best few
//! ("elites"). Repeat. No gradients needed, every candidate is evaluated
//! independently (so it parallelises across the cluster), and everything here
//! is deterministic given the seed.
//!
//! OWNER: Agent G. Pure functions only (no I/O): the CLI and the
//! coordinator both drive this module.

use crate::linear;
use crate::rng::Rng;
use crate::seed::child_seed;
use crate::types::{JobSpec, NUM_WEIGHTS, Rules, StrategyKind, UnitStats, Weights};
use serde::{Deserialize, Serialize};

/// Where the search starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Init {
    /// Start at `linear::default_weights()` (plays like the heuristic).
    Heuristic,
    /// Start at all zeros (plays almost randomly): a dramatic learning curve.
    Zero,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CemConfig {
    /// Candidates per generation (including the mean itself at index 0).
    pub population: usize,
    /// How many of the best candidates the next belief is fitted to.
    pub elites: usize,
    /// 0..1: how far to move toward the elites each generation (1 = all the way).
    pub smoothing: f64,
    /// Floor on every sigma so the search never freezes.
    pub min_sigma: f64,
}

impl Default for CemConfig {
    fn default() -> CemConfig {
        CemConfig {
            population: 24,
            elites: 6,
            smoothing: 0.7,
            min_sigma: 2.0,
        }
    }
}

/// The search's belief after `generation` completed generations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CemState {
    pub generation: u32,
    /// Length NUM_WEIGHTS.
    pub mean: Vec<f64>,
    /// Length NUM_WEIGHTS.
    pub sigma: Vec<f64>,
}

/// Starting belief: mean from `init`, every sigma = `sigma0`.
pub fn initial_state(init: Init, sigma0: f64) -> CemState {
    let mean = match init {
        Init::Heuristic => linear::default_weights()
            .0
            .iter()
            .map(|&w| w as f64)
            .collect(),
        Init::Zero => vec![0.0; NUM_WEIGHTS],
    };
    CemState {
        generation: 0,
        mean,
        sigma: vec![sigma0; NUM_WEIGHTS],
    }
}

/// Weights are clamped to this magnitude so a runaway search can't produce
/// numbers whose scores overflow or look silly.
pub const WEIGHT_LIMIT: f64 = 1_000_000.0;

/// Round and clamp a real-valued vector into integer weights.
///
/// Missing entries (a vector shorter than `NUM_WEIGHTS`) become 0 and extra
/// entries are ignored. `NaN` becomes 0 (`as i32` saturates and maps NaN to 0).
pub fn to_weights(v: &[f64]) -> Weights {
    let mut w = [0i32; NUM_WEIGHTS];
    for (slot, x) in w.iter_mut().zip(v) {
        *slot = x.round().clamp(-WEIGHT_LIMIT, WEIGHT_LIMIT) as i32;
    }
    Weights(w)
}

/// One draw from (approximately) the standard normal distribution N(0, 1).
///
/// Method: the sum of 12 independent uniform(0,1) numbers has mean 6 and
/// variance exactly 1 (each uniform has variance 1/12), and by the central
/// limit theorem it is close to a bell curve. So `sum - 6` is a cheap N(0,1).
/// The tails are cut off at +-6 sigma, which is irrelevant here.
///
/// Why not the textbook Box-Muller (`sqrt(-2 ln u) cos(2 pi v)`)? It needs
/// `ln` and `cos`, and those come from the platform's math library, which may
/// differ in the last bit between macOS, Linux/glibc and musl. A last-bit
/// difference can flip a rounded weight and make the cluster's training
/// diverge from a local run. Here we only use `+`, `-` and `*` on f64, which
/// IEEE 754 defines exactly, so every CPU gets the same bits.
fn normal(rng: &mut Rng) -> f64 {
    let mut sum = 0.0;
    for _ in 0..12 {
        // Top 53 bits of the random word, scaled into [0, 1). Exact in f64.
        sum += (rng.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0);
    }
    sum - 6.0
}

/// CONTRACT: sample `population` candidates. Index 0 is always
/// `to_weights(&state.mean)` (so every generation measures the current
/// belief, which is the learning curve); the rest are mean + sigma·N(0,1),
/// using an RNG seeded from `seed`. Deterministic.
///
/// The draws are consumed in a fixed order (candidate 1, weights 0..24, then
/// candidate 2, ...), so candidate `i` is the same for any population size
/// that includes it.
pub fn sample(state: &CemState, population: usize, seed: u64) -> Vec<Weights> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(population);
    if population == 0 {
        return out;
    }
    out.push(to_weights(&state.mean));
    let mut v = vec![0.0; NUM_WEIGHTS];
    for _ in 1..population {
        for (i, x) in v.iter_mut().enumerate() {
            *x = state.mean[i] + state.sigma[i] * normal(&mut rng);
        }
        out.push(to_weights(&v));
    }
    out
}

/// CONTRACT: given every candidate's fitness (average penalty points per hand;
/// LOWER is better; `f64::INFINITY` for a failed evaluation), fit the new
/// belief to the `cfg.elites` best (mean and std per weight), blend with the
/// old belief by `cfg.smoothing`, floor sigmas at `cfg.min_sigma`, and return
/// it with `generation + 1`. Ties broken by candidate index. Deterministic.
///
/// Details worth knowing:
/// - `NaN` fitness is treated like `INFINITY` (worst).
/// - Elites with infinite fitness are skipped: if fewer than `cfg.elites`
///   candidates succeeded, the belief is fitted to the ones that did. If none
///   did, the belief stays where it was (but the generation counter still
///   advances, so a run can't get stuck).
/// - The elite statistics use the *rounded* integer weights, because those
///   are what was actually evaluated.
/// - The standard deviation divides by the number of elites (not n-1): we
///   describe these particular points, we aren't estimating a population.
pub fn update(state: &CemState, cfg: &CemConfig, scored: &[(Weights, f64)]) -> CemState {
    let key = |f: f64| if f.is_nan() { f64::INFINITY } else { f };
    let mut order: Vec<usize> = (0..scored.len()).collect();
    // Sort by (fitness, index). total_cmp is a total order, so no panics.
    order.sort_by(|&a, &b| {
        key(scored[a].1)
            .total_cmp(&key(scored[b].1))
            .then(a.cmp(&b))
    });
    let elites: Vec<&Weights> = order
        .iter()
        .take(cfg.elites)
        .filter(|&&i| key(scored[i].1).is_finite())
        .map(|&i| &scored[i].0)
        .collect();

    let mut next = CemState {
        generation: state.generation + 1,
        mean: state.mean.clone(),
        sigma: state.sigma.clone(),
    };
    if elites.is_empty() {
        return next;
    }
    let n = elites.len() as f64;
    let keep = 1.0 - cfg.smoothing;
    for i in 0..NUM_WEIGHTS {
        let mut sum = 0.0;
        for w in &elites {
            sum += w.0[i] as f64;
        }
        let elite_mean = sum / n;
        let mut sq = 0.0;
        for w in &elites {
            let d = w.0[i] as f64 - elite_mean;
            sq += d * d;
        }
        let elite_std = (sq / n).sqrt();
        next.mean[i] = keep * state.mean[i] + cfg.smoothing * elite_mean;
        next.sigma[i] = (keep * state.sigma[i] + cfg.smoothing * elite_std).max(cfg.min_sigma);
    }
    next
}

// ----------------------------------------------------------------------------
// Shared evaluation contract (implemented here, used by the CLI and the
// coordinator so local and distributed training give identical results).
// ----------------------------------------------------------------------------

/// CONTRACT: every candidate in generation `g` plays the SAME hands (seed
/// `generation_seed(run_seed, g)`). Comparing candidates on identical deals
/// ("common random numbers") removes most of the luck from the comparison.
pub const fn generation_seed(run_seed: u64, generation: u32) -> u64 {
    child_seed(run_seed, generation as u64)
}

/// CONTRACT: the tournament that scores one candidate: the candidate in slot
/// 0, the three opponents in slots 1..4. Seat rotation (`types::seating`)
/// moves the candidate around the table, so no seat is favoured.
pub fn candidate_spec(
    candidate: Weights,
    opponents: [StrategyKind; 3],
    seed: u64,
    rules: Rules,
) -> JobSpec {
    JobSpec {
        seed,
        strategies: [
            StrategyKind::Linear(candidate),
            opponents[0],
            opponents[1],
            opponents[2],
        ],
        rules,
    }
}

/// CONTRACT: fitness = slot 0's average penalty points per hand. Lower is
/// better. A heuristic bot against 3 heuristics scores ~6.5 (26 / 4).
pub fn fitness(stats: &UnitStats) -> f64 {
    if stats.hands == 0 {
        f64::INFINITY
    } else {
        stats.slots[0].points as f64 / stats.hands as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(elites: usize, smoothing: f64, min_sigma: f64) -> CemConfig {
        CemConfig {
            population: 8,
            elites,
            smoothing,
            min_sigma,
        }
    }

    fn w(x: i32) -> Weights {
        Weights([x; NUM_WEIGHTS])
    }

    #[test]
    fn initial_states() {
        let z = initial_state(Init::Zero, 20.0);
        assert_eq!(z.generation, 0);
        assert!(z.mean.iter().all(|&m| m == 0.0));
        assert!(z.sigma.iter().all(|&s| s == 20.0));
        let h = initial_state(Init::Heuristic, 5.0);
        assert_eq!(to_weights(&h.mean), linear::default_weights());
    }

    #[test]
    fn to_weights_rounds_clamps_and_survives_nan() {
        let mut v = vec![0.0; NUM_WEIGHTS];
        v[0] = 2.5; // round half away from zero
        v[1] = -2.5;
        v[2] = 1e12;
        v[3] = -1e12;
        v[4] = f64::NAN;
        v[5] = 0.49;
        let w = to_weights(&v);
        assert_eq!(&w.0[..6], &[3, -3, 1_000_000, -1_000_000, 0, 0]);
    }

    #[test]
    fn sample_is_deterministic_and_candidate_zero_is_the_mean() {
        let mut s = initial_state(Init::Heuristic, 20.0);
        s.mean[3] = 7.4;
        let a = sample(&s, 10, 99);
        let b = sample(&s, 10, 99);
        assert_eq!(a, b);
        assert_eq!(a.len(), 10);
        assert_eq!(a[0], to_weights(&s.mean));
        assert_ne!(a, sample(&s, 10, 100));
        // A smaller population is a prefix of a larger one.
        assert_eq!(&sample(&s, 4, 99)[..], &a[..4]);
        assert!(sample(&s, 0, 1).is_empty());
    }

    #[test]
    fn normal_draws_look_standard_normal() {
        let mut rng = Rng::new(5);
        let n = 100_000;
        let xs: Vec<f64> = (0..n).map(|_| normal(&mut rng)).collect();
        let mean = xs.iter().sum::<f64>() / n as f64;
        let var = xs.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((var - 1.0).abs() < 0.03, "var {var}");
    }

    #[test]
    fn update_moves_toward_elites() {
        let state = initial_state(Init::Zero, 20.0);
        // Candidates 1 and 2 are best and sit at +10 / +20; the rest are bad.
        let scored = vec![
            (w(0), 9.0),
            (w(10), 5.0),
            (w(20), 6.0),
            (w(-30), 8.0),
            (w(-40), 9.5),
        ];
        let next = update(&state, &cfg(2, 0.5, 2.0), &scored);
        assert_eq!(next.generation, 1);
        // Elite mean = 15; halfway from 0 is 7.5.
        assert!(next.mean.iter().all(|&m| (m - 7.5).abs() < 1e-12));
        // Elite std = 5; halfway from 20 to 5 is 12.5.
        assert!(next.sigma.iter().all(|&s| (s - 12.5).abs() < 1e-12));
        // smoothing 1 jumps all the way.
        let full = update(&state, &cfg(2, 1.0, 2.0), &scored);
        assert!(full.mean.iter().all(|&m| (m - 15.0).abs() < 1e-12));
    }

    #[test]
    fn update_floors_sigma() {
        let state = initial_state(Init::Zero, 20.0);
        // One elite => std 0 => sigma would collapse to 0 without the floor.
        let scored = vec![(w(4), 1.0), (w(9), 2.0)];
        let next = update(&state, &cfg(1, 1.0, 2.0), &scored);
        assert!(next.sigma.iter().all(|&s| s == 2.0));
        assert!(next.mean.iter().all(|&m| m == 4.0));
    }

    #[test]
    fn update_treats_infinity_and_nan_as_worst() {
        let state = initial_state(Init::Zero, 20.0);
        let scored = vec![
            (w(100), f64::INFINITY),
            (w(8), 5.0),
            (w(-100), f64::NAN),
            (w(12), 6.0),
        ];
        // Three elites requested but only two candidates succeeded.
        let next = update(&state, &cfg(3, 1.0, 1.0), &scored);
        assert!(next.mean.iter().all(|&m| (m - 10.0).abs() < 1e-12));
        // Everything failed: belief unchanged, generation still advances.
        let all_bad = vec![(w(1), f64::INFINITY), (w(2), f64::INFINITY)];
        let same = update(&state, &cfg(1, 1.0, 1.0), &all_bad);
        assert_eq!(same.generation, 1);
        assert_eq!(same.mean, state.mean);
        assert_eq!(same.sigma, state.sigma);
    }

    #[test]
    fn update_breaks_ties_by_candidate_index() {
        let state = initial_state(Init::Zero, 20.0);
        let scored = vec![(w(10), 5.0), (w(20), 5.0), (w(30), 5.0)];
        let next = update(&state, &cfg(2, 1.0, 1.0), &scored);
        assert!(next.mean.iter().all(|&m| (m - 15.0).abs() < 1e-12));
    }
}
