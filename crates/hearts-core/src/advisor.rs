//! PIMC advisor: estimate expected points for each legal card.
//!
//! Perfect-information Monte Carlo: guess ("determinize") where the unseen
//! cards are, then for every legal move play it and finish the hand with the
//! heuristic bot, and average the points we end up with. Using the same guess
//! for every move cancels most of the noise when comparing moves.

use crate::card::{Card, CardSet, Suit};
use crate::game::Game;
use crate::rng::Rng;
use crate::rules::{final_points, legal_moves};
use crate::seed::child_seed;
use crate::strategy::heuristic_play;
use crate::types::{MoveStat, MoveTotals, PlayerView};
use rayon::prelude::*;
use std::time::Instant;

/// Samples are processed in fixed-size chunks; chunk `k` uses
/// `seed::child_seed(seed, k)`. Chunks, not threads, own the randomness, so
/// results don't depend on the thread count.
pub const SAMPLE_CHUNK: u32 = 64;

/// Run `samples` determinizations for the player in `view`.
///
/// CONTRACT:
/// - One sample = deal the unseen cards randomly to the other seats,
///   respecting `view.voids`, the known passed cards, and each seat's card
///   count; then, for EVERY legal move, play that move and finish the hand
///   with heuristic bots, recording this player's final hand points
///   (moon-shot adjusted). Using the same deal for every move is
///   deliberate: it reduces noise between moves.
/// - Every returned `MoveStat` has `samples == samples_done`.
/// - `deadline`: checked before each chunk starts; when passed, stop and
///   return what's done (`samples_done < samples`). Never panics on a
///   deadline.
/// - Deterministic for a given `(view, samples, seed)` when no deadline is hit.
/// - Runs on the caller's current rayon pool.
///
/// The view should pass `rules::validate_view`; on an inconsistent view the
/// result is meaningless but this function still does not panic.
pub fn simulate(
    view: &PlayerView,
    samples: u32,
    seed: u64,
    deadline: Option<Instant>,
) -> MoveTotals {
    simulate_impl(view, samples, seed, deadline, true)
}

/// Same result as [`simulate`] (no deadline) but on the calling thread only.
/// Used by the PIMC bot inside tournaments, which already run one hand per
/// rayon task, so nesting parallelism would only add overhead.
pub fn simulate_serial(view: &PlayerView, samples: u32, seed: u64) -> MoveTotals {
    simulate_impl(view, samples, seed, None, false)
}

/// Totals for one chunk: sum of final points per legal move (indexed in
/// ascending card order) and how many samples went into it.
struct Chunk {
    totals: [u64; 13],
    n: u32,
}

fn simulate_impl(
    view: &PlayerView,
    samples: u32,
    seed: u64,
    deadline: Option<Instant>,
    parallel: bool,
) -> MoveTotals {
    let legal = legal_moves(view);
    let mut cards = [Card::TWO_OF_CLUBS; 13];
    let mut n_cards = 0;
    for c in legal {
        cards[n_cards] = c;
        n_cards += 1;
    }
    let cards = &cards[..n_cards];

    let nchunks = samples.div_ceil(SAMPLE_CHUNK);
    let run_chunk = |k: u32| -> Option<Chunk> {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return None;
        }
        let n = SAMPLE_CHUNK.min(samples - k * SAMPLE_CHUNK);
        let mut rng = Rng::new(child_seed(seed, k as u64));
        let mut totals = [0u64; 13];
        for _ in 0..n {
            let hands = determinize(view, &mut rng);
            let base = Game::from_view(view, hands);
            for (j, &card) in cards.iter().enumerate() {
                let mut g = base; // `Game` is Copy: this is a memcpy
                g.play(card);
                playout(&mut g);
                totals[j] += final_points(g.points)[view.seat as usize] as u64;
            }
        }
        Some(Chunk { totals, n })
    };

    // `collect` on an indexed parallel iterator keeps chunk order, so the
    // reduction below is in chunk order regardless of scheduling.
    let chunks: Vec<Option<Chunk>> = if parallel {
        (0..nchunks).into_par_iter().map(run_chunk).collect()
    } else {
        (0..nchunks).map(run_chunk).collect()
    };

    let mut totals = [0u64; 13];
    let mut done = 0u32;
    for ch in chunks.into_iter().flatten() {
        done += ch.n;
        for (t, x) in totals.iter_mut().zip(ch.totals) {
            *t += x;
        }
    }
    MoveTotals {
        moves: cards
            .iter()
            .enumerate()
            .map(|(j, &card)| MoveStat {
                card,
                total_points: totals[j],
                samples: done,
            })
            .collect(),
        samples_done: done,
    }
}

/// Finish the hand with the heuristic policy for every seat.
#[inline]
fn playout(g: &mut Game) {
    while !g.is_over() {
        let seat = g.to_play();
        let view = g.view(seat);
        if view.hand.is_empty() {
            break; // only reachable from an inconsistent view
        }
        g.play(heuristic_play(&view));
    }
}

