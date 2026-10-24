//! Bots: Random, Heuristic, PIMC.
//!
//! A [`Strategy`] makes two kinds of decisions, and only ever sees what a real
//! player sees:
//! - `choose_pass`: which 3 cards to give away (the hand and the direction),
//! - `choose_play`: which legal card to play (a [`PlayerView`]).
//!
//! Dispatch: strategies are tiny `Copy` values, so the hot path uses the
//! [`Bot`] enum (a `match`, no vtable, no heap) while [`make_strategy`] boxes
//! it for callers that want a `dyn Strategy`.

use crate::advisor;
use crate::card::{Card, CardSet, Suit};
use crate::linear;
use crate::rng::Rng;
use crate::rules::legal_moves;
use crate::types::{PassDirection, PlayerView, StrategyKind, Weights};

/// A player. `&self` (not `&mut`) because bots are stateless: everything they
/// remember is in the view, and all randomness comes from the caller's `Rng`.
pub trait Strategy: Send + Sync {
    /// Choose 3 cards from `hand` to pass in direction `dir` (never `Hold`).
    fn choose_pass(&self, hand: CardSet, dir: PassDirection, rng: &mut Rng) -> CardSet;
    /// Choose a card from `legal_moves(view)`.
    fn choose_play(&self, view: &PlayerView, rng: &mut Rng) -> Card;
}

/// Enum dispatch over the built-in bots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bot {
    Random,
    Heuristic,
    Pimc { samples: u32 },
    Linear(Weights),
}

impl Bot {
    pub fn new(kind: StrategyKind) -> Bot {
        match kind {
            StrategyKind::Random => Bot::Random,
            StrategyKind::Heuristic => Bot::Heuristic,
            StrategyKind::Pimc { samples } => Bot::Pimc { samples },
            StrategyKind::Linear(w) => Bot::Linear(w),
        }
    }
}

impl Strategy for Bot {
    #[inline]
    fn choose_pass(&self, hand: CardSet, dir: PassDirection, rng: &mut Rng) -> CardSet {
        match self {
            Bot::Random => random_pass(hand, rng),
            // PIMC passes like the heuristic (searching a pass is out of scope).
            Bot::Heuristic | Bot::Pimc { .. } => heuristic_pass(hand),
            Bot::Linear(w) => linear::linear_pass(w, hand, dir),
        }
    }

    #[inline]
    fn choose_play(&self, view: &PlayerView, rng: &mut Rng) -> Card {
        match self {
            Bot::Random => random_play(view, rng),
            Bot::Heuristic => heuristic_play(view),
            Bot::Pimc { samples } => pimc_play(view, *samples, rng),
            Bot::Linear(w) => linear::linear_play(w, view),
        }
    }
}

pub fn make_strategy(kind: StrategyKind) -> Box<dyn Strategy> {
    Box::new(Bot::new(kind))
}

/// The `n`-th (0-based, ascending index) card of a non-empty set.
#[inline]
pub fn nth_card(set: CardSet, n: usize) -> Card {
    let mut bits = set.0;
    for _ in 0..n {
        bits &= bits - 1; // clear the lowest set bit
    }
    Card::from_index(bits.trailing_zeros() as u8)
}

// ---------------------------------------------------------------- Random

pub fn random_pass(hand: CardSet, rng: &mut Rng) -> CardSet {
    let mut rest = hand;
    let mut out = CardSet::EMPTY;
    for _ in 0..3 {
        let c = nth_card(rest, rng.below(rest.len() as usize));
        rest.remove(c);
        out.insert(c);
    }
    out
}

pub fn random_play(view: &PlayerView, rng: &mut Rng) -> Card {
    let legal = legal_moves(view);
    nth_card(legal, rng.below(legal.len() as usize))
}

// ------------------------------------------------------------- Heuristic

