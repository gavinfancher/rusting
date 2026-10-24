//! The learnable bot: a linear scoring function over hand-crafted features.
//!
//! For every candidate card we compute a small vector of integer *features*
//! (facts about the card in the current situation), multiply each feature by
//! a learned integer *weight* and add everything up:
//!
//! ```text
//! score(card) = w[0]*f[0] + w[1]*f[1] + ... + w[15]*f[15]      (play)
//! ```
//!
//! The card with the highest score is played. Positive weight on a feature
//! means "I like cards that have this property"; negative means "avoid".
//! Which features exist and what they are called is fixed by
//! [`FEATURE_NAMES`](crate::types::FEATURE_NAMES) (a project-wide contract);
//! this file decides their exact numeric scale.
//!
//! Everything is integer arithmetic (`i64` accumulators), so the result is
//! bit-for-bit identical on every CPU, and nothing allocates: this is the hot
//! loop of every training tournament.
//!
//! # Feature scales
//!
//! Features are scaled so that one learned weight of magnitude ~10 moves a
//! score by ~1000 and every feature spans roughly 0..~100-200. That keeps all
//! 24 weights on a comparable scale, so one CEM `sigma` suits them all.
//! A "boolean" is 0 or 100. A "rank" is `0..=12` (2 = 0 ... A = 12) times 8,
//! so 0..=96. A "count" is the count times 8. "Points" are penalty points
//! times 7 (Q♠ = 91, a heart = 7).
//!
//! ## Play features (index 0..16), evaluated for each legal card
//!
//! | # | name | value |
//! |---|------|-------|
//! | 0 | `play.leading` | 100 if we lead this trick, else 0. The same for every candidate, so it never changes the choice; it is kept because the name is part of the contract. |
//! | 1 | `play.rank` | rank (0..=12) x 8 |
//! | 2 | `play.is_heart` | 100 if the card is a heart |
//! | 3 | `play.is_queen_spades` | 100 if the card is Q♠ |
//! | 4 | `play.high_spade_queen_unseen` | 100 if the card is K♠ or A♠ and Q♠ is still unseen (not in our hand, not played, not on the table) |
//! | 5 | `play.wins_now` | 100 if the card currently wins the trick (it is the highest card of the led suit so far; any lead counts as winning) |
//! | 6 | `play.wins_x_points_on_table` | wins_now x penalty points already on the table x 7 |
//! | 7 | `play.wins_x_last_to_play` | 100 if wins_now and we are the 4th player (so the win is certain) |
//! | 8 | `play.wins_x_my_points` | wins_now x points we already took this hand x 7 |
//! | 9 | `play.dump_x_points` | if we are void in the led suit: this card's penalty points x 7, else 0 |
//! | 10 | `play.dump_x_rank` | if we are void in the led suit: rank x 8, else 0 |
//! | 11 | `play.makes_void` | 100 if playing it leaves no card of its suit in our hand |
//! | 12 | `play.suit_len_after` | cards of its suit we would still hold x 8 |
//! | 13 | `play.higher_unseen` | unseen cards of its suit that outrank it x 8 |
//! | 14 | `play.lower_unseen` | unseen cards of its suit below it x 8 |
//! | 15 | `play.lead_low_spade_fishing` | 100 if we lead a spade below Q while Q♠ is unseen |
//!
//! ("Void in the led suit" means: following a led suit and holding none of
//! it. That is the only time a card of another suit can be legal, apart from
//! leading.)
//!
//! ## Pass features (index 16..24), evaluated for each card in hand
//!
//! | # | name | value |
//! |---|------|-------|
//! | 16 | `pass.rank` | rank x 8 |
//! | 17 | `pass.heart_x_rank` | rank x 8 if the card is a heart, else 0 |
//! | 18 | `pass.is_queen_spades` | 100 if Q♠ |
//! | 19 | `pass.is_high_spade` | 100 if K♠ or A♠ |
//! | 20 | `pass.spades_guarded_x_high` | 100 if we hold 3+ spades below Q and the card is Q♠, K♠ or A♠ |
//! | 21 | `pass.short_side_suit` | clubs/diamonds only: (4 - suit length) x 8 if the suit has at most 3 cards in hand, else 0 |
//! | 22 | `pass.is_low` | 100 if the card is a 2, 3, 4 or 5 |
//! | 23 | `pass.side_suit_x_rank` | rank x 8 if the card is a club or diamond, else 0 |
//!
//! The 3 highest-scoring cards are passed.

