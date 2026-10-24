//! Integration tests for the linear bot: it must only ever play legal cards
//! (whatever the weights), and its tournaments must be reproducible.

use hearts_core::card::CardSet;
use hearts_core::game::Game;
use hearts_core::linear::{default_weights, linear_pass, linear_play};
use hearts_core::rng::Rng;
use hearts_core::rules::{self, legal_moves};
use hearts_core::seed::child_seed;
use hearts_core::strategy::{Bot, Strategy};
use hearts_core::tournament::run_hands;
use hearts_core::types::{JobSpec, NUM_WEIGHTS, PassDirection, Rules, StrategyKind, Weights};
use proptest::prelude::*;

fn weights_strategy() -> impl Strategy_<Value = Weights> {
    prop::collection::vec(-300i32..300, NUM_WEIGHTS).prop_map(|v| {
        let mut a = [0i32; NUM_WEIGHTS];
        a.copy_from_slice(&v);
        Weights(a)
    })
}

// `Strategy` is also the name of hearts-core's bot trait, so alias proptest's.
use proptest::strategy::Strategy as Strategy_;

proptest! {
    /// Linear (random weights) vs random bots: every pass and every play is legal.
    #[test]
    fn linear_plays_only_legal_cards(
        w in weights_strategy(),
        seed in any::<u64>(),
        idx in 0u64..96,
    ) {
        let bots = [Bot::Linear(w), Bot::Random, Bot::Linear(w), Bot::Random];
        let mut deal_rng = Rng::new(child_seed(seed, 0));
        let hands = rules::deal(&mut deal_rng);
        let mut rngs: [Rng; 4] =
            std::array::from_fn(|s| Rng::new(child_seed(seed, 1 + s as u64)));
        let mut g = Game::new(hands, Rules::default());
        let dir = PassDirection::for_hand(idx);
        if dir != PassDirection::Hold {
            let passes: [CardSet; 4] =
                std::array::from_fn(|s| bots[s].choose_pass(hands[s], dir, &mut rngs[s]));
            for s in 0..4 {
                prop_assert_eq!(passes[s].len(), 3);
                prop_assert!((passes[s] - hands[s]).is_empty());
            }
            g.apply_passes(dir, passes);
        }
        while !g.is_over() {
            let seat = g.to_play();
            let v = g.view(seat);
            let c = bots[seat as usize].choose_play(&v, &mut rngs[seat as usize]);
            prop_assert!(legal_moves(&v).contains(c), "illegal {c}");
            g.play(c);
        }
    }

    /// Direct calls: the picked card is always one of the legal moves.
    #[test]
    fn linear_play_and_pass_are_in_range(w in weights_strategy(), seed in any::<u64>()) {
        let mut rng = Rng::new(seed);
        let hands = rules::deal(&mut rng);
        let p = linear_pass(&w, hands[0], PassDirection::Left);
        prop_assert_eq!(p.len(), 3);
        prop_assert!((p - hands[0]).is_empty());
        let g = Game::new(hands, Rules::default());
        let v = g.view(g.to_play());
        prop_assert!(legal_moves(&v).contains(linear_play(&w, &v)));
    }
}

#[test]
fn linear_tournament_is_reproducible_and_splittable() {
    let spec = JobSpec {
        seed: 11,
        strategies: [
            StrategyKind::Linear(default_weights()),
            StrategyKind::Heuristic,
            StrategyKind::Heuristic,
            StrategyKind::Random,
        ],
        rules: Rules::default(),
    };
    let whole = run_hands(&spec, 0..3000);
    assert_eq!(whole, run_hands(&spec, 0..3000));
    let mut parts = run_hands(&spec, 0..1234);
    parts.merge(&run_hands(&spec, 1234..3000));
    assert_eq!(whole, parts);
}

/// The default weights should be roughly as good as the heuristic: well
/// inside 0.5 points of the heuristic-vs-heuristic baseline.
#[test]
fn default_weights_play_like_the_heuristic() {
    let avg = |s0: StrategyKind| {
        let spec = JobSpec {
            seed: 3,
            strategies: [
                s0,
                StrategyKind::Heuristic,
                StrategyKind::Heuristic,
                StrategyKind::Heuristic,
            ],
            rules: Rules::default(),
        };
        let st = run_hands(&spec, 0..20_000);
        st.slots[0].points as f64 / st.slots[0].hands as f64
    };
    let base = avg(StrategyKind::Heuristic);
    let lin = avg(StrategyKind::Linear(default_weights()));
    assert!((lin - base).abs() < 0.5, "linear {lin} vs heuristic {base}");
}