/// Randomly deal the cards `view` cannot see to the other three seats.
///
/// Guarantees (for a valid view that has a feasible deal): seat `s` gets
/// exactly the number of cards it still holds, none in a suit it is void in,
/// and the cards the player passed (if still unplayed) go to their receiver.
/// `hands[view.seat]` is the player's real hand.
///
/// Method ("rejection-free"): place the unseen cards most-constrained first
/// (cards that only 1 seat may hold, then 2, then 3),
/// each into a random eligible seat weighted by how many cards that seat
/// still needs. With no voids this is exactly a uniform deal. If a placement
/// dead-ends (every eligible seat is full) one swap repairs it; if even that
/// fails, voids are ignored for that card rather than looping or panicking.
pub fn determinize(view: &PlayerView, rng: &mut Rng) -> [CardSet; 4] {
    let me = view.seat as usize;
    let unseen = view.unseen();
    let mut hands = [CardSet::EMPTY; 4];
    hands[me] = view.hand;

    // Cards each other seat still holds.
    let mut need = [0usize; 4];
    for s in 0..4u8 {
        if s as usize != me {
            let played_in_trick = view.trick.iter().any(|(seat, _)| seat == s) as usize;
            need[s as usize] = (13usize)
                .saturating_sub(view.tricks_played as usize)
                .saturating_sub(played_in_trick);
        }
    }

    // Passed cards that are still hidden are known to be with the receiver.
    let mut fixed = CardSet::EMPTY;
    if let Some((to, cards)) = view.passed
        && (to as usize) != me
    {
        fixed = cards & unseen;
        hands[to as usize] = hands[to as usize] | fixed;
        need[to as usize] = need[to as usize].saturating_sub(fixed.len() as usize);
    }
    let pool = unseen - fixed;

    // Which other seats may hold each suit.
    let mut suit_elig = [0u8; 4];
    for suit in Suit::ALL {
        let mut mask = 0u8;
        for s in 0..4u8 {
            if s as usize != me && view.voids[s as usize] & suit.bit() == 0 {
                mask |= 1 << s;
            }
        }
        if mask == 0 {
            // Contradictory voids: fall back to "anyone" instead of failing.
            mask = !(1u8 << me) & 0b1111;
        }
        suit_elig[suit.index()] = mask;
    }

    let mut order = [0u8; 52];
    let mut n = 0;
    for c in pool {
        order[n] = c.index();
        n += 1;
    }

    for want in 1..=3u32 {
        for &idx in &order[..n] {
            let card = Card::from_index(idx);
            let elig = suit_elig[card.suit().index()];
            if elig.count_ones() != want {
                continue;
            }
            let seat = pick_seat(elig, &need, rng)
                .or_else(|| repair(elig, &mut hands, &mut need, &suit_elig, fixed))
                .or_else(|| pick_seat(!(1u8 << me) & 0b1111, &need, rng));
            if let Some(s) = seat {
                hands[s] = hands[s] | card.bit();
                need[s] -= 1;
            }
            // else: more unseen cards than capacity (invalid view); drop it.
        }
    }
    hands
}

/// Pick a seat from `elig` with probability proportional to its remaining need.
// Seats double as bit positions in `elig`, so index loops read best here.
#[allow(clippy::needless_range_loop)]
#[inline]
fn pick_seat(elig: u8, need: &[usize; 4], rng: &mut Rng) -> Option<usize> {
    let mut total = 0;
    for s in 0..4 {
        if elig & (1 << s) != 0 {
            total += need[s];
        }
    }
    if total == 0 {
        return None;
    }
    let mut r = rng.below(total);
    for s in 0..4 {
        if elig & (1 << s) != 0 {
            if r < need[s] {
                return Some(s);
            }
            r -= need[s];
        }
    }
    None
}

/// All seats eligible for the card being placed are full. Find a seat `e` that is eligible
/// for it and holds a movable card `d` that a seat `t` with free capacity may
/// hold; move `d` to `t` and let the caller use `e`'s freed slot. Returns `e`
/// (the caller then inserts `card` and decrements `need[e]`, so we pre-restore
/// it here).
fn repair(
    elig: u8,
    hands: &mut [CardSet; 4],
    need: &mut [usize; 4],
    suit_elig: &[u8; 4],
    fixed: CardSet,
) -> Option<usize> {
    for e in 0..4 {
        if elig & (1 << e) == 0 {
            continue;
        }
        for d in hands[e] - fixed {
            let d_elig = suit_elig[d.suit().index()];
            for t in 0..4 {
                if t != e && d_elig & (1 << t) != 0 && need[t] > 0 {
                    hands[e].remove(d);
                    hands[t] = hands[t] | d.bit();
                    need[t] -= 1;
                    need[e] += 1; // a slot opened in `e`
                    return Some(e);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::validate_view;
    use crate::types::Trick;

    fn c(s: &str) -> Card {
        s.parse().unwrap()
    }

    fn opening_view() -> PlayerView {
        PlayerView {
            seat: 0,
            hand: CardSet::parse_list("2C 7C 9D QS KS 3H 8H AH 4D 6D JC TS 5S").unwrap(),
            trick: Trick::new(0),
            played: CardSet::EMPTY,
            voids: [0; 4],
            hearts_broken: false,
            tricks_played: 0,
            points: [0; 4],
            passed: None,
            rules: Default::default(),
        }
    }

    #[test]
    fn single_legal_move_is_returned() {
        let v = opening_view();
        assert_eq!(validate_view(&v), Ok(()));
        let t = simulate(&v, 10, 1, None);
        assert_eq!(t.moves.len(), 1);
        assert_eq!(t.best(), Some(c("2C")));
        assert_eq!(t.samples_done, 10);
    }

    #[test]
    fn expired_deadline_does_no_work() {
        let v = opening_view();
        let t = simulate(&v, 1000, 1, Some(Instant::now()));
        assert!(t.samples_done < 1000);
        assert!(t.moves.iter().all(|m| m.samples == t.samples_done));
    }
}