use crate::card::{Card, CardSet, Suit};
use crate::rules::legal_moves;
use crate::types::{
    NUM_PASS_FEATURES, NUM_PLAY_FEATURES, NUM_WEIGHTS, PassDirection, PlayerView, Weights,
};

const B: i64 = 100; // a "boolean" feature
const RANK: i64 = 8; // per rank step
const PTS: i64 = 7; // per penalty point

/// The facts about the current trick that do not depend on which card we
/// consider. Computed once per decision, then reused for every candidate.
struct PlayContext {
    leading: bool,
    /// Led suit, if following.
    led: Option<Suit>,
    /// Rank of the highest led-suit card on the table (meaningless if leading).
    top_rank: u8,
    table_points: i64,
    last_to_play: bool,
    my_points: i64,
    /// We hold nothing of the led suit (always false when leading).
    void_in_led: bool,
    hand: CardSet,
    unseen: CardSet,
    queen_unseen: bool,
}

impl PlayContext {
    fn new(view: &PlayerView) -> PlayContext {
        let led = view.trick.led_suit();
        let mut top_rank = 0u8;
        let mut table_points = 0i64;
        for (_, c) in view.trick.iter() {
            if Some(c.suit()) == led && c.rank() >= top_rank {
                top_rank = c.rank();
            }
            table_points += c.points() as i64;
        }
        let unseen = view.unseen();
        PlayContext {
            leading: led.is_none(),
            led,
            top_rank,
            table_points,
            last_to_play: view.trick.len() == 3,
            my_points: view.points[view.seat as usize] as i64,
            void_in_led: led.is_some_and(|s| !view.hand.has_suit(s)),
            hand: view.hand,
            unseen,
            queen_unseen: unseen.contains(Card::QUEEN_OF_SPADES),
        }
    }

    /// Would `c` currently win the trick? (Leading counts as winning.)
    #[inline]
    fn wins(&self, c: Card) -> bool {
        match self.led {
            None => true,
            Some(led) => c.suit() == led && c.rank() > self.top_rank,
        }
    }
}

/// The 16 play features of card `c`, in `FEATURE_NAMES` order.
#[inline]
fn play_features(cx: &PlayContext, c: Card) -> [i64; NUM_PLAY_FEATURES] {
    let suit = c.suit();
    let rank = c.rank() as i64;
    let wins = cx.wins(c);
    let wins_b = if wins { B } else { 0 };
    let in_suit = cx.hand.of_suit(suit);

    // Unseen cards of this suit above / below `c`. Cards are numbered
    // suit*13 + rank, so "same suit, higher rank" is "higher bit, same mask".
    let unseen_suit = (cx.unseen & suit.mask()).0;
    let idx = c.index() as u32;
    let higher = (unseen_suit & !((2u64 << idx) - 1)).count_ones() as i64;
    let lower = (unseen_suit & ((1u64 << idx) - 1)).count_ones() as i64;

    let is_high_spade = suit == Suit::Spades && c.rank() >= 11;
    let is_queen = c == Card::QUEEN_OF_SPADES;

    [
        if cx.leading { B } else { 0 },
        rank * RANK,
        if suit == Suit::Hearts { B } else { 0 },
        if is_queen { B } else { 0 },
        if is_high_spade && cx.queen_unseen {
            B
        } else {
            0
        },
        wins_b,
        if wins { cx.table_points * PTS } else { 0 },
        if wins && cx.last_to_play { B } else { 0 },
        if wins { cx.my_points * PTS } else { 0 },
        if cx.void_in_led {
            c.points() as i64 * PTS
        } else {
            0
        },
        if cx.void_in_led { rank * RANK } else { 0 },
        if in_suit.len() == 1 { B } else { 0 },
        (in_suit.len() as i64 - 1) * RANK,
        higher * RANK,
        lower * RANK,
        if cx.leading && suit == Suit::Spades && c.rank() < 10 && cx.queen_unseen {
            B
        } else {
            0
        },
    ]
}

