#![allow(clippy::needless_range_loop)] // seat indices index several parallel arrays
//! Integration tests for the engine: invariants, determinism, advisor, and
//! bot strength sanity checks.

use hearts_core::advisor::{self, determinize};
use hearts_core::card::{Card, CardSet, Suit};
use hearts_core::game::Game;
use hearts_core::rng::Rng;
use hearts_core::rules::{self, legal_moves, validate_view};
use hearts_core::seed::child_seed;
use hearts_core::strategy::{Bot, Strategy};
use hearts_core::tournament::{play_hand, run_hands};
use hearts_core::types::{JobSpec, PassDirection, PlayerView, Rules, StrategyKind, UnitStats};
use proptest::prelude::*;
use std::time::Instant;

/// Deal, pass and play `plays` cards with the given bots, returning the game
/// mid-hand. Asserts legality of every move on the way.
fn game_after(bots: &[Bot; 4], seed: u64, hand_index: u64, plays: usize) -> Game {
    let mut rng = Rng::new(child_seed(seed, 0));
    let hands = rules::deal(&mut rng);
    let mut rngs: [Rng; 4] = std::array::from_fn(|s| Rng::new(child_seed(seed, 1 + s as u64)));
    let mut g = Game::new(hands, Rules::default());
    let dir = PassDirection::for_hand(hand_index);
    if dir != PassDirection::Hold {
        let passes = std::array::from_fn(|s| bots[s].choose_pass(hands[s], dir, &mut rngs[s]));
        g.apply_passes(dir, passes);
    }
    for _ in 0..plays {
        if g.is_over() {
            break;
        }
        let seat = g.to_play();
        let v = g.view(seat);
        let c = bots[seat as usize].choose_play(&v, &mut rngs[seat as usize]);
        assert!(
            legal_moves(&v).contains(c),
            "illegal move {c} for seat {seat}"
        );
        g.play(c);
    }
    g
}

proptest! {
    #[test]
    fn full_hand_plays_every_card_once_and_scores_26_or_78(seed in any::<u64>(), idx in 0u64..96) {
        let bots = [Bot::Random; 4];
        let mut g = game_after(&bots, seed, idx, 0);
        let mut seen = CardSet::EMPTY;
        let mut plays = 0;
        let mut rng = Rng::new(seed ^ 1);
        while !g.is_over() {
            let seat = g.to_play();
            let v = g.view(seat);
            prop_assert_eq!(validate_view(&v), Ok(()));
            let legal = legal_moves(&v);
            prop_assert!(!legal.is_empty());
            let c = Bot::Random.choose_play(&v, &mut rng);
            prop_assert!(legal.contains(c));
            prop_assert!(!seen.contains(c), "card played twice");
            seen.insert(c);
            plays += 1;
            g.play(c);
        }
        prop_assert_eq!(plays, 52);
        prop_assert_eq!(seen, CardSet::FULL_DECK);
        let total: u32 = g.final_points().iter().map(|&p| p as u32).sum();
        prop_assert!(total == 26 || total == 78, "total {}", total);
        prop_assert!(g.hands.iter().all(|h| h.is_empty()));
    }

    #[test]
    fn heuristic_hands_are_legal_and_conserve_points(seed in any::<u64>(), idx in 0u64..96) {
        let r = play_hand(&[Bot::Heuristic; 4], Rules::default(), idx, seed, &mut ());
        let total: u32 = r.points.iter().map(|&p| p as u32).sum();
        prop_assert!(total == 26 || total == 78);
        prop_assert_eq!(r.moon.is_some(), total == 78);
    }

    /// Sample deals must always honour card counts, voids, passed cards, and
    /// partition exactly the unseen cards.
    #[test]
    fn determinize_respects_constraints(seed in any::<u64>(), idx in 0u64..96, plays in 0usize..50) {
        let bots = [Bot::Heuristic, Bot::Random, Bot::Heuristic, Bot::Random];
        let g = game_after(&bots, seed, idx, plays);
        if g.is_over() { return Ok(()); }
        let seat = g.to_play();
        let v = g.view(seat);
        prop_assert_eq!(validate_view(&v), Ok(()));
        let mut rng = Rng::new(seed ^ 0xABCD);
        for _ in 0..20 {
            let hands = determinize(&v, &mut rng);
            prop_assert_eq!(hands[seat as usize], v.hand);
            let mut union = CardSet::EMPTY;
            for s in 0..4usize {
                prop_assert!((union & hands[s]).is_empty(), "card dealt twice");
                union = union | hands[s];
                if s != seat as usize {
                    prop_assert_eq!(hands[s].len(), g.hands[s].len(), "seat {} count", s);
                    for suit in Suit::ALL {
                        if v.voids[s] & suit.bit() != 0 {
                            prop_assert!(!hands[s].has_suit(suit), "seat {} void {:?} violated", s, suit);
                        }
                    }
                }
            }
            prop_assert_eq!(union - v.hand, v.unseen());
            if let Some((to, cards)) = v.passed {
                let still = cards & v.unseen();
                prop_assert_eq!(hands[to as usize] & still, still, "passed cards must be with receiver");
            }
        }
    }
}