/// Score each card by how much we want to get rid of it and pass the top 3.
/// Everything is integer arithmetic on a stack array: no allocation.
pub fn heuristic_pass(hand: CardSet) -> CardSet {
    let spades = hand.of_suit(Suit::Spades);
    // Spades strictly below the queen. With 3+ of them the Q♠ is "guarded":
    // we can duck spade leads, so we keep the high spades.
    let below_queen = CardSet(spades.0 & (Card::QUEEN_OF_SPADES.bit().0 - 1));
    let guarded = below_queen.len() >= 3;
    let suit_len = |s: Suit| hand.of_suit(s).len() as i32;

    let mut scored = [(0i32, 0u8); 13];
    let mut n = 0;
    for c in hand {
        let rank = c.rank() as i32;
        let mut score = rank;
        match c.suit() {
            Suit::Spades => {
                if c == Card::QUEEN_OF_SPADES {
                    score = if guarded { 10 } else { 100 };
                } else if rank > 10 {
                    score = if guarded { 8 } else { 90 + rank };
                }
            }
            Suit::Hearts => {
                score += if rank >= 9 {
                    60
                } else if rank >= 6 {
                    30
                } else {
                    0
                };
            }
            suit => {
                if rank >= 10 {
                    score += 40;
                }
                // Short minor suits are worth voiding.
                let len = suit_len(suit);
                if len <= 3 {
                    score += (4 - len) * 6;
                }
            }
        }
        scored[n] = (score, c.index());
        n += 1;
    }
    // Highest score first; ties by card index so the result is deterministic.
    scored[..n].sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored[..n]
        .iter()
        .take(3)
        .map(|&(_, i)| Card::from_index(i))
        .collect()
}

/// The rollout policy (also a standalone bot). Deterministic, no RNG.
///
/// Following suit: play the highest card that still loses the trick; if every
/// card wins, win with the highest (high cards are liabilities, so shed them),
/// but avoid Q♠ unless forced. Void: dump Q♠, else the highest heart, else the
/// highest card. Leading: the lowest card, preferring non-hearts, never leading
/// A♠/K♠ while Q♠ is unseen, and Q♠ only if nothing else is legal.
pub fn heuristic_play(view: &PlayerView) -> Card {
    let legal = legal_moves(view);
    if legal.len() == 1 {
        return first_card(legal);
    }
    match view.trick.led_suit() {
        Some(led) => {
            let in_suit = legal.of_suit(led);
            if in_suit.is_empty() {
                return dump(legal);
            }
            // Highest rank of the led suit currently on the table.
            let mut top = 0u8;
            for (_, c) in view.trick.iter() {
                if c.suit() == led && c.rank() > top {
                    top = c.rank();
                }
            }
            // Cards of the led suit below `top` lose the trick. Because
            // `in_suit` only has led-suit cards, masking by the bit index of
            // (led, top) keeps exactly the lower ranks.
            let below = CardSet((1u64 << (led.index() as u32 * 13 + top as u32)) - 1);
            let losers = in_suit & below;
            if let Some(c) = losers.highest() {
                return c;
            }
            // Forced to win. Keep Q♠ if we have any alternative.
            let safe = in_suit - Card::QUEEN_OF_SPADES.bit();
            let pool = if safe.is_empty() { in_suit } else { safe };
            pool.highest().unwrap_or(Card::TWO_OF_CLUBS)
        }
        None => lead(view, legal),
    }
}

/// Lowest card of a non-empty set (used for the forced single-move case).
fn first_card(set: CardSet) -> Card {
    set.lowest().unwrap_or(Card::TWO_OF_CLUBS)
}

/// Void in the led suit: get rid of the most dangerous card.
fn dump(legal: CardSet) -> Card {
    if legal.contains(Card::QUEEN_OF_SPADES) {
        return Card::QUEEN_OF_SPADES;
    }
    let hearts = legal.of_suit(Suit::Hearts);
    if let Some(h) = hearts.highest() {
        return h;
    }
    // Only the top card of each suit can be the overall highest by
    // (rank, index), so look at 4 candidates instead of up to 13.
    Suit::ALL
        .iter()
        .filter_map(|&s| legal.of_suit(s).highest())
        .max_by_key(|c| (c.rank(), c.index()))
        .unwrap_or(Card::TWO_OF_CLUBS)
}