/// Σ weight × feature over the play half of the weight vector.
#[inline]
fn play_score(w: &Weights, cx: &PlayContext, c: Card) -> i64 {
    let f = play_features(cx, c);
    dot(&w.0[..NUM_PLAY_FEATURES], &f)
}

/// Σ weight × feature, accumulated in i64 (exact; weights are clamped to
/// ±1,000,000 by the trainer and features are at most a few hundred).
#[inline]
fn dot(weights: &[i32], features: &[i64]) -> i64 {
    weights
        .iter()
        .zip(features)
        .map(|(&w, &f)| w as i64 * f)
        .sum()
}

/// Pick a card to play from `legal_moves(view)`. Highest score wins; ties go
/// to the lowest card index (we iterate in ascending order and only replace
/// on a strictly greater score).
pub fn linear_play(w: &Weights, view: &PlayerView) -> Card {
    let legal = legal_moves(view);
    if legal.len() == 1 {
        return legal.lowest().unwrap_or(Card::TWO_OF_CLUBS);
    }
    let cx = PlayContext::new(view);
    let mut best = Card::TWO_OF_CLUBS;
    let mut best_score = i64::MIN;
    for c in legal {
        let s = play_score(w, &cx, c);
        if s > best_score {
            best_score = s;
            best = c;
        }
    }
    best
}

/// The 8 pass features of card `c` in `hand`, in `FEATURE_NAMES` order
/// (starting at index 16).
#[inline]
fn pass_features(hand: CardSet, guarded: bool, c: Card) -> [i64; NUM_PASS_FEATURES] {
    let suit = c.suit();
    let rank = c.rank() as i64;
    let is_queen = c == Card::QUEEN_OF_SPADES;
    let is_high_spade = suit == Suit::Spades && c.rank() >= 11;
    let side = suit == Suit::Clubs || suit == Suit::Diamonds;
    let len = hand.of_suit(suit).len() as i64;
    [
        rank * RANK,
        if suit == Suit::Hearts { rank * RANK } else { 0 },
        if is_queen { B } else { 0 },
        if is_high_spade { B } else { 0 },
        if guarded && (is_queen || is_high_spade) {
            B
        } else {
            0
        },
        if side && len <= 3 {
            (4 - len) * RANK
        } else {
            0
        },
        if c.rank() <= 3 { B } else { 0 },
        if side { rank * RANK } else { 0 },
    ]
}