fn spec(strategies: [&str; 4], seed: u64) -> JobSpec {
    JobSpec {
        seed,
        strategies: strategies.map(|s| s.parse::<StrategyKind>().unwrap()),
        rules: Rules::default(),
    }
}

fn pool(n: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .build()
        .unwrap()
}

#[test]
fn run_hands_same_for_1_and_8_threads() {
    let s = spec(["heuristic", "random", "heuristic", "random"], 42);
    let a = pool(1).install(|| run_hands(&s, 0..1000));
    let b = pool(8).install(|| run_hands(&s, 0..1000));
    assert_eq!(a, b);
    assert_eq!(a.hands, 1000);
}

#[test]
fn run_hands_is_range_splittable() {
    let s = spec(["heuristic", "heuristic", "random", "random"], 7);
    let whole = run_hands(&s, 0..1000);
    let mut merged = run_hands(&s, 0..500);
    merged.merge(&run_hands(&s, 500..1000));
    assert_eq!(whole, merged);
    // Uneven split that is not a multiple of the seating period.
    let mut m2 = run_hands(&s, 0..333);
    m2.merge(&run_hands(&s, 333..1000));
    assert_eq!(whole, m2);
    assert_eq!(run_hands(&s, 5..5), UnitStats::default());
}

#[test]
fn pimc_run_hands_deterministic_across_threads() {
    let s = spec(["pimc:20", "heuristic", "random", "heuristic"], 3);
    let a = pool(1).install(|| run_hands(&s, 0..16));
    let b = pool(4).install(|| run_hands(&s, 0..16));
    assert_eq!(a, b);
}

#[test]
fn slot_points_balance() {
    // Every hand distributes 26 (or 78) points over the slots.
    let s = spec(["random", "random", "random", "random"], 1);
    let st = run_hands(&s, 0..400);
    let total: u64 = st.slots.iter().map(|x| x.points).sum();
    let moons: u64 = st.slots.iter().map(|x| x.moons).sum();
    assert_eq!(total, 26 * (400 - moons) + 78 * moons);
    assert!(st.slots.iter().all(|x| x.hands == 400));
    let queens: u64 = st.slots.iter().map(|x| x.queens).sum();
    assert_eq!(queens, 400, "exactly one seat takes Q♠ per hand");
}

fn midgame_view(seed: u64, plays: usize) -> PlayerView {
    let g = game_after(&[Bot::Heuristic; 4], seed, 0, plays);
    g.view(g.to_play())
}