fn lead(view: &PlayerView, legal: CardSet) -> Card {
    let mut cand = legal - Card::QUEEN_OF_SPADES.bit();
    let queen_unseen =
        !view.played.contains(Card::QUEEN_OF_SPADES) && !view.hand.contains(Card::QUEEN_OF_SPADES);
    if queen_unseen {
        // Leading A♠/K♠ invites the Q♠ onto us.
        let high_spades = Card::new(Suit::Spades, 11).bit() | Card::new(Suit::Spades, 12).bit();
        cand = cand - high_spades;
    }
    if cand.is_empty() {
        cand = legal;
    }
    // Likewise only the lowest card of each suit can win (same heart
    // penalty within a suit, and lower rank means lower score).
    Suit::ALL
        .iter()
        .filter_map(|&s| cand.of_suit(s).lowest())
        .min_by_key(|c| {
            let heart_penalty = if c.suit() == Suit::Hearts { 4 } else { 0 };
            (c.rank() + heart_penalty, c.index())
        })
        .unwrap_or(Card::TWO_OF_CLUBS)
}

// ------------------------------------------------------------------ PIMC

/// Ask the advisor (serially: tournaments already parallelise across hands)
/// and play its best card. The advisor seed is the next value from this seat's
/// RNG stream, so it depends on the hand seed and the decision's position.
pub fn pimc_play(view: &PlayerView, samples: u32, rng: &mut Rng) -> Card {
    let legal = legal_moves(view);
    if legal.len() == 1 {
        return first_card(legal);
    }
    let seed = rng.next_u64();
    advisor::simulate_serial(view, samples, seed)
        .best()
        .unwrap_or_else(|| first_card(legal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Trick;

    fn c(s: &str) -> Card {
        s.parse().unwrap()
    }

    fn view(hand: &str, trick: &[&str], leader: u8, tp: u8) -> PlayerView {
        let mut t = Trick::new(leader);
        for s in trick {
            t.push(c(s));
        }
        PlayerView {
            seat: t.to_play(),
            hand: CardSet::parse_list(hand).unwrap(),
            trick: t,
            played: CardSet::EMPTY,
            voids: [0; 4],
            hearts_broken: true,
            tricks_played: tp,
            points: [0; 4],
            passed: None,
            rules: Default::default(),
        }
    }

    #[test]
    fn nth_card_walks_the_set() {
        let s = CardSet::parse_list("2C 9D QS").unwrap();
        assert_eq!(nth_card(s, 0), c("2C"));
        assert_eq!(nth_card(s, 1), c("9D"));
        assert_eq!(nth_card(s, 2), c("QS"));
    }

    #[test]
    fn heuristic_passes_queen_and_high_hearts() {
        let hand = CardSet::parse_list("QS 2C 3C 4D 5D 6D AH KH 9S 2S 3H 4H 7C").unwrap();
        let p = heuristic_pass(hand);
        assert_eq!(p.len(), 3);
        assert!(p.contains(c("QS")));
        assert!(p.contains(c("AH")));
    }

    #[test]
    fn heuristic_follows_low_and_dumps_queen() {
        // Led 5H, we hold 9H and 3H: play the highest loser? 9H beats 5H, 3H loses.
        let v = view("3H 9H 2D", &["5H"], 0, 3);
        assert_eq!(heuristic_play(&v), c("3H"));
        // Void in hearts: dump the queen.
        let v = view("QS AD 2D", &["5H"], 0, 3);
        assert_eq!(heuristic_play(&v), c("QS"));
        // Forced to win with spades but holding the queen: do not play it.
        let v = view("QS AS 2D", &["2S"], 0, 3);
        assert_eq!(heuristic_play(&v), c("AS"));
    }

    #[test]
    fn heuristic_leads_low_and_avoids_high_spades() {
        let v = view("AS KS 8D 9C", &[], 0, 3);
        assert_eq!(heuristic_play(&v), c("8D"));
    }
}