/// Pick 3 cards from `hand` to pass: the three highest-scoring cards.
/// `dir` is accepted for API symmetry (a future version may pass differently
/// to an opponent on the left than across) but is not used by Level 1.
pub fn linear_pass(w: &Weights, hand: CardSet, _dir: PassDirection) -> CardSet {
    // "Guarded": 3+ spades below the queen, so we can duck spade leads.
    let below_queen = CardSet(hand.of_suit(Suit::Spades).0 & (Card::QUEEN_OF_SPADES.bit().0 - 1));
    let guarded = below_queen.len() >= 3;

    let mut scored = [(i64::MIN, 0u8); 13];
    let mut n = 0;
    for c in hand {
        let f = pass_features(hand, guarded, c);
        let s = dot(&w.0[NUM_PLAY_FEATURES..], &f);
        scored[n] = (s, c.index());
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

/// Hand-set starting weights that play about as well as `Heuristic`
/// (measured: slightly better, see `docs/LEARNING.md`). Training's
/// `init: "heuristic"` starts here.
///
/// How they were chosen: the *signs* come from reading `heuristic_play`
/// (avoid winning tricks, never play Q♠ voluntarily, dump point cards when
/// void, avoid leading hearts); the *magnitudes* were then adjusted with a
/// short coordinate search on the play weights (nudge one weight, keep the
/// change if 100k hands against 3 heuristics got better). The pass weights
/// are scaled by hand from `heuristic_pass`. One limitation is visible in the
/// numbers: a single `play.rank` weight must serve both "lead low" and
/// "follow suit", so it can't copy the heuristic's "play the highest loser"
/// rule. Learning can still find compensations; see the doc.
pub fn default_weights() -> Weights {
    Weights(DEFAULT)
}

#[rustfmt::skip]
const DEFAULT: [i32; NUM_WEIGHTS] = [
    // play
    0, -5, -9, -30, -20, -30, 0, 0, 0, 150, 35, 12, -28, 12, 23, 22,
    // pass
    1, 6, 8, 7, -8, 6, 0, 4,
];

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::{StrategyKind, Trick};

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
    fn weight_layout_matches_feature_count() {
        assert_eq!(NUM_PLAY_FEATURES + NUM_PASS_FEATURES, NUM_WEIGHTS);
        assert_eq!(DEFAULT.len(), crate::types::FEATURE_NAMES.len());
    }

    #[test]
    fn default_weights_round_trip_through_text() {
        let kind = StrategyKind::Linear(default_weights());
        let text = kind.to_string();
        assert!(text.starts_with("linear:"));
        assert_eq!(text.parse::<StrategyKind>().unwrap(), kind);
    }

    #[test]
    fn features_have_documented_values() {
        // Hearts led (5H), we hold the Q♠ and an ace, and are void in hearts.
        let v = view("QS AD 2D", &["5H"], 0, 3);
        let cx = PlayContext::new(&v);
        let f = play_features(&cx, c("QS"));
        assert_eq!(f[0], 0); // not leading
        assert_eq!(f[1], 10 * 8); // queen is rank index 10
        assert_eq!(f[3], 100); // is Q♠
        assert_eq!(f[5], 0); // different suit, cannot win
        assert_eq!(f[9], 13 * 7); // void in led suit x 13 points
        assert_eq!(f[10], 10 * 8);
        // Following suit with a winner: 9H beats 5H, table has 1 point.
        let v = view("9H 3H 2D", &["5H"], 0, 3);
        let cx = PlayContext::new(&v);
        let f9 = play_features(&cx, c("9H"));
        assert_eq!(f9[5], 100);
        assert_eq!(f9[6], 7);
        assert_eq!(f9[2], 100);
        assert_eq!(f9[12], 8); // 3H remains
        assert_eq!(play_features(&cx, c("3H"))[5], 0);
        assert_eq!(play_features(&cx, c("3H"))[11], 0);
    }

    #[test]
    fn ties_go_to_the_lowest_card_index() {
        // All-zero weights: every card scores 0, so the lowest index wins.
        let w = Weights([0; NUM_WEIGHTS]);
        let v = view("9D QS 3H", &[], 0, 3);
        assert_eq!(linear_play(&w, &v), c("9D"));
        let hand = CardSet::parse_list("2C 3C 4C 5C 6C 7C 8C 9C TC JC QC KC AC").unwrap();
        assert_eq!(
            linear_pass(&w, hand, PassDirection::Left),
            CardSet::parse_list("2C 3C 4C").unwrap()
        );
    }

    #[test]
    fn default_bot_dumps_queen_when_void_and_ducks_otherwise() {
        let w = default_weights();
        // Void in hearts: dump the queen.
        let v = view("QS AD 2D", &["5H"], 0, 3);
        assert_eq!(linear_play(&w, &v), c("QS"));
        // Can follow with a loser: do not take the trick with the 9.
        let v = view("3H 9H 2D", &["5H"], 0, 3);
        assert_eq!(linear_play(&w, &v), c("3H"));
    }

    #[test]
    fn default_pass_gives_away_queen_and_high_hearts() {
        let hand = CardSet::parse_list("QS 2C 3C 4D 5D 6D AH KH 9S 2S 3H 4H 7C").unwrap();
        let p = linear_pass(&default_weights(), hand, PassDirection::Left);
        assert_eq!(p.len(), 3);
        assert!(p.contains(c("QS")));
        assert!(p.contains(c("AH")));
        assert!((p - hand).is_empty());
    }
}
