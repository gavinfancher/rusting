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
    let _ = (init, sigma0, NUM_WEIGHTS);
    todo!("Agent G: implement initial_state")
}

/// Round and clamp a real-valued vector into integer weights.
pub fn to_weights(v: &[f64]) -> Weights {
    let _ = v;
    todo!("Agent G: implement to_weights")
}

/// CONTRACT: sample `population` candidates. Index 0 is always
/// `to_weights(&state.mean)` (so every generation measures the current
/// belief, which is the learning curve); the rest are mean + sigma·N(0,1),
/// using an RNG seeded from `seed`. Deterministic.
pub fn sample(state: &CemState, population: usize, seed: u64) -> Vec<Weights> {
    let _ = (state, population, seed);
    todo!("Agent G: implement sample")
}

/// CONTRACT: given every candidate's fitness (average penalty points per hand;
/// LOWER is better; `f64::INFINITY` for a failed evaluation), fit the new
/// belief to the `cfg.elites` best (mean and std per weight), blend with the
/// old belief by `cfg.smoothing`, floor sigmas at `cfg.min_sigma`, and return
/// it with `generation + 1`. Ties broken by candidate index. Deterministic.
pub fn update(state: &CemState, cfg: &CemConfig, scored: &[(Weights, f64)]) -> CemState {
    let _ = (state, cfg, scored);
    todo!("Agent G: implement update")
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
