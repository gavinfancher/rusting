//! The `train` and `features` subcommands: argument parsing and printing.
//! The algorithm itself lives in [`crate::train`].

use crate::train::{TrainConfig, train};
use crate::{CommonRules, StrategyList, pool};
use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use hearts_core::learn::{CemConfig, Init};
use hearts_core::types::{FEATURE_NAMES, StrategyKind, Weights};
use std::time::Instant;

/// Command-line spelling of [`Init`] (clap needs `ValueEnum`, which the core
/// crate deliberately doesn't depend on).
#[derive(Clone, Copy, ValueEnum)]
pub enum InitArg {
    Heuristic,
    Zero,
}

#[derive(Args)]
pub struct TrainArgs {
    /// Candidates per generation (candidate 0 is the current mean).
    #[arg(long, default_value_t = 24)]
    population: usize,
    /// Best candidates the next generation is fitted to.
    #[arg(long, default_value_t = 6)]
    elites: usize,
    #[arg(long, default_value_t = 30)]
    generations: u32,
    /// Hands each candidate plays per generation.
    #[arg(long, default_value_t = 40_000)]
    hands: u64,
    /// The three opponents every candidate plays against.
    #[arg(long, default_value = "heuristic,heuristic,heuristic")]
    opponents: StrategyList,
    /// Starting weights: the hand-set default, or all zeros.
    #[arg(long, value_enum, default_value = "heuristic")]
    init: InitArg,
    /// Initial standard deviation of every weight.
    #[arg(long, default_value_t = 20.0)]
    sigma0: f64,
    /// 0..1: how far to move toward the elites each generation.
    #[arg(long, default_value_t = 0.7)]
    smoothing: f64,
    /// Floor for the standard deviations.
    #[arg(long, default_value_t = 2.0)]
    min_sigma: f64,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Worker threads (0 = all cores). Never changes the result.
    #[arg(long, default_value_t = 0)]
    threads: usize,
    /// Hands for the final champion-vs-heuristic comparison (0 = skip).
    #[arg(long, default_value_t = 200_000)]
    validation_hands: u64,
    /// Print machine-readable JSON (progress goes to stderr).
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    rules: CommonRules,
}

pub fn train_cmd(a: TrainArgs) -> Result<()> {
    if a.population < 2 {
        bail!("--population must be at least 2");
    }
    if a.elites == 0 || a.elites > a.population {
        bail!("--elites must be in 1..=population");
    }
    if !(0.0..=1.0).contains(&a.smoothing) {
        bail!("--smoothing must be in 0..=1");
    }
    let n_opp = a.opponents.0.len();
    let opponents: [StrategyKind; 3] = a
        .opponents
        .0
        .try_into()
        .map_err(|_| anyhow::anyhow!("need exactly 3 opponents, got {n_opp}"))?;
    let init = match a.init {
        InitArg::Heuristic => Init::Heuristic,
        InitArg::Zero => Init::Zero,
    };
    let cfg = TrainConfig {
        cem: CemConfig {
            population: a.population,
            elites: a.elites,
            smoothing: a.smoothing,
            min_sigma: a.min_sigma,
        },
        generations: a.generations,
        hands: a.hands,
        opponents,
        init,
        sigma0: a.sigma0,
        seed: a.seed,
        validation_hands: a.validation_hands,
        rules: a.rules.rules(),
    };
    let pool = pool(a.threads)?;
    let json = a.json;
    // Progress is a human-facing table; in --json mode it moves to stderr so
    // stdout stays one parseable document.
    let say = move |line: String| {
        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    say(format!(
        "CEM: population {} elites {} generations {} hands/candidate {} init {} sigma0 {} seed {} threads {}",
        cfg.cem.population,
        cfg.cem.elites,
        cfg.generations,
        cfg.hands,
        if matches!(init, Init::Zero) {
            "zero"
        } else {
            "heuristic"
        },
        cfg.sigma0,
        cfg.seed,
        pool.current_num_threads()
    ));
    say(format!(
        "{:>4} {:>9} {:>9} {:>9} {:>9} {:>9} {:>8}",
        "gen", "mean", "best", "elite", "pop_mean", "sigma", "secs"
    ));
    let t0 = Instant::now();
    let result = pool.install(|| {
        train(&cfg, |g, _| {
            say(format!(
                "{:>4} {:>9.4} {:>9.4} {:>9.4} {:>9.4} {:>9.3} {:>8.1}",
                g.generation,
                g.mean_fitness,
                g.best_fitness,
                g.elite_mean_fitness,
                g.population_mean_fitness,
                g.sigma_mean,
                g.elapsed_ms as f64 / 1000.0
            ));
        })
    });
    let total = t0.elapsed().as_secs_f64();

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "\nchampion ({total:.1} s total):\n{}\n",
        result.champion_strategy
    );
    print_weights(&result.champion);
    if let Some(v) = &result.validation {
        println!(
            "\nvalidation on {} fresh hands vs {}:",
            v.hands,
            cfg.opponents.map(|o| o.to_string()).join(",")
        );
        println!("  champion  {:.4} avg points", v.champion_fitness);
        println!("  heuristic {:.4} avg points", v.baseline_fitness);
        println!(
            "  difference {:+.4} +- {:.4} (1 standard error; negative = champion better)",
            v.difference, v.difference_stderr
        );
    }
    Ok(())
}

fn print_weights(w: &Weights) {
    println!("{:<4} {:<30} {:>8}", "#", "feature", "weight");
    for (i, (name, v)) in FEATURE_NAMES.iter().zip(w.0).enumerate() {
        println!("{i:<4} {name:<30} {v:>8}");
    }
}

#[derive(Args)]
pub struct FeaturesArgs {
    /// `linear:w0,...,w23`, or `default` for the hand-set starting weights.
    #[arg(long, default_value = "default")]
    strategy: String,
}

pub fn features_cmd(a: FeaturesArgs) -> Result<()> {
    let w = if a.strategy.trim().eq_ignore_ascii_case("default") {
        hearts_core::linear::default_weights()
    } else {
        match a
            .strategy
            .parse::<StrategyKind>()
            .map_err(anyhow::Error::msg)?
        {
            StrategyKind::Linear(w) => w,
            other => bail!("--strategy must be linear:... or default, got {other}"),
        }
    };
    print_weights(&w);
    Ok(())
}
