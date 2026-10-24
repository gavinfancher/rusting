//! Engine benchmarks. Everything except `run_hands` is single-threaded so the
//! numbers are per-core and comparable between optimizations.
//!
//! Run: `cargo bench -p hearts-core` (add `-- --quick` for a fast pass).

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use hearts_core::advisor;
use hearts_core::card::CardSet;
use hearts_core::seed::hand_seed;
use hearts_core::strategy::Bot;
use hearts_core::tournament::{play_hand, run_hands};
use hearts_core::types::{JobSpec, PlayerView, Rules, StrategyKind, Trick};
use std::hint::black_box;

fn bench_hand(c: &mut Criterion) {
    let mut g = c.benchmark_group("hand");
    for (name, bot) in [
        ("random", Bot::Random),
        ("heuristic", Bot::Heuristic),
        (
            "linear",
            Bot::Linear(hearts_core::linear::default_weights()),
        ),
    ] {
        let bots = [bot; 4];
        let mut i = 0u64;
        g.throughput(Throughput::Elements(1));
        g.bench_function(name, |b| {
            b.iter(|| {
                i += 1;
                black_box(play_hand(
                    &bots,
                    Rules::default(),
                    i,
                    hand_seed(42, i),
                    &mut (),
                ))
            })
        });
    }
    g.finish();
}

fn bench_run_hands(c: &mut Criterion) {
    let spec = JobSpec {
        seed: 42,
        strategies: [StrategyKind::Heuristic; 4],
        rules: Rules::default(),
    };
    let mut g = c.benchmark_group("run_hands");
    g.throughput(Throughput::Elements(1000));
    // Single-threaded pool so this is a per-core figure.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    g.bench_function("heuristic_1000", |b| {
        b.iter(|| pool.install(|| black_box(run_hands(&spec, 0..1000))))
    });
    g.finish();
}

fn bench_advisor(c: &mut Criterion) {
    // A typical mid-hand decision: opening lead with a full 13-card hand is
    // forced (2C), so use trick 1 as the 3rd player instead, with real choices.
    let mut trick = Trick::new(0);
    trick.push("2C".parse().unwrap());
    trick.push("9C".parse().unwrap());
    let view = PlayerView {
        seat: 2,
        hand: CardSet::parse_list("5C 7C KC 9D QS KS 3H 8H AH 4D 6D JD TS").unwrap(),
        trick,
        played: CardSet::EMPTY,
        voids: [0; 4],
        hearts_broken: false,
        tricks_played: 0,
        points: [0; 4],
        passed: None,
        rules: Rules::default(),
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let mut g = c.benchmark_group("advisor");
    g.throughput(Throughput::Elements(1000));
    g.bench_function("simulate_1000_samples", |b| {
        b.iter(|| pool.install(|| black_box(advisor::simulate(&view, 1000, 7, None))))
    });
    // Just the dealing step, to see how much of a sample it is.
    g.bench_function("determinize_1000", |b| {
        let mut rng = hearts_core::rng::Rng::new(1);
        b.iter(|| {
            for _ in 0..1000 {
                black_box(advisor::determinize(&view, &mut rng));
            }
        })
    });
    g.finish();
}

criterion_group!(benches, bench_hand, bench_run_hands, bench_advisor);
criterion_main!(benches);
