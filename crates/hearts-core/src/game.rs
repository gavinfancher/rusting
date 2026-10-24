//! Mutable state of one hand of Hearts.
//!
//! `Game` is `Copy` (about 130 bytes, no heap), which is what makes the
//! advisor cheap: to try every legal move on the same deal it just copies the
//! struct instead of cloning vectors. Bots never see a `Game`; they only get a
//! [`PlayerView`] built by [`Game::view`], so they cannot cheat.

use crate::card::{Card, CardSet, Suit};
use crate::rules;
use crate::types::{PassDirection, PlayerView, Rules, Seat, Trick};

/// Result of the card that completed a trick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrickDone {
    pub winner: Seat,
    pub points: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct Game {
    /// Cards each seat still holds.
    pub hands: [CardSet; 4],
    pub trick: Trick,
    /// Cards in completed tricks.
    pub played: CardSet,
    /// Penalty points taken so far per seat (before moon adjustment).
    pub points: [u8; 4],
    /// Public knowledge: suits each seat has shown it is out of.
    pub voids: [u8; 4],
    pub hearts_broken: bool,
    pub tricks_played: u8,
    pub rules: Rules,
    pass_dir: PassDirection,
    /// Cards each seat passed away (for building views).
    passed: [CardSet; 4],
    /// Who took Q♠, once it has been played into a completed trick.
    pub queen_taker: Option<Seat>,
}

impl Game {
    /// A freshly dealt hand, before passing. The holder of 2♣ is set as leader
    /// (and is recomputed by [`Game::apply_passes`]).
    pub fn new(hands: [CardSet; 4], rules: Rules) -> Game {
        let mut g = Game {
            hands,
            trick: Trick::new(0),
            played: CardSet::EMPTY,
            points: [0; 4],
            voids: [0; 4],
            hearts_broken: false,
            tricks_played: 0,
            rules,
            pass_dir: PassDirection::Hold,
            passed: [CardSet::EMPTY; 4],
            queen_taker: None,
        };
        g.set_opening_leader();
        g
    }

    /// Rebuild a mid-hand state from what one player knows plus a guess
    /// (determinization) of everyone else's hands. `hands[view.seat]` is
    /// replaced by the player's real hand. Who took Q♠ earlier is unknown to
    /// a view, so `queen_taker` stays `None`; scoring only needs `points`.
    pub fn from_view(view: &PlayerView, mut hands: [CardSet; 4]) -> Game {
        hands[view.seat as usize] = view.hand;
        Game {
            hands,
            trick: view.trick,
            played: view.played,
            points: view.points,
            voids: view.voids,
            hearts_broken: view.hearts_broken,
            tricks_played: view.tricks_played,
            rules: view.rules,
            pass_dir: PassDirection::Hold,
            passed: [CardSet::EMPTY; 4],
            queen_taker: None,
        }
    }

    fn set_opening_leader(&mut self) {
        let leader = (0..4u8)
            .find(|&s| self.hands[s as usize].contains(Card::TWO_OF_CLUBS))
            .unwrap_or(0);
        self.trick = Trick::new(leader);
    }

    /// Exchange the three passed cards per seat simultaneously. `passes[s]`
    /// must be a 3-card subset of seat `s`'s hand. A `Hold` hand passes nothing.
    pub fn apply_passes(&mut self, dir: PassDirection, passes: [CardSet; 4]) {
        self.pass_dir = dir;
        for s in 0..4u8 {
            if dir.target(s).is_some() {
                self.hands[s as usize] = self.hands[s as usize] - passes[s as usize];
                self.passed[s as usize] = passes[s as usize];
            }
        }
        for s in 0..4u8 {
            if let Some(t) = dir.target(s) {
                self.hands[t as usize] = self.hands[t as usize] | passes[s as usize];
            }
        }
        self.set_opening_leader();
    }

    /// The information `seat` is allowed to see.
    #[inline]
    pub fn view(&self, seat: Seat) -> PlayerView {
        PlayerView {
            seat,
            hand: self.hands[seat as usize],
            trick: self.trick,
            played: self.played,
            voids: self.voids,
            hearts_broken: self.hearts_broken,
            tricks_played: self.tricks_played,
            points: self.points,
            passed: self
                .pass_dir
                .target(seat)
                .map(|t| (t, self.passed[seat as usize])),
            rules: self.rules,
        }
    }

    #[inline]
    pub fn to_play(&self) -> Seat {
        self.trick.to_play()
    }

    #[inline]
    pub fn is_over(&self) -> bool {
        self.tricks_played == 13
    }

    /// Play `card` for the seat whose turn it is. The caller guarantees the
    /// move is legal (bots are tested for that; `debug_assert` re-checks in
    /// debug builds). Returns trick info when this card completes a trick.
    #[inline]
    pub fn play(&mut self, card: Card) -> Option<TrickDone> {
        let seat = self.trick.to_play();
        debug_assert!(self.hands[seat as usize].contains(card));
        debug_assert!(rules::legal_moves(&self.view(seat)).contains(card));
        self.hands[seat as usize].remove(card);
        if let Some(led) = self.trick.led_suit()
            && card.suit() != led
        {
            // Failing to follow suit publicly reveals a void.
            self.voids[seat as usize] |= led.bit();
        }
        if card.suit() == Suit::Hearts {
            self.hearts_broken = true;
        }
        self.trick.push(card);
        if !self.trick.is_complete() {
            return None;
        }
        let winner = rules::trick_winner(&self.trick);
        let cards = self.trick.cards();
        let points = cards.points() as u8;
        self.points[winner as usize] += points;
        if cards.contains(Card::QUEEN_OF_SPADES) {
            self.queen_taker = Some(winner);
        }
        self.played = self.played | cards;
        self.tricks_played += 1;
        self.trick = Trick::new(winner);
        Some(TrickDone { winner, points })
    }

    /// Final per-seat scores (moon adjusted). Meaningful once `is_over()`.
    pub fn final_points(&self) -> [u8; 4] {
        rules::final_points(self.points)
    }
}

#[cfg(test)]
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn pass_moves_cards_and_sets_leader() {
        let hands = rules::deal(&mut Rng::new(5));
        let mut g = Game::new(hands, Rules::default());
        let passes: [CardSet; 4] = std::array::from_fn(|s| hands[s].iter().take(3).collect());
        g.apply_passes(PassDirection::Left, passes);
        for s in 0..4 {
            assert_eq!(g.hands[s].len(), 13);
            assert!(!(g.hands[(s + 1) % 4] & passes[s]).is_empty());
        }
        assert!(g.hands[g.to_play() as usize].contains(Card::TWO_OF_CLUBS));
        let v = g.view(0);
        assert_eq!(v.passed, Some((1, passes[0])));
    }
}