#[test]
fn advisor_is_deterministic_for_a_seed() {
    let v = midgame_view(11, 10);
    let a = advisor::simulate(&v, 300, 99, None);
    let b = pool(1).install(|| advisor::simulate(&v, 300, 99, None));
    let c = pool(6).install(|| advisor::simulate(&v, 300, 99, None));
    assert_eq!(a, b);
    assert_eq!(a, c);
    assert_eq!(a, {
        let mut t = advisor::simulate_serial(&v, 300, 99);
        t.moves.sort_by_key(|m| m.card);
        t
    });
    let d = advisor::simulate(&v, 300, 100, None);
    assert_ne!(a, d, "different seeds should differ");
    assert_eq!(a.samples_done, 300);
    assert_eq!(a.moves.len(), legal_moves(&v).len() as usize);
    assert!(a.moves.iter().all(|m| m.samples == 300));
}

#[test]
fn advisor_deadline_in_the_past_returns_partial() {
    let v = midgame_view(2, 8);
    let t = advisor::simulate(&v, 100_000, 1, Some(Instant::now()));
    assert!(t.samples_done < 100_000);
    assert!(t.moves.iter().all(|m| m.samples == t.samples_done));
}

#[test]
fn advisor_single_legal_move() {
    let hand = CardSet::parse_list("2C 7C 9D QS KS 3H 8H AH 4D 6D JC TS 5S").unwrap();
    let mut v = PlayerView {
        seat: 0,
        hand,
        trick: hearts_core::types::Trick::new(0),
        played: CardSet::EMPTY,
        voids: [0; 4],
        hearts_broken: false,
        tricks_played: 0,
        points: [0; 4],
        passed: None,
        rules: Rules::default(),
    };
    let t = advisor::simulate(&v, 50, 3, None);
    assert_eq!(t.best(), Some(Card::TWO_OF_CLUBS));
    assert_eq!(t.moves.len(), 1);
    v.trick = hearts_core::types::Trick::new(3);
    assert!(validate_view(&v).is_err());
}

#[test]
fn advisor_prefers_ducking_the_queen() {
    // Trick 2, we must follow spades with QS or 2S while 9S is on the table:
    // 2S ducks, QS would be dumped on whoever wins. 2S must come out ahead.
    let mut trick = hearts_core::types::Trick::new(0);
    trick.push("9S".parse().unwrap());
    let v = PlayerView {
        seat: 1,
        hand: CardSet::parse_list("QS 2S 3C 4C 5C 6C 7C 8C 9C TC JC QC").unwrap(),
        trick,
        played: CardSet::parse_list("2D 3D 4D 5D").unwrap(),
        voids: [0; 4],
        hearts_broken: false,
        tricks_played: 1,
        points: [0; 4],
        passed: None,
        rules: Rules::default(),
    };
    assert_eq!(validate_view(&v), Ok(()));
    let t = advisor::simulate(&v, 400, 5, None);
    assert_eq!(t.best(), Some("2S".parse().unwrap()));
}

fn avg_points(s: &JobSpec, hands: u64) -> [f64; 4] {
    let st = run_hands(s, 0..hands);
    std::array::from_fn(|i| st.slots[i].points as f64 / st.slots[i].hands as f64)
}

#[test]
fn heuristic_beats_random() {
    let s = spec(["heuristic", "heuristic", "random", "random"], 2024);
    let a = avg_points(&s, 20_000);
    println!("heuristic vs random avg points: {a:?}");
    assert!(a[0] < a[2] - 2.0 && a[1] < a[3] - 2.0, "{a:?}");
    // Random players average ~6.5 each; heuristics clearly less.
    assert!(a[0] < 5.5, "{a:?}");
}

#[test]
fn pimc_beats_heuristic() {
    // pimc:100 is slow-ish; 400 hands is enough to see a clear gap.
    let s = spec(["pimc:100", "heuristic", "pimc:100", "heuristic"], 77);
    let a = avg_points(&s, 400);
    println!("pimc:100 vs heuristic avg points: {a:?}");
    assert!((a[0] + a[2]) / 2.0 < (a[1] + a[3]) / 2.0, "{a:?}");
}
