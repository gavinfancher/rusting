//! Tournament runner: simulate a range of hands of a job.
//!
//! One hand = deal, pass, play 13 tricks. Everything random in hand `i` comes
//! from `seed::hand_seed(spec.seed, i)` (see `rng.rs` for the per-decision
//! derivation), so any hand can be replayed alone and any range can be split
//! across workers and merged.

use crate::card::{Card, CardSet};
use crate::game::{Game, TrickDone};
use crate::rng::Rng;
use crate::rules;
use crate::seed::{child_seed, hand_seed};
use crate::strategy::{Bot, Strategy};
use crate::types::{JobSpec, PassDirection, Rules, Seat, UnitStats, seating};
use rayon::prelude::*;
use std::ops::Range;

/// Result of one hand, indexed by SEAT (not strategy slot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandResult {
    /// Final scores after the moon adjustment.
    pub points: [u8; 4],
    pub moon: Option<Seat>,
    /// Seat that took Q♠.
    pub queen_taker: Seat,
}

/// Hook for watching a hand as it is played (used by `hearts-cli play`).
/// All methods default to no-ops; the `()` observer compiles away entirely.
pub trait Observer {
    fn dealt(&mut self, _hands: &[CardSet; 4]) {}
    fn passed(&mut self, _dir: PassDirection, _passes: &[CardSet; 4]) {}
    fn played(&mut self, _seat: Seat, _card: Card, _done: Option<TrickDone>) {}
}

impl Observer for () {}

/// Play one full hand. `bots[s]` plays seat `s`.
pub fn play_hand<O: Observer>(
    bots: &[Bot; 4],
    rules: Rules,
    hand_index: u64,
    hseed: u64,
    obs: &mut O,
) -> HandResult {
    let mut deal_rng = Rng::new(child_seed(hseed, 0));
    let hands = rules::deal(&mut deal_rng);
    let mut rngs: [Rng; 4] = std::array::from_fn(|s| Rng::new(child_seed(hseed, 1 + s as u64)));
    let mut game = Game::new(hands, rules);
    obs.dealt(&hands);

    let dir = PassDirection::for_hand(hand_index);
    if dir != PassDirection::Hold {
        let passes: [CardSet; 4] =
            std::array::from_fn(|s| bots[s].choose_pass(hands[s], dir, &mut rngs[s]));
        debug_assert!(
            (0..4).all(|s| passes[s].len() == 3 && (passes[s] - hands[s]).is_empty()),
            "a bot passed an invalid set"
        );
        game.apply_passes(dir, passes);
        obs.passed(dir, &passes);
    }

    while !game.is_over() {
        let seat = game.to_play();
        let view = game.view(seat);
        let card = bots[seat as usize].choose_play(&view, &mut rngs[seat as usize]);
        let done = game.play(card);
        obs.played(seat, card, done);
    }

    HandResult {
        points: game.final_points(),
        moon: rules::moon_shooter(game.points),
        queen_taker: game.queen_taker.unwrap_or(0),
    }
}

/// Add one hand's result to `acc`, translating seats to strategy slots.
#[allow(clippy::needless_range_loop)] // `seat` indexes three parallel arrays
fn accumulate(acc: &mut UnitStats, r: &HandResult, seat_to_slot: [u8; 4]) {
    acc.hands += 1;
    let best = *r.points.iter().min().unwrap_or(&0);
    for seat in 0..4 {
        let s = &mut acc.slots[seat_to_slot[seat] as usize];
        s.hands += 1;
        s.points += r.points[seat] as u64;
        s.moons += (r.moon == Some(seat as Seat)) as u64;
        s.queens += (r.queen_taker as usize == seat) as u64;
        s.wins += (r.points[seat] == best) as u64;
    }
}

/// Simulate hands `range` (hand indices, e.g. `0..2000`) of `spec` and
/// return per-slot totals.
///
/// CONTRACT (other crates depend on this):
/// - Hand `i` is dealt from `seed::hand_seed(spec.seed, i)`; all randomness
///   for that hand (including bot decisions) derives from that seed.
/// - Seating comes from `types::seating(i)` (seat -> slot) and passing from
///   `PassDirection::for_hand(i)`.
/// - Deterministic: the same `(spec, range)` always returns identical stats,
///   regardless of rayon thread count, and splitting a range in two and
///   merging the results gives exactly the same stats as the whole range.
/// - Runs in parallel on the caller's current rayon pool.
/// - Pure CPU; never blocks on I/O. May take seconds to minutes (PIMC).
///
/// Why this is deterministic under parallelism: each hand's result depends
/// only on `(spec, i)`, and `UnitStats::merge` is integer addition
/// (commutative + associative), so rayon's fold/reduce tree shape is
/// irrelevant.
pub fn run_hands(spec: &JobSpec, range: Range<u64>) -> UnitStats {
    let slot_bots: [Bot; 4] = std::array::from_fn(|i| Bot::new(spec.strategies[i]));
    let rules = spec.rules;
    let seed = spec.seed;
    range
        .into_par_iter()
        // `fold` gives each rayon task a private accumulator (no sharing, no
        // locks); `reduce` then merges the per-task accumulators.
        .fold(UnitStats::default, |mut acc, i| {
            let slots = seating(i);
            let bots: [Bot; 4] = std::array::from_fn(|seat| slot_bots[slots[seat] as usize]);
            let r = play_hand(&bots, rules, i, hand_seed(seed, i), &mut ());
            accumulate(&mut acc, &r, slots);
            acc
        })
        .reduce(UnitStats::default, |mut a, b| {
            a.merge(&b);
            a
        })
}
